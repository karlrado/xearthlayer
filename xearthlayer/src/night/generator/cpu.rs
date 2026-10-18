//! The CPU lit generator.
//!
//! Flight-test algorithm: the class comes from the nearest recipe cell and
//! the pattern is tiled straight, anchored to world position per 1 degree
//! tile.

use std::collections::HashMap;
use std::sync::Arc;

use image::{Rgba, RgbaImage};

use super::{gain, LitGenerator, LitRequest};
use crate::night::footprint::{sample, TILES_PER_TEXTURE};
use crate::night::laminar::LitPattern;
use crate::night::recipe::Recipe;

/// Metres of ground per degree of latitude.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// Generates lit pixels on the CPU.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuLitGenerator;

impl LitGenerator for CpuLitGenerator {
    fn generate(&self, request: &LitRequest<'_>) -> RgbaImage {
        let (name, size, brightness) = (request.name, request.size, request.brightness);
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

        // Most texels share their neighbour's tile and class: remember the
        // current tile's recipe, its patterns by palette index, and its metres
        // per degree of longitude, so the per-texel work is arithmetic.
        let mut current: Option<(i32, i32)> = None;
        let mut recipe: Option<Arc<Recipe>> = None;
        let mut by_index: Vec<Option<Option<Arc<LitPattern>>>> = Vec::new();
        let mut metres_per_lon_degree = 0.0;

        for (i, &lat) in lats.iter().enumerate() {
            let lat0 = lat.floor() as i32;
            let top = f64::from(lat0) + 1.0;
            for (j, &lon) in lons.iter().enumerate() {
                let lon0 = lon.floor() as i32;
                if current != Some((lat0, lon0)) {
                    current = Some((lat0, lon0));
                    recipe = recipes
                        .entry((lat0, lon0))
                        .or_insert_with(|| (request.recipes)(lat0, lon0))
                        .clone();
                    by_index = vec![None; recipe.as_ref().map_or(0, |r| r.palette.len())];
                    metres_per_lon_degree =
                        METRES_PER_DEGREE * (f64::from(lat0) + 0.5).to_radians().cos();
                }
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
                let pattern = by_index[index].get_or_insert_with(|| {
                    let class_name = &r.palette[index];
                    patterns
                        .entry(class_name.clone())
                        .or_insert_with(|| (request.patterns)(class_name))
                        .clone()
                });
                let Some(p) = pattern.as_deref() else {
                    continue;
                };
                // World-anchored ground metres from the tile's south-west corner.
                let metres_x = dx * metres_per_lon_degree;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{DdsFilename, TextureKind};
    use crate::night::parse_recipe;

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
        // The other urban block sits under radiance 0.
        let img = generate(&name_at(37.875, -122.85), 32, golden, red, 1.0);
        assert_eq!(lit_texels(&img), 0);
    }

    #[test]
    fn generation_is_deterministic() {
        let a = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        let b = generate(&name_at(37.20, -122.725), 64, golden, red, 1.0);
        assert_eq!(a.as_raw(), b.as_raw());
    }
}
