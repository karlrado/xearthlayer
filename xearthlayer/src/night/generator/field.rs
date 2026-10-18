//! Recipe layers sampled as continuous functions of world position.
//!
//! Cells are indexed globally (row 0 at the north pole, column 0 at 180 W),
//! so a sample near a 1 degree edge reads the neighbouring tile's cells and
//! every texture computes the same value for the same ground.

use std::collections::HashMap;
use std::sync::Arc;

use super::RecipeLookup;
use crate::night::footprint::Footprint;
use crate::night::recipe::{BrightnessScale, Grid, Recipe};

/// How far beyond a footprint sampling can reach, in degrees: bilinear
/// neighbours and the class warp stay well inside it.
pub const FIELD_MARGIN_DEG: f64 = 0.01;

/// A recipe layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    // used from Task 4/6
    #[allow(dead_code)]
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

    // used from Task 4/6
    #[allow(dead_code)]
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
        let r = self.tiles.get(&(lat0 as i32, lon0 as i32))?;
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
}
