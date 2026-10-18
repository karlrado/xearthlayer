//! Hex-grid stochastic tiling.
//!
//! A triangle lattice (the dual of a hex grid) is laid over Web Mercator
//! metres with cells the size of the pattern's ground scale. Each lattice
//! vertex hashes its integer coordinates to an offset and a quarter-turn
//! rotation, and a point blends the pattern as its three surrounding
//! vertices transform it, with sharpened barycentric weights. Everything is
//! a function of world position, so textures never disagree on shared
//! ground and the pattern never visibly repeats.

use super::noise::{hash2, mix, unit};
use crate::night::laminar::LitPattern;

const EARTH_RADIUS_M: f64 = 6_378_137.0;

/// Exponent that sharpens the three blend weights, so the blend keeps the
/// pattern's contrast instead of averaging three copies flat.
pub const BLEND_SHARPNESS: i32 = 4;

/// Skew and unskew factors of a 2D triangle lattice: (sqrt(3) - 1) / 2 and
/// (3 - sqrt(3)) / 6.
const SKEW: f64 = 0.366_025_403_784_438_6;
const UNSKEW: f64 = 0.211_324_865_405_187_1;

const TILING_SEED: u64 = 0x4E49_4748_545F_4C49;

/// Web Mercator metres of a latitude and longitude.
pub fn mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = EARTH_RADIUS_M * lon.to_radians();
    let y = EARTH_RADIUS_M
        * (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
            .tan()
            .ln();
    (x, y)
}

/// Latitude (degrees) of a Web Mercator northing.
fn latitude_of(y: f64) -> f64 {
    (2.0 * (y / EARTH_RADIUS_M).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

/// The pattern's colour at Web Mercator `(x, y)` metres.
pub fn sample(pattern: &LitPattern, x: f64, y: f64) -> [f32; 3] {
    let cell = pattern.ground_scale_m;
    let (a, b) = (x / cell, y / cell);
    let s = (a + b) * SKEW;
    let (i, j) = ((a + s).floor(), (b + s).floor());
    let t = (i + j) * UNSKEW;
    let (x0, y0) = (a - (i - t), b - (j - t));
    let (i1, j1) = if x0 > y0 { (1.0, 0.0) } else { (0.0, 1.0) };
    let corners = [(i, j), (i + i1, j + j1), (i + 1.0, j + 1.0)];
    let unskewed = corners.map(|(ci, cj)| {
        let t = (ci + cj) * UNSKEW;
        (ci - t, cj - t)
    });
    let weights = barycentric((a, b), unskewed).map(|w| w.max(0.0).powi(BLEND_SHARPNESS));
    let total: f64 = weights.iter().sum();
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        let w = (weights[k] / total) as f32;
        if w == 0.0 {
            continue;
        }
        let c = corner_sample(pattern, x, y, corners[k], unskewed[k]);
        for ch in 0..3 {
            out[ch] += w * c[ch];
        }
    }
    out
}

/// The pattern as lattice corner `(ci, cj)` transforms it, at `(x, y)`.
fn corner_sample(
    pattern: &LitPattern,
    x: f64,
    y: f64,
    (ci, cj): (f64, f64),
    (px, py): (f64, f64),
) -> [f32; 3] {
    let h = hash2(
        ci as i64,
        cj as i64,
        TILING_SEED ^ pattern.ground_scale_m.to_bits(),
    );
    let (ou, ov) = (unit(h), unit(mix(h)));
    let turns = mix(mix(h)) % 4;
    let (vx, vy) = (px * pattern.ground_scale_m, py * pattern.ground_scale_m);
    // Pattern repeats per Mercator metre at the corner's own latitude.
    let k = latitude_of(vy).to_radians().cos() / pattern.ground_scale_m;
    let (du, dv) = ((x - vx) * k, -(y - vy) * k); // image v runs south
    let (u, v) = match turns {
        0 => (du, dv),
        1 => (-dv, du),
        2 => (-du, -dv),
        _ => (dv, -du),
    };
    pattern.sample(u + ou, v + ov)
}

/// Barycentric coordinates of `q` in the triangle `p`.
fn barycentric(q: (f64, f64), p: [(f64, f64); 3]) -> [f64; 3] {
    let ((x1, y1), (x2, y2), (x3, y3)) = (p[0], p[1], p[2]);
    let det = (y2 - y3) * (x1 - x3) + (x3 - x2) * (y1 - y3);
    let l1 = ((y2 - y3) * (q.0 - x3) + (x3 - x2) * (q.1 - y3)) / det;
    let l2 = ((y3 - y1) * (q.0 - x3) + (x1 - x3) * (q.1 - y3)) / det;
    [l1, l2, 1.0 - l1 - l2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::night::generator::noise::{hash2, unit};
    use crate::night::laminar::LitPattern;

    fn solid() -> LitPattern {
        LitPattern {
            size: 4,
            rgb: vec![[200, 100, 50]; 16],
            ground_scale_m: 4096.0,
        }
    }

    /// 32 x 32 random texels: every copy and rotation looks different.
    fn speckled() -> LitPattern {
        let rgb = (0..32 * 32)
            .map(|i| {
                let h = hash2(i, 0, 99);
                [(unit(h) * 255.0) as u8, (unit(h >> 8) * 255.0) as u8, 0]
            })
            .collect();
        LitPattern {
            size: 32,
            rgb,
            ground_scale_m: 4096.0,
        }
    }

    fn points() -> impl Iterator<Item = (f64, f64)> {
        (0..300).map(|i| {
            let lat = 37.0 + unit(hash2(i, 1, 5)) * 1.5;
            let lon = -123.0 + unit(hash2(i, 2, 5)) * 1.5;
            mercator(lat, lon)
        })
    }

    #[test]
    fn a_solid_pattern_stays_solid() {
        let p = solid();
        for (x, y) in points() {
            let c = sample(&p, x, y);
            for (got, want) in c.iter().zip([200.0, 100.0, 50.0]) {
                assert!((got - want).abs() < 1e-3, "{c:?}");
            }
        }
    }

    #[test]
    fn sampling_is_deterministic_and_continuous() {
        let p = speckled();
        for (x, y) in points() {
            let a = sample(&p, x, y);
            assert_eq!(a, sample(&p, x, y));
            let b = sample(&p, x + 0.01, y + 0.01);
            for ch in 0..3 {
                assert!((a[ch] - b[ch]).abs() < 2.0, "1 cm apart: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn the_pattern_does_not_repeat_at_its_ground_scale() {
        // Straight tiling would repeat exactly one ground scale east.
        let p = speckled();
        let mut differ = 0;
        for (x, y) in points() {
            let lat =
                (2.0 * (y / 6_378_137.0).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
            let step = p.ground_scale_m / lat.to_radians().cos();
            let (a, b) = (sample(&p, x, y), sample(&p, x + step, y));
            if (a[0] - b[0]).abs() > 5.0 {
                differ += 1;
            }
        }
        assert!(differ > 200, "only {differ} of 300 differ");
    }

    #[test]
    fn mercator_of_the_origin_and_a_known_point() {
        let (ox, oy) = mercator(0.0, 0.0);
        assert!(ox.abs() < 1e-6 && oy.abs() < 1e-6, "{ox} {oy}");
        let (x, y) = mercator(37.0, -122.0);
        assert!((x - -13_580_977.876_779_376).abs() < 0.1, "{x}");
        assert!((y - 4_439_106.787_250_583).abs() < 0.1, "{y}");
    }
}
