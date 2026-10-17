//! Where FUSE gets lit textures.
//!
//! The lit path never touches the ortho executor, coalescer or caches
//! (requirement R8). Phase 1 serves the minimal black DDS for every name.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;

use super::layout::minimal_black_dds;
use super::{Footprint, LaminarArt, LitLayout, RecipeStore, TextureSettingSource};
use crate::fuse::DdsFilename;

/// The source of lit DDS bytes for FUSE.
///
/// `size` must equal the length `read` serves for the same name (R7), and
/// neither may fail: every failure is a black texture (R4, R19).
pub trait LitTextureSource: Send + Sync {
    /// The file size for `name`, known before any generation.
    fn size<'a>(&'a self, name: &'a DdsFilename) -> BoxFuture<'a, u64>;

    /// Up to `len` bytes from `offset`; empty at or past the end.
    fn read<'a>(&'a self, name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes>;
}

/// Serves the minimal black DDS for every lit name.
#[derive(Debug, Default, Clone, Copy)]
pub struct BlackLitSource;

impl LitTextureSource for BlackLitSource {
    fn size<'a>(&'a self, _name: &'a DdsFilename) -> BoxFuture<'a, u64> {
        Box::pin(async { minimal_black_dds().len() as u64 })
    }

    fn read<'a>(&'a self, _name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes> {
        Box::pin(async move {
            let dds = minimal_black_dds();
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(dds.len());
            let end = start.saturating_add(len as usize).min(dds.len());
            dds.slice(start..end)
        })
    }
}

/// Concurrent `has_light` probes (recipe and Laminar file reads). The permits
/// sit inside the blocking pool's `BLOCKING_POOL_HEADROOM`.
const LIT_PROBE_PERMITS: usize = 2;

/// Footprints whose answer is remembered.
const LIT_PROBE_CACHE_ENTRIES: u64 = 65_536;

/// Zoom levels a lit footprint is computed for. `parse_dds_filename` accepts
/// any two-digit zoom and `Footprint::from_name` shifts by it, so anything
/// outside this range is dark without computing anything.
const SUPPORTED_LIT_ZOOMS: std::ops::RangeInclusive<u8> = 12..=22;

/// Phase 2a's lit source: full-size, correctly laid out black textures where
/// the recipe and the user's art say there is light; the minimal black DDS
/// everywhere else.
pub struct NightLitSource {
    store: Arc<RecipeStore>,
    art: Arc<LaminarArt>,
    setting: Arc<dyn TextureSettingSource>,
    lit: moka::sync::Cache<(u32, u32, u8), bool>,
    probes: tokio::sync::Semaphore,
}

impl NightLitSource {
    pub fn new(
        store: Arc<RecipeStore>,
        art: Arc<LaminarArt>,
        setting: Arc<dyn TextureSettingSource>,
    ) -> Self {
        Self {
            store,
            art,
            setting,
            lit: moka::sync::Cache::new(LIT_PROBE_CACHE_ENTRIES),
            probes: tokio::sync::Semaphore::new(LIT_PROBE_PERMITS),
        }
    }

    async fn has_light(&self, name: &DdsFilename) -> bool {
        if !SUPPORTED_LIT_ZOOMS.contains(&name.zoom) {
            return false;
        }
        let key = (name.row, name.col, name.zoom);
        if let Some(hit) = self.lit.get(&key) {
            return hit;
        }
        let Ok(_permit) = self.probes.acquire().await else {
            return false;
        };
        if let Some(hit) = self.lit.get(&key) {
            return hit; // another probe answered while we waited
        }
        let (store, art, footprint) = (
            Arc::clone(&self.store),
            Arc::clone(&self.art),
            Footprint::from_name(name),
        );
        match tokio::task::spawn_blocking(move || {
            footprint.has_light(
                |lat, lon| store.get(lat, lon),
                |n| art.lit_terrain(n).is_some(),
            )
        })
        .await
        {
            Ok(answer) => {
                self.lit.insert(key, answer);
                answer
            }
            Err(_) => false, // panicked: dark for this call, not remembered
        }
    }

    fn layout(&self, name: &DdsFilename) -> LitLayout {
        LitLayout::new(name.zoom, self.setting.texture_setting())
    }
}

impl LitTextureSource for NightLitSource {
    fn size<'a>(&'a self, name: &'a DdsFilename) -> BoxFuture<'a, u64> {
        Box::pin(async move {
            if self.has_light(name).await {
                self.layout(name).total_len
            } else {
                minimal_black_dds().len() as u64
            }
        })
    }

    fn read<'a>(&'a self, name: &'a DdsFilename, offset: u64, len: u32) -> BoxFuture<'a, Bytes> {
        Box::pin(async move {
            if self.has_light(name).await {
                self.layout(name).read_black(offset, len)
            } else {
                BlackLitSource.read(name, offset, len).await
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{DdsFilename, TextureKind};

    fn lit() -> DdsFilename {
        DdsFilename {
            row: 25712,
            col: 12960,
            zoom: 16,
            map_type: "BI".into(),
            kind: TextureKind::Lit,
        }
    }

    #[tokio::test]
    async fn black_source_reports_the_minimal_size() {
        assert_eq!(BlackLitSource.size(&lit()).await, 152);
    }

    #[tokio::test]
    async fn black_source_serves_ranges_of_the_minimal_dds() {
        let whole = BlackLitSource.read(&lit(), 0, 4096).await;
        assert_eq!(whole, minimal_black_dds());
        assert_eq!(BlackLitSource.read(&lit(), 128, 8).await.len(), 8);
        assert!(
            BlackLitSource.read(&lit(), 152, 8).await.is_empty(),
            "at EOF"
        );
        assert!(
            BlackLitSource.read(&lit(), 9999, 8).await.is_empty(),
            "past EOF"
        );
    }

    mod night_source {
        use super::super::*;
        use crate::fuse::{DdsFilename, TextureKind};
        use crate::night::{FixedTextureSetting, LaminarArt, RecipeStore};
        use std::sync::Arc;
        use tempfile::TempDir;

        const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");

        /// A package with urban.oxn for +37-123 and an installation where
        /// urban_tmp_sdry3 is lit.
        fn fixture() -> (TempDir, NightLitSource) {
            let temp = TempDir::new().unwrap();
            let pkg = temp.path().join("pkg");
            let recipe = crate::night::recipe_path(&pkg, 37, -123);
            std::fs::create_dir_all(recipe.parent().unwrap()).unwrap();
            std::fs::write(&recipe, URBAN).unwrap();

            let art = temp.path().join("art");
            std::fs::create_dir_all(art.join("terrain12")).unwrap();
            std::fs::create_dir_all(art.join("textures12/urban_FAR")).unwrap();
            std::fs::write(
                art.join("library.txt"),
                "A\n1200\nLIBRARY\nEXPORT lib/g10/terrain10/urban_tmp_sdry3.ter terrain12/u.ter\n",
            )
            .unwrap();
            std::fs::write(
                art.join("terrain12/u.ter"),
                "TEXTURE_LIT ../textures12/urban_FAR/uw5_FAR_LIT.dds\n",
            )
            .unwrap();
            std::fs::write(art.join("textures12/urban_FAR/uw5_FAR_LIT.dds"), b"DDS ").unwrap();

            let source = NightLitSource::new(
                Arc::new(RecipeStore::new(vec![pkg])),
                Arc::new(LaminarArt::new(art)),
                Arc::new(FixedTextureSetting(4)),
            );
            (temp, source)
        }

        /// The ZL16 texture name whose footprint contains (lat, lon).
        fn name_at(lat: f64, lon: f64) -> DdsFilename {
            let n = f64::from(1u32 << 16);
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let lat_r = lat.to_radians();
            let y = ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0
                * n) as u32;
            DdsFilename {
                row: y / 16 * 16,
                col: x / 16 * 16,
                zoom: 16,
                map_type: "BI".into(),
                kind: TextureKind::Lit,
            }
        }

        #[tokio::test]
        async fn a_lit_footprint_is_full_size_black() {
            let (_t, source) = fixture();
            let name = name_at(37.20, -122.725); // inside the golden urban block over radiance
            assert_eq!(source.size(&name).await, 2_796_344);
            let head = source.read(&name, 0, 128).await;
            assert_eq!(&head[0..4], b"DDS ");
            assert!(source
                .read(&name, 1_000_000, 4096)
                .await
                .iter()
                .all(|&b| b == 0));
        }

        #[tokio::test]
        async fn a_dark_footprint_is_the_minimal_black_dds() {
            let (_t, source) = fixture();
            let name = name_at(37.60, -122.20); // countryside in the golden tile
            assert_eq!(source.size(&name).await, 152);
            assert_eq!(source.read(&name, 0, 4096).await, minimal_black_dds());
        }

        #[tokio::test]
        async fn size_and_read_agree() {
            let (_t, source) = fixture();
            for name in [name_at(37.20, -122.725), name_at(37.60, -122.20)] {
                let size = source.size(&name).await;
                assert_eq!(source.read(&name, 0, u32::MAX).await.len() as u64, size);
            }
        }

        #[tokio::test]
        async fn an_unsupported_zoom_is_dark_without_panicking() {
            let (_t, source) = fixture();
            let mut name = name_at(37.20, -122.725);
            name.zoom = 40;
            assert_eq!(source.size(&name).await, 152);
            assert_eq!(source.read(&name, 0, 4096).await, minimal_black_dds());
        }
    }
}
