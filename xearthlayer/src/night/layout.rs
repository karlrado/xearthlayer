//! The layout of lit DDS files: the minimal black DDS (phase 1), and
//! `LitLayout`, the full mip chain geometry and all-zero bytes of a lit
//! footprint (phase 2a).

use std::sync::OnceLock;

use bytes::Bytes;

use crate::cache::integrity::full_chain_bc1_dds_size;
use crate::dds::{DdsFormat, DdsHeader};

/// First visible level of a ZL16 lit texture: about 7.5 m per texel.
pub const FIRST_VISIBLE_ZL16: u32 = 1024;

/// Largest top level a lit DDS declares. At the lowest texture setting the
/// uncapped top would be 32768 px, unverified in X-Plane; the cap shrinks the
/// first visible level instead (design: Lit DDS layout, Risk).
pub const MAX_TOP_LEVEL: u32 = 16384;

/// The shape of one lit DDS: blank levels for the fade, then the visible ones.
#[derive(Debug, Clone)]
pub struct LitLayout {
    pub top: u32,
    pub levels: u32,
    pub blank_levels: u32,
    pub first_visible: u32,
    pub total_len: u64,
    header: Bytes,
}

impl LitLayout {
    /// The layout for a texture at `zoom` under X-Plane texture setting `t` (R15).
    pub fn new(zoom: u8, texture_setting: u8) -> Self {
        let t = u32::from(texture_setting.min(4));
        let blank_levels = (4 - t) + 1;
        let wanted_first = if zoom >= 16 {
            (FIRST_VISIBLE_ZL16 >> (zoom - 16)).max(4)
        } else {
            (FIRST_VISIBLE_ZL16 << (16 - zoom)).min(MAX_TOP_LEVEL)
        };
        let top = (wanted_first << blank_levels).min(MAX_TOP_LEVEL);
        let first_visible = top >> blank_levels;
        let levels = top.ilog2() + 1;
        let header = Bytes::from(DdsHeader::new(top, top, levels, DdsFormat::BC1).to_bytes());
        let total_len = full_chain_bc1_dds_size(top, top) as u64;
        Self {
            top,
            levels,
            blank_levels,
            first_visible,
            total_len,
            header,
        }
    }

    /// Bytes `[offset, offset + len)` of the all-black texture: the header,
    /// then zero blocks (black, adds nothing to the base colour).
    pub fn read_black(&self, offset: u64, len: u32) -> Bytes {
        let start = offset.min(self.total_len);
        let end = start.saturating_add(u64::from(len)).min(self.total_len);
        let mut out = vec![0u8; (end - start) as usize];
        let header_len = self.header.len() as u64;
        if start < header_len {
            let n = (header_len.min(end) - start) as usize;
            out[..n].copy_from_slice(&self.header[start as usize..start as usize + n]);
        }
        Bytes::from(out)
    }
}

/// The texture served wherever nothing is lit: 4 x 4 DXT1 with its full
/// chain (4, 2 and 1 px levels, one 8-byte block each), all blocks zero,
/// which decodes to black and adds nothing to the base colour. 152 bytes.
pub fn minimal_black_dds() -> Bytes {
    static DDS: OnceLock<Bytes> = OnceLock::new();
    DDS.get_or_init(|| {
        let mut bytes = DdsHeader::new(4, 4, 3, DdsFormat::BC1).to_bytes();
        bytes.resize(bytes.len() + 3 * 8, 0);
        Bytes::from(bytes)
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn minimal_black_dds_is_a_152_byte_4x4_dxt1_with_three_levels() {
        let dds = minimal_black_dds();
        assert_eq!(dds.len(), 152);
        assert_eq!(&dds[0..4], b"DDS ");
        assert_eq!(u32_at(&dds, 12), 4, "height");
        assert_eq!(u32_at(&dds, 16), 4, "width");
        assert_eq!(u32_at(&dds, 28), 3, "mip levels");
        assert_eq!(&dds[84..88], b"DXT1");
        assert!(dds[128..].iter().all(|&b| b == 0), "all blocks black");
    }

    #[test]
    fn zl16_at_full_setting_matches_the_design_table() {
        let l = LitLayout::new(16, 4);
        assert_eq!((l.blank_levels, l.first_visible, l.top), (1, 1024, 2048));
        assert_eq!(l.levels, 12);
        assert_eq!(l.total_len, 2_796_344);
    }

    #[test]
    fn lower_settings_blank_more_levels() {
        let l = LitLayout::new(16, 2);
        assert_eq!((l.blank_levels, l.top), (3, 8192));
        let l = LitLayout::new(16, 3);
        assert_eq!((l.blank_levels, l.top), (2, 4096));
    }

    #[test]
    fn first_visible_scales_with_zoom() {
        assert_eq!(LitLayout::new(17, 4).first_visible, 512);
        assert_eq!(LitLayout::new(18, 4).first_visible, 256);
        assert_eq!(LitLayout::new(15, 4).first_visible, 2048);
    }

    #[test]
    fn the_top_level_is_capped() {
        let l = LitLayout::new(16, 0);
        assert_eq!(l.top, MAX_TOP_LEVEL);
        assert_eq!(l.first_visible, MAX_TOP_LEVEL >> l.blank_levels);
    }

    #[test]
    fn black_reads_are_header_then_zeros_and_agree_with_size() {
        let l = LitLayout::new(16, 4);
        let head = l.read_black(0, 200);
        assert_eq!(&head[0..4], b"DDS ");
        assert_eq!(u32_at(&head, 16), 2048, "width");
        assert_eq!(u32_at(&head, 28), 12, "mips");
        assert!(head[128..].iter().all(|&b| b == 0));
        let all: usize = (0..l.total_len)
            .step_by(1 << 20)
            .map(|o| l.read_black(o, 1 << 20).len())
            .sum();
        assert_eq!(all as u64, l.total_len);
        assert!(l.read_black(l.total_len, 8).is_empty());
        assert_eq!(l.read_black(l.total_len - 3, 8).len(), 3);
    }
}
