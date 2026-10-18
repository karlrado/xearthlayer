//! Where FUSE gets lit textures.
//!
//! The lit path never touches the ortho executor, coalescer or caches
//! (requirement R8). `BlackLitSource` (phase 1) serves the minimal black DDS
//! for every name; `NightLitSource` (phase 2a) serves full-size black
//! textures where the recipe and the user's art say there is light, and the
//! minimal DDS elsewhere. Phase 2b makes it generate the lit pixels: concurrent
//! reads of one footprint share a single generation, and the number of
//! concurrent generations follows the core count.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use tracing::{debug, warn};

use super::generator::field::{RecipeField, FIELD_MARGIN_DEG};
use super::generator::{CpuLitGenerator, LitGenerator, LitRequest};
use super::layout::minimal_black_dds;
use super::{Footprint, LaminarArt, LitLayout, Recipe, RecipeStore, TextureSettingSource};
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

/// Zoom levels a lit footprint is computed for. `parse_dds_filename` accepts
/// any two-digit zoom and `Footprint::from_name` shifts by it, so anything
/// outside this range is dark without computing anything.
const SUPPORTED_LIT_ZOOMS: std::ops::RangeInclusive<u8> = 12..=22;

/// Fewest and most concurrent lit generations (CPU: about a million texels
/// plus a BC1 encode each).
const MIN_LIT_GENERATION_PERMITS: usize = 2;
const MAX_LIT_GENERATION_PERMITS: usize = 4;

// Lit work runs on the blocking pool inside its headroom (R17); keep it to a
// quarter of that headroom so it cannot starve other blocking work.
const _: () = assert!(
    LIT_PROBE_PERMITS + MAX_LIT_GENERATION_PERMITS <= crate::executor::BLOCKING_POOL_HEADROOM / 4
);

/// Concurrent lit generations for a machine with `cores` cores: an eighth of
/// them, 2 to 4.
pub fn lit_generation_permits(cores: usize) -> usize {
    (cores / 8).clamp(MIN_LIT_GENERATION_PERMITS, MAX_LIT_GENERATION_PERMITS)
}

/// Extra attempts at a recipe read that failed with an I/O error before the
/// tile is given up on for the session.
const RECIPE_RETRIES: u32 = 2;
const RECIPE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Calls `attempt` until it succeeds, at most `retries + 1` times.
fn with_retries<T, E>(
    retries: u32,
    delay: std::time::Duration,
    mut attempt: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut left = retries;
    loop {
        match attempt() {
            Err(_) if left > 0 => {
                left -= 1;
                std::thread::sleep(delay);
            }
            result => return result,
        }
    }
}

/// A recipe that could not be read, even after retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unreadable;

/// Recipe reads for the lit path: retried, and given up on for the session
/// once the retries run out.
///
/// `RecipeStore` caches no I/O error, so each attempt here really rereads.
/// Once a tile's retries are exhausted it is remembered here, and later
/// reads of it fail at once: the retries, their delay and the warning cost
/// once per tile, not once per footprint.
struct SessionRecipes {
    store: Arc<RecipeStore>,
    given_up: DashSet<(i32, i32)>,
}

impl SessionRecipes {
    fn new(store: Arc<RecipeStore>) -> Self {
        Self {
            store,
            given_up: DashSet::new(),
        }
    }

    /// The tile's recipe (`None` for none), or `Unreadable`. Blocking I/O.
    fn get(&self, lat: i32, lon: i32) -> Result<Option<Arc<Recipe>>, Unreadable> {
        if self.given_up.contains(&(lat, lon)) {
            return Err(Unreadable);
        }
        with_retries(RECIPE_RETRIES, RECIPE_RETRY_DELAY, || {
            self.store.get_checked(lat, lon)
        })
        .map_err(|e| {
            if self.given_up.insert((lat, lon)) {
                warn!(lat, lon, error = %e,
                      "Night lighting: recipe unreadable after retries; tile stays dark this session");
            }
            Unreadable
        })
    }

    /// Every recipe the generator can read for `footprint`, or `Unreadable`
    /// when one of them cannot be read. Blocking I/O.
    fn for_footprint(&self, footprint: &Footprint) -> Result<FootprintRecipes, Unreadable> {
        footprint
            .expanded(FIELD_MARGIN_DEG)
            .tiles()
            .into_iter()
            .map(|(lat, lon)| Ok(((lat, lon), self.get(lat, lon)?)))
            .collect()
    }
}

/// The recipes of the tiles a footprint's field covers.
type FootprintRecipes = HashMap<(i32, i32), Option<Arc<Recipe>>>;

/// Memory for generated visible levels (about 0.7 MB per urban ZL16 texture).
const LIT_PAYLOAD_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Default overall brightness (`night.brightness`).
pub const DEFAULT_NIGHT_BRIGHTNESS: f64 = 1.0;

/// The night lit source: full-size, correctly laid out textures where the
/// recipe and the user's art say there is light (generated on first read of
/// the visible levels, coalesced across concurrent readers, with permits
/// following the core count); the minimal black DDS everywhere else.
pub struct NightLitSource {
    recipes: Arc<SessionRecipes>,
    art: Arc<LaminarArt>,
    setting: Arc<dyn TextureSettingSource>,
    /// Session-stable answers. Never evicts: an evicted entry could be
    /// re-probed to a different answer, and a lit file's size must not change
    /// while the kernel may hold the old one (R7). One byte-sized entry per
    /// footprint ever asked about.
    lit: DashMap<(u32, u32, u8), bool>,
    probes: tokio::sync::Semaphore,
    /// Generated visible levels by footprint. Evicting is safe: a payload
    /// regenerates to the same bytes (R13), or to none (black) once one of its
    /// recipes has been given up on, and never changes the size. A payload is
    /// only built from a complete set of recipes.
    payloads: moka::future::Cache<(u32, u32, u8), Bytes>,
    generations: tokio::sync::Semaphore,
    brightness: f64,
    generator: Arc<dyn LitGenerator>,
}

impl NightLitSource {
    pub fn new(
        store: Arc<RecipeStore>,
        art: Arc<LaminarArt>,
        setting: Arc<dyn TextureSettingSource>,
    ) -> Self {
        Self {
            recipes: Arc::new(SessionRecipes::new(store)),
            art,
            setting,
            lit: DashMap::new(),
            probes: tokio::sync::Semaphore::new(LIT_PROBE_PERMITS),
            payloads: moka::future::Cache::builder()
                .max_capacity(LIT_PAYLOAD_CACHE_BYTES)
                .weigher(|_, v: &Bytes| v.len().try_into().unwrap_or(u32::MAX))
                .build(),
            generations: tokio::sync::Semaphore::new(lit_generation_permits(
                std::thread::available_parallelism().map_or(1, |n| n.get()),
            )),
            brightness: DEFAULT_NIGHT_BRIGHTNESS,
            generator: Arc::new(CpuLitGenerator),
        }
    }

    /// Overall lit brightness (`night.brightness`, 0.5 to 1.5 in config).
    pub fn with_brightness(mut self, brightness: f64) -> Self {
        self.brightness = brightness;
        self
    }

    /// Use `generator` for lit pixels (tests, or another implementation).
    pub fn with_generator(mut self, generator: Arc<dyn LitGenerator>) -> Self {
        self.generator = generator;
        self
    }

    /// The generated visible levels of a lit footprint, or `None` (black)
    /// when generation fails or a recipe cannot be read. Concurrent callers
    /// for one key share one generation; a failure is not cached.
    async fn payload(&self, name: &DdsFilename, layout: &LitLayout) -> Option<Bytes> {
        let key = (name.row, name.col, name.zoom);
        self.payloads
            .optionally_get_with(key, self.generate(name.clone(), layout.first_visible))
            .await
    }

    async fn generate(&self, name: DdsFilename, size: u32) -> Option<Bytes> {
        let _permit = self.generations.acquire().await.ok()?;
        let started = Instant::now();
        let (recipes, art, generator, owned, size, brightness) = (
            Arc::clone(&self.recipes),
            Arc::clone(&self.art),
            Arc::clone(&self.generator),
            name.clone(),
            size,
            self.brightness,
        );
        let generated = tokio::task::spawn_blocking(move || {
            // All recipes first: a payload missing one would be cached and
            // differ from its regeneration (R13).
            let Ok(fetched) = recipes.for_footprint(&Footprint::from_name(&owned)) else {
                return None;
            };
            let lookup = |lat: i32, lon: i32| fetched.get(&(lat, lon)).cloned().flatten();
            let patterns = |n: &str| art.pattern(n);
            let image = generator.generate(&LitRequest {
                name: &owned,
                size,
                brightness,
                recipes: &lookup,
                patterns: &patterns,
            });
            Some(super::generator::encode_visible(image))
        })
        .await;
        match generated {
            Ok(None) => {
                // Warned once, when the tile was given up on.
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    "Night lighting: a recipe is unreadable; texture served black"
                );
                None
            }
            Ok(Some(Ok(payload))) => {
                debug!(
                    row = name.row,
                    col = name.col,
                    zoom = name.zoom,
                    ms = started.elapsed().as_millis() as u64,
                    "Night lighting: lit texture generated"
                );
                Some(payload)
            }
            Ok(Some(Err(e))) => {
                warn!(error = %e, "Night lighting: lit encode failed; texture served black");
                None
            }
            Err(_) => {
                warn!("Night lighting: lit generation failed; texture served black");
                None
            }
        }
    }

    async fn has_light(&self, name: &DdsFilename) -> bool {
        if !SUPPORTED_LIT_ZOOMS.contains(&name.zoom) {
            return false;
        }
        let key = (name.row, name.col, name.zoom);
        if let Some(hit) = self.lit.get(&key) {
            return *hit;
        }
        let started = Instant::now();
        let Ok(_permit) = self.probes.acquire().await else {
            return false;
        };
        if let Some(hit) = self.lit.get(&key) {
            return *hit; // another probe answered while we waited
        }
        let wait_ms = started.elapsed().as_millis() as u64;
        let probe_started = Instant::now();
        let (recipes, art) = (Arc::clone(&self.recipes), Arc::clone(&self.art));
        let footprint = Footprint::from_name(name);
        let lit = match tokio::task::spawn_blocking(move || {
            // The field reads every tile the generator could read for this
            // footprint, once each; an unreadable one counts as dark.
            let lookup = |lat: i32, lon: i32| recipes.get(lat, lon).ok().flatten();
            RecipeField::new(&footprint, &lookup).has_light(|n| art.lit_terrain(n).is_some())
        })
        .await
        {
            Ok(answer) => answer,
            Err(_) => {
                // The probe is meant to be panic-free: XEL's panic hook
                // unmounts on any panic. This arm is a last resort, and it
                // remembers the dark answer so size and read keep agreeing.
                warn!("Night lighting: lit probe failed; footprint stays dark this session");
                false
            }
        };
        let lit = self.remember(key, lit);
        debug!(
            row = name.row,
            col = name.col,
            zoom = name.zoom,
            lit,
            wait_ms,
            probe_ms = probe_started.elapsed().as_millis() as u64,
            "Night lighting: lit probe"
        );
        lit
    }

    /// Commits a probe answer and returns the committed one: the first
    /// answer for a key wins, so two probes that finish with different
    /// answers cannot give `size` and `read` different lengths (R7).
    fn remember(&self, key: (u32, u32, u8), lit: bool) -> bool {
        *self.lit.entry(key).or_insert(lit)
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
                let layout = self.layout(name);
                // Header and blank levels need no pixels; only a read that
                // reaches the visible levels generates them.
                if offset >= layout.total_len
                    || offset.saturating_add(u64::from(len)) <= layout.payload_start()
                {
                    return layout.read_black(offset, len);
                }
                match self.payload(name, &layout).await {
                    Some(payload) => layout.read_lit(offset, len, &payload),
                    None => layout.read_black(offset, len),
                }
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

    #[test]
    fn retries_stop_at_the_first_success() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, std::time::Duration::ZERO, || {
            calls += 1;
            if calls < 2 {
                Err("transient")
            } else {
                Ok(7)
            }
        });
        assert_eq!((r, calls), (Ok(7), 2));
    }

    #[test]
    fn retries_give_up_after_the_limit() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, std::time::Duration::ZERO, || {
            calls += 1;
            Err("down")
        });
        assert_eq!((r, calls), (Err("down"), 3));
    }

    #[test]
    fn generation_permits_follow_the_core_count() {
        assert_eq!(lit_generation_permits(4), 2);
        assert_eq!(lit_generation_permits(24), 3);
        assert_eq!(lit_generation_permits(64), 4);
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
            // A 4 x 4 solid red DXT1 lit texture.
            let mut dds =
                crate::dds::DdsHeader::new(4, 4, 1, crate::dds::DdsFormat::BC1).to_bytes();
            dds.extend_from_slice(&[0x00, 0xF8, 0, 0, 0, 0, 0, 0]);
            std::fs::write(art.join("textures12/urban_FAR/uw5_FAR_LIT.dds"), dds).unwrap();

            let source = NightLitSource::new(
                Arc::new(RecipeStore::new(vec![pkg])),
                Arc::new(LaminarArt::new(art)),
                Arc::new(FixedTextureSetting(4)),
            );
            (temp, source)
        }

        /// The ZL16 texture name whose footprint contains (lat, lon).
        fn name_at(lat: f64, lon: f64) -> DdsFilename {
            name_at_zoom(lat, lon, 16)
        }

        fn name_at_zoom(lat: f64, lon: f64, zoom: u8) -> DdsFilename {
            let n = f64::from(1u32 << zoom);
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let lat_r = lat.to_radians();
            let y = ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0
                * n) as u32;
            DdsFilename {
                row: y / 16 * 16,
                col: x / 16 * 16,
                zoom,
                map_type: "BI".into(),
                kind: TextureKind::Lit,
            }
        }

        /// Makes the fixture's recipe unreadable; false when running as root,
        /// where permissions do not apply.
        #[cfg(unix)]
        fn set_recipe_readable(temp: &TempDir, readable: bool) -> bool {
            use std::os::unix::fs::PermissionsExt;
            let path = crate::night::recipe_path(&temp.path().join("pkg"), 37, -123);
            let mode = if readable { 0o644 } else { 0o000 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            readable || std::fs::File::open(&path).is_err()
        }

        #[test]
        fn the_first_committed_probe_answer_wins() {
            // Two probes of one footprint can finish with different answers
            // (one recipe read failing after its retries); whichever commits
            // first is the answer for everyone, so size and read agree (R7).
            let (_t, source) = fixture();
            let key = (1, 2, 16);
            assert!(source.remember(key, true));
            assert!(source.remember(key, false), "the later answer is dropped");
            assert!(source.lit.get(&key).is_some_and(|v| *v));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn a_recipe_read_failure_during_generation_is_not_a_payload() {
            // A payload built without one of its recipes would be cached and
            // differ from a regeneration (R13): generation gives up instead,
            // and the read serves full-size black.
            let (temp, source) = fixture();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            if !set_recipe_readable(&temp, false) {
                return;
            }
            assert_eq!(source.payload(&name, &layout).await, None);
            assert_eq!(source.payloads.entry_count(), 0, "nothing cached");
            source.payloads.run_pending_tasks().await;
            assert!(source
                .payloads
                .get(&(name.row, name.col, name.zoom))
                .await
                .is_none());
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn an_unreadable_tile_is_remembered_for_the_session() {
            // After its retries a tile is given up on for the session, so the
            // retries, their delay and the warning cost once per tile, not
            // once per footprint.
            let (temp, source) = fixture();
            if !set_recipe_readable(&temp, false) {
                return;
            }
            assert_eq!(source.size(&name_at(37.20, -122.725)).await, 152);
            assert!(set_recipe_readable(&temp, true));
            let other = name_at_zoom(37.17, -122.74, 17);
            assert_eq!(source.size(&other).await, 152, "the tile stays given up");
            assert_eq!(source.read(&other, 0, u32::MAX).await, minimal_black_dds());
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
        async fn a_lit_footprint_has_light_in_its_visible_levels() {
            let (_t, source) = fixture();
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert_eq!(
                visible.len() as u64,
                layout.payload_len(),
                "short read at EOF"
            );
            assert!(visible.iter().any(|&b| b != 0), "generated light");
            let blank = source.read(&name, 128, 4096).await;
            assert!(blank.iter().all(|&b| b == 0), "the fade level stays black");
        }

        #[tokio::test]
        async fn zero_brightness_keeps_it_black() {
            let (_t, source) = fixture();
            let source = source.with_brightness(0.0);
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert!(visible.iter().all(|&b| b == 0));
            assert_eq!(source.size(&name).await, layout.total_len, "size unchanged");
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
        async fn a_size_answer_is_remembered_for_the_session() {
            let (temp, source) = fixture();
            let name = name_at(37.20, -122.725);
            assert_eq!(source.size(&name).await, 2_796_344);
            std::fs::remove_file(crate::night::recipe_path(
                &temp.path().join("pkg"),
                37,
                -123,
            ))
            .unwrap();
            assert_eq!(source.size(&name).await, 2_796_344, "memoised");
            assert_eq!(
                source.read(&name, 0, u32::MAX).await.len() as u64,
                2_796_344,
                "read agrees with the memoised size"
            );
        }

        /// A generator that lights every texel white, counting its calls.
        #[derive(Default)]
        struct White(std::sync::atomic::AtomicUsize);

        impl crate::night::generator::LitGenerator for White {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn the_injected_generator_makes_the_payload() {
            let (_t, source) = fixture();
            let white = Arc::new(White::default());
            let source = source.with_generator(white.clone());
            let name = name_at(37.20, -122.725);
            let layout = LitLayout::new(16, 4);
            let visible = source.read(&name, layout.payload_start(), 1 << 20).await;
            assert!(visible.iter().any(|&b| b != 0));
            assert_eq!(white.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        /// Like `White`, but slow enough that concurrent reads overlap.
        #[derive(Default)]
        struct Slow(std::sync::atomic::AtomicUsize);

        impl crate::night::generator::LitGenerator for Slow {
            fn generate(&self, r: &crate::night::generator::LitRequest<'_>) -> image::RgbaImage {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(100));
                image::RgbaImage::from_pixel(r.size, r.size, image::Rgba([255, 255, 255, 255]))
            }
        }

        #[tokio::test]
        async fn concurrent_reads_share_one_generation() {
            let (_t, source) = fixture();
            let white = Arc::new(Slow::default());
            let source = source.with_generator(white.clone());
            let name = name_at(37.20, -122.725);
            let start = LitLayout::new(16, 4).payload_start();
            let reads = (0..8).map(|_| source.read(&name, start, 4096));
            let results = futures::future::join_all(reads).await;
            assert!(results.iter().all(|r| r.iter().any(|&b| b != 0)));
            assert_eq!(white.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn a_footprint_beside_lit_ground_is_lit() {
            // The golden urban block's east edge is lon -122.70. A footprint
            // whose west edge is a little east of it holds no lit cell, but
            // the soft edge reaches into it, so it must not be answered dark.
            // ZL22 texture 1629616_667600: west edge -122.69943.
            let (_t, source) = fixture();
            let name = DdsFilename {
                row: 1_629_616,
                col: 667_600,
                zoom: 22,
                map_type: "BI".into(),
                kind: TextureKind::Lit,
            };
            let f = crate::night::Footprint::from_name(&name);
            assert!(f.west > -122.70 && f.west - -122.70 < 1.0 / 1200.0);
            assert_eq!(source.size(&name).await, LitLayout::new(22, 4).total_len);
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
