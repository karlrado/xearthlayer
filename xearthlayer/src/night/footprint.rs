//! A lit texture's ground footprint, and whether any of it is lit.

use std::f64::consts::PI;
use std::sync::Arc;

use super::recipe::{Grid, Recipe};
use crate::fuse::DdsFilename;

/// Web Mercator tiles along each side of one Ortho4XP texture.
pub const TILES_PER_TEXTURE: u32 = 16;

/// The ground one lit texture covers, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Footprint {
    pub north: f64,
    pub south: f64,
    pub west: f64,
    pub east: f64,
    pub zoom: u8,
}

impl Footprint {
    /// The footprint of `<til_y_top>_<til_x_left>_<map><zoom>` (R5).
    pub fn from_name(name: &DdsFilename) -> Self {
        let n = f64::from(1u32 << name.zoom);
        let lon = |x: f64| x / n * 360.0 - 180.0;
        let lat = |y: f64| (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
        let (x, y) = (f64::from(name.col), f64::from(name.row));
        let side = f64::from(TILES_PER_TEXTURE);
        Self {
            north: lat(y),
            south: lat(y + side),
            west: lon(x),
            east: lon(x + side),
            zoom: name.zoom,
        }
    }

    /// The footprint grown by `degrees` on every side.
    pub fn expanded(&self, degrees: f64) -> Self {
        Self {
            north: self.north + degrees,
            south: self.south - degrees,
            west: self.west - degrees,
            east: self.east + degrees,
            zoom: self.zoom,
        }
    }

    /// South-west corners of every 1 degree tile the footprint touches.
    pub fn tiles(&self) -> Vec<(i32, i32)> {
        let lats = (self.south.floor() as i32)..=(self.north.ceil() as i32 - 1);
        let lons = (self.west.floor() as i32)..=(self.east.ceil() as i32 - 1);
        lats.flat_map(|lat| lons.clone().map(move |lon| (lat, lon)))
            .collect()
    }

    /// True when some lit class in the footprint has radiance (R7: answers
    /// the file size from recipes alone, without generating pixels).
    pub fn has_light(
        &self,
        recipe_for: impl Fn(i32, i32) -> Option<Arc<Recipe>>,
        is_lit: impl Fn(&str) -> bool,
    ) -> bool {
        self.tiles().into_iter().any(|(lat0, lon0)| {
            recipe_for(lat0, lon0).is_some_and(|r| self.tile_has_light(&r, lat0, lon0, &is_lit))
        })
    }

    fn tile_has_light(
        &self,
        r: &Recipe,
        lat0: i32,
        lon0: i32,
        is_lit: &impl Fn(&str) -> bool,
    ) -> bool {
        let (top, west) = (f64::from(lat0) + 1.0, f64::from(lon0));
        let class = &r.class;
        let (h, w) = (class.height as f64, class.width as f64);
        let rows = span(top - self.north, top - self.south, h, class.height);
        let cols = span(self.west - west, self.east - west, w, class.width);
        for row in rows {
            let lat = top - (row as f64 + 0.5) / h;
            if !(self.south..self.north).contains(&lat) {
                continue;
            }
            for col in cols.clone() {
                let lon = west + (col as f64 + 0.5) / w;
                if !(self.west..self.east).contains(&lon) {
                    continue;
                }
                let index = usize::from(class.get(row, col));
                if index != 0
                    && sample(&r.brightness, top - lat, lon - west) > 0
                    && is_lit(&r.palette[index])
                {
                    return true;
                }
            }
        }
        false
    }
}

/// Grid indices that can overlap `[from, to)` degrees from the grid's edge.
fn span(from: f64, to: f64, cells_per_degree: f64, len: usize) -> std::ops::Range<usize> {
    let lo = (from * cells_per_degree).floor().max(0.0) as usize;
    let hi = ((to * cells_per_degree).ceil().max(0.0) as usize).min(len);
    lo.min(hi)..hi
}

/// The grid cell containing a point `dy` degrees below the top, `dx` east of the west edge.
pub(crate) fn sample(grid: &Grid, dy: f64, dx: f64) -> u8 {
    let row = ((dy * grid.height as f64).floor().max(0.0) as usize).min(grid.height - 1);
    let col = ((dx * grid.width as f64).floor().max(0.0) as usize).min(grid.width - 1);
    grid.get(row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::parse_dds_filename;
    use crate::night::parse_recipe;

    const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");

    fn urban() -> Option<Arc<Recipe>> {
        Some(Arc::new(parse_recipe(URBAN).unwrap()))
    }

    fn only_tile(lat: i32, lon: i32) -> impl Fn(i32, i32) -> Option<Arc<Recipe>> {
        move |a, b| if (a, b) == (lat, lon) { urban() } else { None }
    }

    fn urban_only(name: &str) -> bool {
        name == "lib/g10/terrain10/urban_tmp_sdry3.ter"
    }

    #[test]
    fn expanded_grows_every_edge() {
        let f = fp(37.24, 37.16, -122.74, -122.71).expanded(0.01);
        assert!((f.north - 37.25).abs() < 1e-12 && (f.south - 37.15).abs() < 1e-12);
        assert!((f.west - -122.75).abs() < 1e-12 && (f.east - -122.70).abs() < 1e-12);
        assert_eq!(f.zoom, 16);
    }

    fn fp(north: f64, south: f64, west: f64, east: f64) -> Footprint {
        Footprint {
            north,
            south,
            west,
            east,
            zoom: 16,
        }
    }

    #[test]
    fn footprint_from_an_ortho4xp_name() {
        // A real California texture: LOAD_CENTER 42.19597 -122.38770.
        let f = Footprint::from_name(&parse_dds_filename("24272_10480_GO216_LIT.dds").unwrap());
        assert!((f.west - -122.431640625).abs() < 1e-9);
        assert!((f.east - -122.34375).abs() < 1e-9);
        assert!(((f.north + f.south) / 2.0 - 42.19597).abs() < 0.01);
        assert!(f.north > f.south);
        assert_eq!(f.tiles(), vec![(42, -123)]);
    }

    #[test]
    fn tiles_straddling_a_degree_line() {
        let f = fp(38.02, 37.98, -122.03, -121.97);
        let mut t = f.tiles();
        t.sort();
        assert_eq!(t, vec![(37, -123), (37, -122), (38, -123), (38, -122)]);
    }

    // Golden urban.oxn (tile +37-123), from the recipe format's Conformance table:
    // urban (index 2) rows 900-1019 cols 300-359 = lat 37.25..37.15, lon -122.75..-122.70,
    //   under brightness row 3 col 1 = 230 (lit);
    // urban rows 120-179 cols 120-239 = lat 37.90..37.85, lon -122.90..-122.80,
    //   under brightness row 0 col 0 = 0 (dark: no radiance);
    // industrial (index 1) rows 600-659 cols 600-719 = lat 37.50..37.45, lon -122.50..-122.40.

    #[test]
    fn lit_where_an_urban_class_meets_radiance() {
        assert!(fp(37.24, 37.16, -122.74, -122.71).has_light(only_tile(37, -123), urban_only));
    }

    #[test]
    fn lit_when_only_the_second_straddled_tile_has_light() {
        // Spans lon -123.02..-122.70: tiles (37,-124), which has no recipe, then
        // (37,-123), which holds the lit urban block.
        let f = fp(37.24, 37.16, -123.02, -122.70);
        assert_eq!(f.tiles(), vec![(37, -124), (37, -123)]);
        assert!(f.has_light(only_tile(37, -123), urban_only));
        assert!(!f.has_light(only_tile(37, -124), urban_only));
    }

    #[test]
    fn dark_where_urban_has_no_radiance() {
        assert!(!fp(37.89, 37.86, -122.89, -122.81).has_light(only_tile(37, -123), urban_only));
    }

    #[test]
    fn dark_where_the_class_does_not_resolve_to_lit_art() {
        assert!(!fp(37.49, 37.46, -122.49, -122.41).has_light(only_tile(37, -123), urban_only));
        assert!(fp(37.49, 37.46, -122.49, -122.41).has_light(only_tile(37, -123), |_| true));
    }

    #[test]
    fn dark_over_countryside_and_without_a_recipe() {
        assert!(!fp(37.62, 37.58, -122.22, -122.18).has_light(only_tile(37, -123), |_| true));
        assert!(!fp(37.24, 37.16, -122.74, -122.71).has_light(|_, _| None, |_| true));
    }
}
