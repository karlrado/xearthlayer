//! Recipe layers sampled as continuous functions of world position.
//!
//! Cells are indexed globally (row 0 at the north pole, column 0 at 180 W),
//! so a sample near a 1 degree edge reads the neighbouring tile's cells and
//! every texture computes the same value for the same ground.

use std::collections::HashMap;
use std::sync::Arc;

use super::noise::value_noise;
use super::RecipeLookup;
use crate::night::footprint::Footprint;
use crate::night::recipe::{BrightnessScale, Grid, Recipe};

/// How far beyond a footprint sampling can reach, in degrees: bilinear
/// neighbours and the class warp stay well inside it.
pub const FIELD_MARGIN_DEG: f64 = 0.01;

/// A recipe layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Class,
    Brightness,
}

/// The recipes one footprint (plus margin) touches, fetched once.
///
/// Grid sizes and the brightness scale come from one recipe (the one with
/// the smallest tile key, so the choice is deterministic) and apply to the
/// whole field: version 1 recipes all share them, and a tile whose grid
/// differs reads as missing. Taking them from the field rather than from the
/// tile containing a point keeps a sample next to a missing tile correct.
pub struct RecipeField {
    tiles: HashMap<(i32, i32), Arc<Recipe>>,
    class_dims: Option<(usize, usize)>,
    brightness_dims: Option<(usize, usize)>,
    scale: BrightnessScale,
}

impl RecipeField {
    pub fn new(footprint: &Footprint, lookup: RecipeLookup<'_>) -> Self {
        let tiles: HashMap<(i32, i32), Arc<Recipe>> = footprint
            .expanded(FIELD_MARGIN_DEG)
            .tiles()
            .into_iter()
            .filter_map(|(lat, lon)| lookup(lat, lon).map(|r| ((lat, lon), r)))
            .collect();
        let first = tiles.keys().min().and_then(|k| tiles.get(k));
        let dims = |g: &Grid| (g.height, g.width);
        Self {
            class_dims: first.map(|r| dims(&r.class)),
            brightness_dims: first.map(|r| dims(&r.brightness)),
            scale: first.map(|r| r.scale).unwrap_or_default(),
            tiles,
        }
    }

    fn grid(recipe: &Recipe, layer: Layer) -> &Grid {
        match layer {
            Layer::Class => &recipe.class,
            Layer::Brightness => &recipe.brightness,
        }
    }

    pub(crate) fn recipe(&self, lat0: i32, lon0: i32) -> Option<&Arc<Recipe>> {
        self.tiles.get(&(lat0, lon0))
    }

    /// Rows and columns per degree of `layer` across the field; `None` when
    /// the field has no recipe.
    pub(crate) fn dims(&self, layer: Layer) -> Option<(usize, usize)> {
        match layer {
            Layer::Class => self.class_dims,
            Layer::Brightness => self.brightness_dims,
        }
    }

    /// The value of global cell `(row, col)` of a layer with `ny` x `nx` cells
    /// per degree; `None` without a recipe or when that tile's grid differs.
    pub(crate) fn cell(
        &self,
        layer: Layer,
        ny: usize,
        nx: usize,
        row: i64,
        col: i64,
    ) -> Option<u8> {
        let (ny_i, nx_i) = (ny as i64, nx as i64);
        let lat0 = 89 - row.div_euclid(ny_i);
        let lon0 = col.div_euclid(nx_i) - 180;
        let r = self.recipe(lat0 as i32, lon0 as i32)?;
        let g = Self::grid(r, layer);
        if g.height != ny || g.width != nx {
            return None;
        }
        Some(g.get(row.rem_euclid(ny_i) as usize, col.rem_euclid(nx_i) as usize))
    }

    /// The brightness byte at a point, bilinear between cell centres; 0 where
    /// no recipe covers a neighbour.
    fn brightness_byte(&self, lat: f64, lon: f64) -> f64 {
        let Some((ny, nx)) = self.dims(Layer::Brightness) else {
            return 0.0;
        };
        let u = (lon + 180.0) * nx as f64 - 0.5;
        let v = (90.0 - lat) * ny as f64 - 0.5;
        let (c0, r0) = (u.floor(), v.floor());
        let (fx, fy) = (u - c0, v - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        let at = |r: i64, c: i64| {
            self.cell(Layer::Brightness, ny, nx, r, c)
                .map_or(0.0, f64::from)
        };
        let top = at(r0, c0) * (1.0 - fx) + at(r0, c0 + 1) * fx;
        let bottom = at(r0 + 1, c0) * (1.0 - fx) + at(r0 + 1, c0 + 1) * fx;
        top * (1.0 - fy) + bottom * fy
    }

    /// Radiance (nW/cm2/sr) at a point; 0 where no recipe contributes.
    pub fn radiance(&self, lat: f64, lon: f64) -> f64 {
        let b = self.brightness_byte(lat, lon);
        if b <= 0.0 {
            return 0.0;
        }
        self.scale.radiance_at(b)
    }
}

/// Warp of the class sample position, in class cells (about 45 m at 3
/// arc-seconds): breaks the cell grid into organic edges (R12).
pub const WARP_AMPLITUDE_CELLS: f64 = 0.5;
/// Wavelength of the warp, in class cells.
pub const WARP_WAVELENGTH_CELLS: f64 = 2.0;
const WARP_SEED_X: u64 = 0x5741_5250_5F58_0001;
const WARP_SEED_Y: u64 = 0x5741_5250_5F59_0002;

/// One lit candidate at a point: a class of one tile's palette and its weight.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassWeight {
    pub tile: (i32, i32),
    pub index: u8,
    pub weight: f64,
}

impl RecipeField {
    /// The palette name of class `index` in `tile`.
    pub fn palette_name(&self, tile: (i32, i32), index: u8) -> Option<&str> {
        self.recipe(tile.0, tile.1)?
            .palette
            .get(usize::from(index))
            .map(String::as_str)
    }

    /// The classes at a point with their weights: the bilinear weights of the
    /// four class cell centres around the warped position, merged per class.
    /// Class 0 and cells without a recipe carry no entry, so the weights sum
    /// to less than 1 near the edge of classified ground.
    pub fn class_weights(&self, lat: f64, lon: f64, out: &mut Vec<ClassWeight>) {
        out.clear();
        let Some((ny, nx)) = self.dims(Layer::Class) else {
            return;
        };
        let gx = (lon + 180.0) * nx as f64;
        let gy = (90.0 - lat) * ny as f64;
        let (wx, wy) = (gx / WARP_WAVELENGTH_CELLS, gy / WARP_WAVELENGTH_CELLS);
        let u = gx + WARP_AMPLITUDE_CELLS * value_noise(wx, wy, WARP_SEED_X) - 0.5;
        let v = gy + WARP_AMPLITUDE_CELLS * value_noise(wx, wy, WARP_SEED_Y) - 0.5;
        let (c0, r0) = (u.floor(), v.floor());
        let (fx, fy) = (u - c0, v - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        for (dr, dc, w) in [
            (0, 0, (1.0 - fx) * (1.0 - fy)),
            (0, 1, fx * (1.0 - fy)),
            (1, 0, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            if w <= 0.0 {
                continue;
            }
            let (row, col) = (r0 + dr, c0 + dc);
            let index = match self.cell(Layer::Class, ny, nx, row, col) {
                Some(i) if i != 0 => i,
                _ => continue,
            };
            let tile = (
                (89 - row.div_euclid(ny as i64)) as i32,
                (col.div_euclid(nx as i64) - 180) as i32,
            );
            match out.iter_mut().find(|e| e.tile == tile && e.index == index) {
                Some(e) => e.weight += w,
                None => out.push(ClassWeight {
                    tile,
                    index,
                    weight: w,
                }),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::night::recipe::{BrightnessScale, Grid, Recipe};
    use std::sync::Arc;

    /// A recipe for tile (lat0, lon0): `class(row, col)` on an n_class grid,
    /// `bright(row, col)` on an n_bright grid, palette ["", "lit"].
    pub(crate) fn synthetic(
        lat0: i32,
        lon0: i32,
        n_class: usize,
        class: impl Fn(usize, usize) -> u8,
        n_bright: usize,
        bright: impl Fn(usize, usize) -> u8,
    ) -> Arc<Recipe> {
        let grid = |n: usize, f: &dyn Fn(usize, usize) -> u8| Grid {
            width: n,
            height: n,
            cells: (0..n * n).map(|i| f(i / n, i % n)).collect(),
        };
        Arc::new(Recipe {
            tile: crate::night::tile_name(lat0, lon0),
            palette: vec![String::new(), "lit".into()],
            class: grid(n_class, &class),
            brightness: grid(n_bright, &bright),
            scale: BrightnessScale::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::synthetic;
    use super::*;

    fn footprint(north: f64, south: f64, west: f64, east: f64) -> Footprint {
        Footprint {
            north,
            south,
            west,
            east,
            zoom: 16,
        }
    }

    #[test]
    fn brightness_interpolates_between_cell_centres() {
        // One tile, 4 x 4 brightness: column 1 is 0, column 2 is 200.
        let r = synthetic(
            37,
            -123,
            4,
            |_, _| 1,
            4,
            |_, c| if c >= 2 { 200 } else { 0 },
        );
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        let scale = BrightnessScale::default();
        let mid = field.radiance(37.5, -122.5);
        assert!((mid - scale.radiance_at(100.0)).abs() < 1e-9, "{mid}");
        assert!((field.radiance(37.5, -122.375) - scale.radiance(200)).abs() < 1e-9);
    }

    #[test]
    fn brightness_is_continuous_across_a_tile_edge() {
        let west = synthetic(37, -123, 4, |_, _| 1, 4, |_, _| 150);
        let east = synthetic(37, -122, 4, |_, _| 1, 4, |_, _| 150);
        let lookup = move |a: i32, b: i32| match (a, b) {
            (37, -123) => Some(west.clone()),
            (37, -122) => Some(east.clone()),
            _ => None,
        };
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.1, -121.9), &lookup);
        let expected = BrightnessScale::default().radiance(150);
        for lon in [-122.01, -122.0001, -122.0, -121.9999, -121.99] {
            assert!(
                (field.radiance(37.5, lon) - expected).abs() < 1e-9,
                "lon {lon}"
            );
        }
    }

    #[test]
    fn a_missing_neighbour_reads_as_zero() {
        let only = synthetic(37, -123, 4, |_, _| 1, 4, |_, _| 200);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| only.clone());
        let field = RecipeField::new(&footprint(37.6, 37.4, -122.1, -121.9), &lookup);
        let at_edge = field.radiance(37.5, -122.0);
        let scale = BrightnessScale::default();
        assert!(
            (at_edge - scale.radiance_at(100.0)).abs() < 1e-9,
            "{at_edge}"
        );
        assert_eq!(field.radiance(37.5, -121.5), 0.0, "no recipe at all");
    }

    fn weight_sum(field: &RecipeField, lat: f64, lon: f64) -> f64 {
        let mut out = Vec::new();
        field.class_weights(lat, lon, &mut out);
        out.iter().map(|w| w.weight).sum()
    }

    #[test]
    fn deep_inside_a_class_the_weight_is_one() {
        let r = synthetic(37, -123, 120, |_, _| 1, 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        let mut out = Vec::new();
        field.class_weights(37.5, -122.5, &mut out);
        assert_eq!(out.len(), 1, "one class, merged");
        assert!((out[0].weight - 1.0).abs() < 1e-9);
        assert_eq!(field.palette_name(out[0].tile, out[0].index), Some("lit"));
    }

    #[test]
    fn class_zero_and_missing_tiles_carry_no_weight() {
        let r = synthetic(37, -123, 120, |_, _| 0, 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        let field = RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup);
        assert_eq!(weight_sum(&field, 37.5, -122.5), 0.0);
        assert_eq!(weight_sum(&field, 36.5, -122.5), 0.0, "no recipe");
    }

    /// West half of the tile lit (columns < 60 of 120), east half class 0.
    fn half_lit() -> RecipeField {
        let r = synthetic(37, -123, 120, |_, c| u8::from(c < 60), 4, |_, _| 255);
        let lookup = move |a: i32, b: i32| ((a, b) == (37, -123)).then(|| r.clone());
        RecipeField::new(&footprint(38.0, 37.0, -123.0, -122.0), &lookup)
    }

    #[test]
    fn a_class_edge_is_a_ramp_not_a_step() {
        // The edge is at lon -122.5; a cell is 1/120 degree. Sample across it.
        let field = half_lit();
        let cell = 1.0 / 120.0;
        let ramp: Vec<f64> = (-30..=30)
            .map(|k| weight_sum(&field, 37.5003, -122.5 + f64::from(k) * cell / 10.0))
            .collect();
        let between = ramp.iter().filter(|w| **w > 0.05 && **w < 0.95).count();
        assert!(
            between >= 5,
            "at least half a cell of intermediate weights: {ramp:?}"
        );
        assert!(ramp.first().unwrap() > &0.95 && ramp.last().unwrap() < &0.05);
    }

    #[test]
    fn a_class_edge_is_not_straight() {
        // Where the weight crosses 0.5 moves along the edge, by the warp.
        let field = half_lit();
        let cell = 1.0 / 120.0;
        let crossings: Vec<f64> = (0..40)
            .map(|k| {
                let lat = 37.3 + f64::from(k) * cell * 0.7;
                (0..400)
                    .map(|s| -122.5 - 2.0 * cell + f64::from(s) * cell / 100.0)
                    .find(|&lon| weight_sum(&field, lat, lon) < 0.5)
                    .unwrap()
            })
            .collect();
        let spread = crossings.iter().cloned().fold(f64::MIN, f64::max)
            - crossings.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread > 0.3 * cell, "the 50% line wanders by {spread} deg");
        assert!(spread < 2.0 * cell, "but stays near the edge: {spread} deg");
    }
}
