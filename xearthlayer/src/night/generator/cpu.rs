//! The CPU lit generator.
//!
//! Each texel of the first visible level is a function of its ground
//! position alone: the recipe field gives the class weights (soft, warped
//! class edges) and the radiance (bilinear brightness), the radiance becomes
//! a gain, and the class patterns are sampled by stochastic tiling in Web
//! Mercator metres. Nothing depends on the texture's zoom or on 1 degree
//! tile borders, so neighbouring and nested textures agree on shared ground.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;

use image::{Rgba, RgbaImage};

use super::field::RecipeField;
use super::tiling::{mercator, sample};
use super::{gain, LitGenerator, LitRequest};
use crate::night::footprint::{Footprint, TILES_PER_TEXTURE};
use crate::night::laminar::LitPattern;

/// Generates lit pixels on the CPU.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuLitGenerator;

impl LitGenerator for CpuLitGenerator {
    fn generate(&self, request: &LitRequest<'_>) -> RgbaImage {
        let name = request.name;
        let size = request.size;
        let n = f64::from(1u32 << name.zoom);
        let side = f64::from(TILES_PER_TEXTURE);
        let px = f64::from(size);
        // Texel centres: longitudes per column, latitudes per row (north first).
        let lons: Vec<f64> = (0..size)
            .map(|j| (f64::from(name.col) + (f64::from(j) + 0.5) / px * side) / n * 360.0 - 180.0)
            .collect();
        let lats: Vec<f64> = (0..size)
            .map(|i| {
                let y = f64::from(name.row) + (f64::from(i) + 0.5) / px * side;
                (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees()
            })
            .collect();

        let field = RecipeField::new(&Footprint::from_name(name), request.recipes);
        let mut patterns: HashMap<String, Option<Arc<LitPattern>>> = HashMap::new();
        let mut weights = Vec::with_capacity(4);
        let mut img = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 255]));

        for (i, &lat) in lats.iter().enumerate() {
            for (j, &lon) in lons.iter().enumerate() {
                field.class_weights(lat, lon, &mut weights);
                if weights.is_empty() {
                    continue;
                }
                let g = gain(field.radiance(lat, lon), request.brightness);
                if g <= 0.0 {
                    continue;
                }
                let (x, y) = mercator(lat, lon);
                let mut rgb = [0.0f32; 3];
                for w in &weights {
                    let Some(class) = field.palette_name(w.tile, w.index) else {
                        continue;
                    };
                    if !patterns.contains_key(class) {
                        patterns.insert(class.to_string(), (request.patterns)(class));
                    }
                    let Some(p) = &patterns[class] else {
                        continue;
                    };
                    let c = sample(p, x, y);
                    for ch in 0..3 {
                        rgb[ch] += w.weight as f32 * c[ch];
                    }
                }
                let s = g as f32;
                let byte = |v: f32| (v * s).round().clamp(0.0, 255.0) as u8;
                img.put_pixel(
                    j as u32,
                    i as u32,
                    Rgba([byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), 255]),
                );
            }
        }
        img
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{DdsFilename, TextureKind};
    use crate::night::generator::field::tests_support::synthetic;
    use crate::night::generator::noise::{hash2, unit};
    use crate::night::parse_recipe;
    use crate::night::recipe::Recipe;

    const URBAN: &[u8] = include_bytes!("../../../tests/fixtures/night/urban.oxn");

    fn name_at(lat: f64, lon: f64) -> DdsFilename {
        let n = f64::from(1u32 << 16);
        let x = ((lon + 180.0) / 360.0 * n) as u32;
        let lat_r = lat.to_radians();
        let y = ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
            as u32;
        DdsFilename {
            row: y / 16 * 16,
            col: x / 16 * 16,
            zoom: 16,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    fn golden(lat: i32, lon: i32) -> Option<Arc<Recipe>> {
        ((lat, lon) == (37, -123)).then(|| Arc::new(parse_recipe(URBAN).unwrap()))
    }

    fn red(name: &str) -> Option<Arc<LitPattern>> {
        (name == "lib/g10/terrain10/urban_tmp_sdry3.ter").then(|| {
            Arc::new(LitPattern {
                size: 4,
                rgb: vec![[255, 0, 0]; 16],
                ground_scale_m: 4096.0,
            })
        })
    }

    fn generate(
        name: &DdsFilename,
        size: u32,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        patterns: impl Fn(&str) -> Option<Arc<LitPattern>> + Sync,
        brightness: f64,
    ) -> RgbaImage {
        CpuLitGenerator.generate(&LitRequest {
            name,
            size,
            brightness,
            recipes: &recipes,
            patterns: &patterns,
        })
    }

    fn lit_texels(img: &RgbaImage) -> usize {
        img.pixels()
            .filter(|p| p[0] > 0 || p[1] > 0 || p[2] > 0)
            .count()
    }

    #[test]
    fn a_lit_block_lights_only_its_cells() {
        // The golden urban block (lat 37.15..37.25, lon -122.75..-122.70)
        // sits under radiance 230 (gain 1 with the provisional curve).
        let img = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(img.dimensions(), (64, 64));
        let lit = lit_texels(&img);
        assert!(lit > 0, "the block lights");
        assert!(lit < 64 * 64, "cells outside the block stay dark");
        assert!(img.pixels().all(|p| p[1] == 0 && p[2] == 0 && p[3] == 255));
    }

    #[test]
    fn countryside_and_unlit_classes_are_black() {
        let img = generate(&name_at(37.60, -122.20), 32, golden, red, 1.0);
        assert_eq!(lit_texels(&img), 0);
        let img = generate(&name_at(37.20, -122.725), 32, golden, |_| None, 1.0);
        assert_eq!(lit_texels(&img), 0, "no pattern resolves");
    }

    #[test]
    fn zero_radiance_is_black() {
        let dark = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| dark.clone());
        let img = generate(&name_at(37.5, -122.5), 32, recipes, white, 1.0);
        assert_eq!(lit_texels(&img), 0);
    }

    /// A white 4 x 4 pattern: stochastic tiling of a solid colour is that colour,
    /// so a texel's value is its class weight times the gain times 255.
    fn white(_: &str) -> Option<Arc<LitPattern>> {
        Some(Arc::new(LitPattern {
            size: 4,
            rgb: vec![[255, 255, 255]; 16],
            ground_scale_m: 4096.0,
        }))
    }

    fn speckled(class: &str) -> Option<Arc<LitPattern>> {
        (class == "lib/g10/terrain10/urban_tmp_sdry3.ter").then(|| {
            let rgb = (0..32 * 32)
                .map(|i| {
                    let h = hash2(i, 0, 99);
                    [(unit(h) * 255.0) as u8, (unit(h >> 8) * 255.0) as u8, 0]
                })
                .collect();
            Arc::new(LitPattern {
                size: 32,
                rgb,
                ground_scale_m: 4096.0,
            })
        })
    }

    fn name(row: u32, col: u32, zoom: u8) -> DdsFilename {
        DdsFilename {
            row,
            col,
            zoom,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    #[test]
    fn the_same_ground_gives_the_same_light_at_any_zoom() {
        // A ZL15 texture at 2N px contains the ZL16 texture at N px as one of
        // its quarters; their texel centres coincide, so their pixels match.
        let z16 = name_at(37.20, -122.725);
        let z15 = name(z16.row / 32 * 16, z16.col / 32 * 16, 15);
        let big = generate(&z15, 128, golden, speckled, 1.0);
        let small = generate(&z16, 64, golden, speckled, 1.0);
        assert!(
            lit_texels(&small) > 64 * 64 / 20,
            "the compared ground holds the lit block: {}",
            lit_texels(&small)
        );
        let x0 = (z16.col - 2 * z15.col) / 16 * 64;
        let y0 = (z16.row - 2 * z15.row) / 16 * 64;
        let mut worst = 0i32;
        for y in 0..64 {
            for x in 0..64 {
                let (a, b) = (big.get_pixel(x0 + x, y0 + y), small.get_pixel(x, y));
                for ch in 0..3 {
                    worst = worst.max((i32::from(a[ch]) - i32::from(b[ch])).abs());
                }
            }
        }
        assert!(worst <= 2, "largest difference {worst}");
    }

    #[test]
    fn no_brightness_step_at_a_degree_line() {
        // Texture col 10544 at ZL16 spans lon -122.080..-121.992, across -122.
        // The west tile is brighter than the east one. Brightness 100 is
        // radiance 8 (gain 0.36); nearest-cell brightness would step from
        // full light to that at the line, bilinear brightness ramps.
        let west = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 255);
        let east = synthetic(37, -122, 120, |_, _| 1, 4, |_, _| 100);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        // Row 25392 spans lat 37.440..37.510.
        let img = generate(&name(25392, 10544, 16), 64, recipes, white, 1.0);
        let (mut lo, mut hi, mut step) = (255u8, 0u8, 0i32);
        for y in 0..64 {
            for x in 0..64 {
                let v = img.get_pixel(x, y)[0];
                lo = lo.min(v);
                hi = hi.max(v);
                if x < 63 {
                    let next = img.get_pixel(x + 1, y)[0];
                    step = step.max((i32::from(next) - i32::from(v)).abs());
                }
            }
        }
        assert!(hi - lo >= 10, "both brightnesses show: {lo}..{hi}");
        assert!(step <= 3, "largest step between neighbours {step}");
    }

    #[test]
    fn class_edges_are_soft() {
        // West half of tile +37-123 lit; a ZL14 texture straddling lon -122.5.
        let half = synthetic(37, -123, 120, |_, c| u8::from(c < 60), 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| half.clone());
        // ZL14 texture 6336_2608 spans lat 37.440..37.719, lon -122.695..-122.344.
        let img = generate(&name(6336, 2608, 14), 256, recipes, white, 1.0);
        let partial = img.pixels().filter(|p| p[0] > 12 && p[0] < 243).count();
        assert!(
            partial > 256,
            "a ramp along the edge, got {partial} partial texels"
        );
    }

    /// Release-build timing on the developer's installation; skipped elsewhere.
    #[test]
    #[ignore]
    fn time_a_real_zl16_texture() {
        let pkg = "/media/FlightSim/XEarthLayer Packages/zzXEL_na-usa-california-night_ortho";
        let art = "/media/FlightSim/X-Plane 12-beta/Resources/default scenery/1000 world terrain";
        if !std::path::Path::new(pkg).is_dir() || !std::path::Path::new(art).is_dir() {
            return;
        }
        let store = crate::night::RecipeStore::new(vec![pkg.into()]);
        let art = crate::night::LaminarArt::new(art.into());
        let n = name(25360, 10464, 16);
        let started = std::time::Instant::now();
        let img = generate(&n, 1024, |a, b| store.get(a, b), |c| art.pattern(c), 1.0);
        println!("ZL16 1024 px: {} ms", started.elapsed().as_millis());
        assert!(img.pixels().any(|p| p[0] > 0));
    }

    /// What the service answers for a lit `stat` of `name`.
    fn probe(
        name: &DdsFilename,
        recipes: &(dyn Fn(i32, i32) -> Option<Arc<Recipe>> + Sync),
    ) -> bool {
        RecipeField::new(&Footprint::from_name(name), recipes).has_light(|_| true)
    }

    /// Names at `zoom` whose footprints cover lat `lat0..lat1`, lon `lon0..lon1`.
    fn names_over(zoom: u8, lat0: f64, lat1: f64, lon0: f64, lon1: f64) -> Vec<DdsFilename> {
        let n = f64::from(1u32 << zoom);
        let col = |lon: f64| ((lon + 180.0) / 360.0 * n) as u32 / 16 * 16;
        let row = |lat: f64| {
            let r = f64::to_radians(lat);
            ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / PI) / 2.0 * n) as u32 / 16 * 16
        };
        let mut out = Vec::new();
        for r in (row(lat1)..=row(lat0)).step_by(16) {
            for c in (col(lon0)..=col(lon1)).step_by(16) {
                out.push(name(r, c, zoom));
            }
        }
        out
    }

    /// The probe contract: wherever the generator lights a texel of a
    /// footprint, the probe answers lit for it (over-approximating is fine,
    /// it costs only a full-size black file).
    fn assert_probe_covers(
        label: &str,
        recipes: impl Fn(i32, i32) -> Option<Arc<Recipe>> + Sync,
        window: (f64, f64, f64, f64),
    ) {
        let (lat0, lat1, lon0, lon1) = window;
        let (mut lit, mut dark, mut missed) = (0, 0, Vec::new());
        for zoom in [14u8, 16, 17, 18] {
            for n in names_over(zoom, lat0, lat1, lon0, lon1) {
                let generated = lit_texels(&generate(&n, 64, &recipes, white, 1.0)) > 0;
                let probed = probe(&n, &recipes);
                lit += usize::from(generated);
                dark += usize::from(!probed);
                if generated && !probed {
                    missed.push(format!("{}_{}_ZL{}", n.row, n.col, n.zoom));
                }
            }
        }
        assert!(lit > 0, "{label}: the sweep holds lit footprints");
        assert!(
            dark > 0,
            "{label}: the probe still answers some footprints dark"
        );
        assert!(
            missed.is_empty(),
            "{label}: lit by the generator, dark by the probe: {missed:?}"
        );
    }

    #[test]
    fn the_probe_covers_light_spilling_off_a_lit_block() {
        // Lit class west of lon -122.5, under brightness 0; the brightness
        // cell east of it is 200, so bilinear brightness lights the block's
        // east strip although no lit cell sits on a bright cell.
        let r = synthetic(
            37,
            -123,
            120,
            |_, c| u8::from(c < 60),
            4,
            |_, c| if c >= 2 { 200 } else { 0 },
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        assert_probe_covers("block edge", recipes, (37.45, 37.55, -122.62, -122.38));
    }

    #[test]
    fn the_probe_covers_light_across_a_degree_line() {
        // West tile: lit class, no radiance. East tile: no class, bright.
        let west = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let east = synthetic(37, -122, 120, |_, _| 0, 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        assert_probe_covers("longitude line", recipes, (37.45, 37.55, -122.3, -121.9));
    }

    #[test]
    fn the_probe_covers_light_across_a_latitude_line() {
        // South tile: lit class, no radiance. North tile: no class, bright.
        let south = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 0);
        let north = synthetic(38, -123, 120, |_, _| 0, 4, |_, _| 255);
        let recipes = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(south.clone()),
            (38, -123) => Some(north.clone()),
            _ => None,
        };
        assert_probe_covers("latitude line", recipes, (37.8, 38.1, -122.55, -122.45));
    }

    #[test]
    fn the_probe_covers_light_on_real_sized_grids() {
        // 1200 class and 240 brightness cells per degree, as OXO writes them.
        // A lit block (rows 600..660, cols 600..660) under zero brightness,
        // with one bright brightness cell just past each of its corners.
        let r = synthetic(
            37,
            -123,
            1200,
            |r, c| u8::from((600..660).contains(&r) && (600..660).contains(&c)),
            240,
            |r, c| {
                if (r == 119 || r == 132) && (c == 119 || c == 132) {
                    255
                } else {
                    0
                }
            },
        );
        let recipes = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        assert_probe_covers("real grids", recipes, (37.40, 37.55, -122.55, -122.40));
    }

    #[test]
    fn generation_is_deterministic() {
        let a = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        let b = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(a.as_raw(), b.as_raw());
    }
}
