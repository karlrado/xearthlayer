//! The lit generator: the first visible level of a lit texture, as pixels.
//!
//! `LitGenerator` is the seam for other implementations (a GPU one later,
//! R17); `CpuLitGenerator` is the one in use.

pub mod cpu;
pub mod field;
pub mod gain;
pub mod noise;
pub mod tiling;

use std::sync::Arc;

use image::RgbaImage;

use super::laminar::LitPattern;
use super::recipe::Recipe;
use crate::fuse::DdsFilename;

pub use cpu::CpuLitGenerator;
pub use gain::{gain, GAIN_CEILING_NW, GAIN_FLOOR_NW, GAIN_GAMMA};

/// The recipe of the 1 degree tile whose south-west corner is `(lat, lon)`.
pub type RecipeLookup<'a> = &'a (dyn Fn(i32, i32) -> Option<Arc<Recipe>> + Sync);

/// The lit pattern a recipe palette name stands for, or `None` (dark).
pub type PatternLookup<'a> = &'a (dyn Fn(&str) -> Option<Arc<LitPattern>> + Sync);

/// Everything one generation needs.
pub struct LitRequest<'a> {
    /// The lit texture's name: its footprint and zoom.
    pub name: &'a DdsFilename,
    /// Width and height of the first visible level, in pixels.
    pub size: u32,
    /// `night.brightness`.
    pub brightness: f64,
    pub recipes: RecipeLookup<'a>,
    pub patterns: PatternLookup<'a>,
}

/// Produces the first visible level of a lit texture.
pub trait LitGenerator: Send + Sync {
    /// `size` x `size` pixels, north up, black wherever nothing is lit.
    fn generate(&self, request: &LitRequest<'_>) -> RgbaImage;
}

/// BC1-encode the first visible level and its mip chain down to 1 x 1, as
/// the bytes that follow the blank levels in a lit DDS (no header).
pub fn encode_visible(image: RgbaImage) -> Result<bytes::Bytes, crate::dds::DdsError> {
    let dds = crate::dds::DdsEncoder::new(crate::dds::DdsFormat::BC1).encode(image)?;
    Ok(bytes::Bytes::from(dds).slice(DDS_HEADER_LEN..))
}

/// Bytes of a DDS header (magic plus the 124-byte header).
const DDS_HEADER_LEN: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;
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

    fn generate(name: &DdsFilename, size: u32) -> RgbaImage {
        let recipes = |lat: i32, lon: i32| {
            ((lat, lon) == (37, -123)).then(|| Arc::new(parse_recipe(URBAN).unwrap()))
        };
        let patterns = |n: &str| {
            (n == "lib/g10/terrain10/urban_tmp_sdry3.ter").then(|| {
                Arc::new(LitPattern {
                    size: 4,
                    rgb: vec![[255, 0, 0]; 16],
                    ground_scale_m: 4096.0,
                })
            })
        };
        CpuLitGenerator.generate(&LitRequest {
            name,
            size,
            brightness: 1.0,
            recipes: &recipes,
            patterns: &patterns,
        })
    }

    #[test]
    fn encoded_visible_levels_fit_the_layout() {
        let layout = crate::night::LitLayout::new(18, 4); // first visible 256
        let img = generate(&name_at(37.20, -122.725), 256);
        let payload = encode_visible(img).unwrap();
        assert_eq!(payload.len() as u64, layout.payload_len());
        assert!(payload.iter().any(|&b| b != 0), "light survives encoding");
    }
}
