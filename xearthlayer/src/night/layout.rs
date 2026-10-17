//! The layout of lit DDS files. Phase 1 has one: the minimal black DDS.

use std::sync::OnceLock;

use bytes::Bytes;

use crate::dds::{DdsFormat, DdsHeader};

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
}
