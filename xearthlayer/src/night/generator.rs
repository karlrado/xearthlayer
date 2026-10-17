//! The lit generator: the first visible level of a lit texture, as pixels.
//!
//! Flight-test version (phase 2b slice): the class comes from the nearest
//! recipe cell and the pattern is tiled straight, anchored to world position
//! per 1 degree tile. Stochastic tiling and softened class edges follow in
//! the full phase 2b (design: LitGenerator).

use std::collections::HashMap;
use std::sync::Arc;

use image::{Rgba, RgbaImage};

use super::footprint::{sample, TILES_PER_TEXTURE};
use super::laminar::LitPattern;
use super::recipe::Recipe;
use crate::fuse::DdsFilename;

/// Radiance (nW/cm2/sr) at and below which the gain is 0. Provisional,
/// set on the tuning flight.
pub const GAIN_FLOOR_NW: f64 = 2.0;

/// Radiance at and above which the gain is 1. Provisional.
pub const GAIN_CEILING_NW: f64 = 150.0;

/// Gamma applied to the gain ramp. Provisional.
pub const GAIN_GAMMA: f64 = 1.0;

/// Metres of ground per degree of latitude.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// Light multiplier for a radiance: a linear ramp in `ln(R)` from the floor
/// (0) to the ceiling (1), raised to the gamma, times `brightness`, clamped
/// to 0..1 (R10).
pub fn gain(radiance_nw: f64, brightness: f64) -> f64 {
    if radiance_nw.is_nan() || radiance_nw <= GAIN_FLOOR_NW {
        return 0.0;
    }
    let t = (radiance_nw.ln() - GAIN_FLOOR_NW.ln()) / (GAIN_CEILING_NW.ln() - GAIN_FLOOR_NW.ln());
    (t.clamp(0.0, 1.0).powf(GAIN_GAMMA) * brightness).clamp(0.0, 1.0)
}

/// The first visible level (`size` x `size`, north up) of the lit texture
/// `name`, black wherever nothing is lit.
pub fn generate_visible(
    name: &DdsFilename,
    size: u32,
    recipe_for: impl Fn(i32, i32) -> Option<Arc<Recipe>>,
    pattern_for: impl Fn(&str) -> Option<Arc<LitPattern>>,
    brightness: f64,
) -> RgbaImage {
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
            (std::f64::consts::PI * (1.0 - 2.0 * y / n))
                .sinh()
                .atan()
                .to_degrees()
        })
        .collect();

    let mut recipes: HashMap<(i32, i32), Option<Arc<Recipe>>> = HashMap::new();
    let mut patterns: HashMap<String, Option<Arc<LitPattern>>> = HashMap::new();
    let mut img = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 255]));

    for (i, &lat) in lats.iter().enumerate() {
        let lat0 = lat.floor() as i32;
        let top = f64::from(lat0) + 1.0;
        for (j, &lon) in lons.iter().enumerate() {
            let lon0 = lon.floor() as i32;
            let recipe = recipes
                .entry((lat0, lon0))
                .or_insert_with(|| recipe_for(lat0, lon0));
            let Some(r) = recipe.as_deref() else {
                continue;
            };
            let (dy, dx) = (top - lat, lon - f64::from(lon0));
            let index = usize::from(sample(&r.class, dy, dx));
            if index == 0 {
                continue;
            }
            let g = gain(r.scale.radiance(sample(&r.brightness, dy, dx)), brightness);
            if g <= 0.0 {
                continue;
            }
            let class_name = &r.palette[index];
            let pattern = patterns
                .entry(class_name.clone())
                .or_insert_with(|| pattern_for(class_name));
            let Some(p) = pattern.as_deref() else {
                continue;
            };
            // World-anchored ground metres from the tile's south-west corner.
            let metres_x = dx * METRES_PER_DEGREE * (f64::from(lat0) + 0.5).to_radians().cos();
            let metres_y = dy * METRES_PER_DEGREE;
            let c = p.sample(metres_x / p.ground_scale_m, metres_y / p.ground_scale_m);
            let scale = g as f32;
            img.put_pixel(
                j as u32,
                i as u32,
                Rgba([
                    (c[0] * scale).round() as u8,
                    (c[1] * scale).round() as u8,
                    (c[2] * scale).round() as u8,
                    255,
                ]),
            );
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;
    use crate::night::parse_recipe;

    const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");

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

    fn lit_texels(img: &RgbaImage) -> usize {
        img.pixels()
            .filter(|p| p[0] > 0 || p[1] > 0 || p[2] > 0)
            .count()
    }

    #[test]
    fn gain_ramps_from_floor_to_ceiling() {
        assert_eq!(gain(0.0, 1.0), 0.0);
        assert_eq!(gain(GAIN_FLOOR_NW, 1.0), 0.0);
        assert_eq!(gain(GAIN_CEILING_NW, 1.0), 1.0);
        assert_eq!(gain(1_000.0, 1.0), 1.0);
        let mid = gain((GAIN_FLOOR_NW * GAIN_CEILING_NW).sqrt(), 1.0);
        assert!((mid - 0.5).abs() < 1e-9, "log-midpoint is 0.5, got {mid}");
        assert!((gain((GAIN_FLOOR_NW * GAIN_CEILING_NW).sqrt(), 0.5) - 0.25).abs() < 1e-9);
        assert_eq!(gain(1_000.0, 1.5), 1.0, "clamped");
    }

    #[test]
    fn a_lit_block_lights_only_its_cells() {
        // The golden urban block (lat 37.15..37.25, lon -122.75..-122.70)
        // sits under radiance 230 (gain 1 with the provisional curve).
        let img = generate_visible(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(img.dimensions(), (64, 64));
        let lit = lit_texels(&img);
        assert!(lit > 0, "the block lights");
        assert!(lit < 64 * 64, "cells outside the block stay dark");
        assert!(img.pixels().all(|p| p[1] == 0 && p[2] == 0 && p[3] == 255));
    }

    #[test]
    fn countryside_and_unlit_classes_are_black() {
        let img = generate_visible(&name_at(37.60, -122.20), 32, golden, red, 1.0);
        assert_eq!(lit_texels(&img), 0);
        let img = generate_visible(&name_at(37.20, -122.725), 32, golden, |_| None, 1.0);
        assert_eq!(lit_texels(&img), 0, "no pattern resolves");
    }

    #[test]
    fn zero_radiance_is_black() {
        // The other urban block sits under radiance 0.
        let img = generate_visible(&name_at(37.875, -122.85), 32, golden, red, 1.0);
        assert_eq!(lit_texels(&img), 0);
    }

    #[test]
    fn generation_is_deterministic() {
        let a = generate_visible(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        let b = generate_visible(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(a.as_raw(), b.as_raw());
    }
}
