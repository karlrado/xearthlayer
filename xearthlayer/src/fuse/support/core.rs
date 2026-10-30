//! The backend-neutral core of the ortho union filesystem.
//!
//! [`OrthoUnionCore`] holds the state and logic that are independent of the
//! kernel it is mounted through: the union index, the DDS generation
//! pipeline, the lit texture source, per-open handle memoisation, the
//! pinned-tile budget, and the gauges around them. `fuse3::Fuse3OrthoUnionFS`
//! is a thin adapter over this core: it owns the kernel identity (inodes)
//! and the FUSE init handshake parameters and delegates everything else.
//! The WinFSP backend (Phase 2) will drive the same core with file contexts
//! instead of inodes.
//!
//! See `docs/dev/windows-backend-split.md` §3.1–§3.3 for the split tables
//! and §4.4 for the seam API.

use bytes::Bytes;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tracing::debug;

use crate::coord::TileCoord;
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::coalesce::RequestCoalescer;
use crate::fuse::support::attrs::{virtual_dds_attrs, FileAttributes, VirtualDdsConfig};
use crate::fuse::support::requestor::DdsRequestor;
use crate::fuse::{get_default_placeholder, parse_dds_filename, DdsFilename, TextureKind};
use crate::geo_index::GeoIndex;
use crate::night::{BlackLitSource, LitTextureSource};
use crate::ortho_union::OrthoUnionIndex;
use crate::pinned_budget::PinnedTileBudget;
use crate::prefetch::{PrefetchStateObserver, TileRequestCallback};

/// Per-open state for one virtual DDS file.
///
/// The kernel caps each FUSE read at 1 MiB, so X-Plane reading one
/// 11.17 MB texture arrives as 12-23 ranged calls. Resolving the tile once
/// per open and slicing it turns those into a single executor round trip.
pub struct DdsHandle {
    /// Which tile this handle refers to.
    pub coords: DdsFilename,
    /// The tile, produced on first read and reused by every later read.
    /// `Bytes`: the handle shares the cache's allocation rather than owning
    /// a copy, and `Bytes::slice` serves a ranged read without allocating.
    pub tile: OnceCell<Bytes>,
}

/// The backend-neutral core of the ortho union filesystem.
///
/// This is the state shared by every backend (FUSE today, WinFSP in Phase
/// 2). It carries no kernel concept: identity is the backend's, and the
/// backend maps its kernel handles onto this core's `fh` numbering and
/// [`DdsHandle`] memoisation.
pub struct OrthoUnionCore {
    /// Union index mapping virtual paths to real file locations
    index: Arc<OrthoUnionIndex>,
    /// Geospatial reference database for region-level ownership queries.
    ///
    /// Used to determine if a scenery file falls within a patch-owned region.
    /// When set, FUSE filters lazy resolution to only serve patch sources in
    /// those regions, hiding package files that would cause X-Plane conflicts.
    geo_index: Option<Arc<GeoIndex>>,
    /// Observer for prefetch state divergence (#176).
    state_observer: Option<Arc<PrefetchStateObserver>>,
    /// Client for DDS generation requests (new daemon architecture)
    dds_client: Arc<dyn DdsClient>,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
    /// Limiter for concurrent disk I/O operations
    disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    /// Optional callback for tile request tracking
    tile_request_callback: Option<TileRequestCallback>,
    /// Request coalescer for deduplicating concurrent requests
    request_coalescer: Arc<RequestCoalescer>,
    /// Source of lit textures (`<stem>_LIT.dds`). Lit inodes never reach the
    /// DDS client, coalescer or caches above (night lighting, R8). Defaults
    /// to [`BlackLitSource`], which serves the minimal black DDS.
    lit_source: Arc<dyn LitTextureSource>,
    /// Per-open state for virtual DDS files, keyed by file handle.
    dds_handles: DashMap<u64, Arc<DdsHandle>>,
    /// Source of file handles. Starts at 1: 0 means "no handle".
    next_fh: AtomicU64,
    /// Bytes of DDS tiles currently pinned by open handles, and the ceiling.
    ///
    /// Shared rather than private because prefetch reads the same figure as its
    /// yield signal (#246). See [`PinnedTileBudget`].
    pinned: Arc<PinnedTileBudget>,
    /// Highest concurrent handle count seen. The current count drains to near
    /// zero between bursts, so the peak is what the pinned ceiling has to be
    /// sized against.
    peak_handles_open: AtomicU64,
    /// Whether the budget-exhausted warning has been emitted this session.
    ///
    /// Latched, because the cap engages for every open in a burst rather than
    /// once: an unlatched warn would bury the log it exists to serve. The
    /// per-occurrence count goes to metrics instead (#236).
    budget_warning_logged: AtomicBool,
    /// Optional metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics.
    metrics_client: Option<crate::metrics::MetricsClient>,
}

impl OrthoUnionCore {
    /// Create a new consolidated ortho union core.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::with_defaults(
            "ortho_union_disk_io",
        ));
        let request_coalescer = Arc::new(RequestCoalescer::new());
        debug!(
            max_concurrent = disk_io_limiter.max_concurrent(),
            sources = index.source_count(),
            files = index.file_count(),
            "Consolidated ortho union FUSE filesystem initialized"
        );

        Self {
            index: Arc::new(index),
            geo_index: None,
            state_observer: None,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            lit_source: Arc::new(BlackLitSource),
            dds_handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
            pinned: Arc::new(PinnedTileBudget::new()),
            peak_handles_open: AtomicU64::new(0),
            budget_warning_logged: AtomicBool::new(false),
            metrics_client: None,
        }
    }

    /// Create with custom disk I/O limiter.
    ///
    /// Use this when you need to share a disk I/O limiter across multiple
    /// filesystems or customize the concurrency limits.
    pub fn with_disk_io_limiter(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
        disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    ) -> Self {
        let request_coalescer = Arc::new(RequestCoalescer::new());

        Self {
            index: Arc::new(index),
            geo_index: None,
            state_observer: None,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            lit_source: Arc::new(BlackLitSource),
            dds_handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
            pinned: Arc::new(PinnedTileBudget::new()),
            peak_handles_open: AtomicU64::new(0),
            budget_warning_logged: AtomicBool::new(false),
            metrics_client: None,
        }
    }

    /// Share the pinned-tile budget with the prefetch backpressure signal.
    ///
    /// Without this the filesystem accounts against a budget only it can see,
    /// which is correct for the memoisation cap but leaves prefetch blind to
    /// demand on the mount (#246). Passing the runtime's budget makes the same
    /// number answer both questions.
    pub fn with_pinned_budget(mut self, budget: Arc<PinnedTileBudget>) -> Self {
        self.pinned = budget;
        self
    }

    /// Serve lit textures from `source` instead of the black default.
    pub fn with_lit_source(mut self, source: Arc<dyn LitTextureSource>) -> Self {
        self.lit_source = source;
        self
    }

    /// Set the timeout for DDS generation.
    ///
    /// After this timeout, a placeholder texture is returned to prevent
    /// X-Plane from blocking indefinitely.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Set the callback for tile request tracking.
    ///
    /// This callback is invoked whenever a tile is requested, enabling
    /// the prefetch system to infer aircraft position from FUSE requests.
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.tile_request_callback = Some(callback);
        self
    }

    /// Set the metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics
    /// to the metrics system.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
        self
    }

    /// Set the geospatial reference index for region-level ownership queries.
    ///
    /// When set, FUSE uses the GeoIndex to determine if a scenery file falls
    /// within a patch-owned DSF region. Files in patched regions are resolved
    /// only from patch sources, hiding package files that could conflict.
    pub fn with_geo_index(mut self, geo_index: Arc<GeoIndex>) -> Self {
        self.geo_index = Some(geo_index);
        self
    }

    /// Set the prefetch state observer.
    ///
    /// When set, an on-demand generation in a region prefetch marked
    /// `Prefetched` or `NoCoverage` demotes that region.
    pub fn with_state_observer(mut self, observer: Arc<PrefetchStateObserver>) -> Self {
        self.state_observer = Some(observer);
        self
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the ortho union index.
    pub fn index(&self) -> &OrthoUnionIndex {
        &self.index
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        &self.disk_io_limiter
    }

    /// The pinned-tile budget shared with the prefetch backpressure signal.
    pub fn pinned(&self) -> &Arc<PinnedTileBudget> {
        &self.pinned
    }

    /// The lit texture source.
    pub fn lit_source(&self) -> &Arc<dyn LitTextureSource> {
        &self.lit_source
    }

    /// Per-open virtual-DDS handle state, keyed by file handle.
    pub fn dds_handles(&self) -> &DashMap<u64, Arc<DdsHandle>> {
        &self.dds_handles
    }

    /// The virtual DDS attribute configuration.
    pub fn virtual_dds_config(&self) -> &VirtualDdsConfig {
        &self.virtual_dds_config
    }

    /// Highest concurrent open handle count seen.
    pub fn peak_dds_handles(&self) -> u64 {
        self.peak_handles_open.load(Ordering::Relaxed)
    }

    /// Highest pinned tile byte total seen.
    pub fn peak_pinned_tile_bytes(&self) -> u64 {
        self.pinned.peak_bytes()
    }

    /// Number of virtual DDS files currently open.
    pub fn open_dds_handles(&self) -> usize {
        self.dds_handles.len()
    }

    /// Bytes of DDS tiles currently pinned by open handles.
    pub fn pinned_tile_bytes(&self) -> u64 {
        self.pinned.pinned_bytes()
    }

    // =========================================================================
    // Neutral operations (moved verbatim from the FUSE adapter)
    // =========================================================================

    /// Check if a scenery filename falls in a geo-filtered (patch-owned) region.
    ///
    /// Returns true when the file's DSF region has [`PatchCoverage`] in the
    /// GeoIndex, meaning only patch sources should serve files for this region.
    fn is_geo_filtered(&self, filename: &str) -> bool {
        let geo_index = match self.geo_index {
            Some(ref gi) => gi,
            None => return false,
        };

        use crate::geo_index::{DsfRegion, PatchCoverage};
        use crate::prefetch::tile_based::DsfTileCoord;

        DsfTileCoord::from_scenery_filename(filename)
            .map(|dsf| {
                let region = DsfRegion::new(dsf.lat, dsf.lon);
                geo_index.contains::<PatchCoverage>(&region)
            })
            .unwrap_or(false)
    }

    /// Resolve a lazy path with geospatial awareness.
    ///
    /// If the file is in a patch-owned region (per GeoIndex), patch sources are
    /// searched first. If the file isn't found in patches, all sources are tried
    /// as fallback — this handles cross-region DSF references where a non-patched
    /// DSF references terrain (e.g., `_sea.ter`) in a patched region.
    ///
    /// DDS generation blocking is handled separately by the passthrough gate in
    /// `lookup()`, not here.
    ///
    /// This is the composition point: GeoIndex (geography) + OrthoUnionIndex (files).
    pub fn resolve_lazy_geo(&self, virtual_path: &Path, filename: &str) -> Option<PathBuf> {
        if self.is_geo_filtered(filename) {
            // Patched region: patches first, packages fill gaps
            self.index
                .resolve_lazy_filtered(virtual_path, |s| s.is_patch())
                .or_else(|| self.index.resolve_lazy(virtual_path))
        } else {
            // Normal: all sources
            self.index.resolve_lazy(virtual_path)
        }
    }

    /// Produce a whole DDS tile.
    ///
    /// Called once per open when the handle memoises, once per read when it
    /// does not.
    pub async fn resolve_tile(&self, coords: &DdsFilename) -> Bytes {
        // request_dds parses a filename; Display carries the correct zoom.
        self.request_dds(&format!("{}.dds", coords))
            .await
            .unwrap_or_else(get_default_placeholder)
    }

    /// Request DDS generation by filename string.
    ///
    /// Wrapper around the trait method that parses the filename first.
    async fn request_dds(&self, name_str: &str) -> Option<Bytes> {
        let coords = parse_dds_filename(name_str).ok()?;
        Some(self.request_dds_impl(&coords).await)
    }

    /// Neutral attributes for a virtual DDS, by kind.
    ///
    /// Ortho textures report the configured expected size; lit textures
    /// report the lit source's size, which is what the source will serve
    /// (night lighting R4/R7). The backend maps the result onto its kernel
    /// attribute type (FUSE adds the inode number and the macOS-only fields).
    pub async fn virtual_attrs_for(&self, coords: &DdsFilename) -> FileAttributes {
        let mut attrs = virtual_dds_attrs(&self.virtual_dds_config);
        if coords.kind == TextureKind::Lit {
            let size = self.lit_source.size(coords).await;
            attrs.size = size;
            attrs.blocks = size.div_ceil(u64::from(attrs.blksize));
        }
        attrs
    }

    /// Allocate the next file handle and register a fresh per-open DDS
    /// handle under it.
    ///
    /// Handle numbering is backend-agnostic: just a `u64` key into
    /// [`Self::dds_handles`] starting at 1, so `0` remains "no handle".
    pub fn new_dds_handle(&self, coords: DdsFilename) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dds_handles.insert(
            fh,
            Arc::new(DdsHandle {
                coords,
                tile: OnceCell::new(),
            }),
        );
        fh
    }

    /// Publish the handle gauge. Called on open and release -- two events per
    /// texture, against the 12-23 reads it takes to serve one.
    pub fn report_handle_gauge(&self) {
        let open = self.dds_handles.len() as u64;
        let pinned = self.pinned.pinned_bytes();
        self.peak_handles_open.fetch_max(open, Ordering::Relaxed);

        if let Some(metrics) = &self.metrics_client {
            metrics.fuse_handles(
                open,
                pinned,
                self.peak_handles_open.load(Ordering::Relaxed),
                self.pinned.peak_bytes(),
            );
        }
    }

    /// Claims the one-shot budget warning: true on the first call of the
    /// session, false ever after.
    ///
    /// The cap engages for every open in a burst, not once, so the warning is
    /// latched and the per-occurrence count goes to metrics instead (#236).
    pub fn claim_budget_warning(&self) -> bool {
        !self.budget_warning_logged.swap(true, Ordering::Relaxed)
    }

    /// Record one FUSE `read()` call for the amplification metric.
    ///
    /// `returned` is what this call hands back to the kernel; `materialised`
    /// is what the handler had to obtain to produce it. The kernel caps each read
    /// at 1 MiB, so a handler that builds the whole object per call inflates
    /// the second against the first. See #233 and #234.
    pub fn record_read(&self, returned: u64, materialised: u64, virtual_dds: bool) {
        if let Some(metrics) = &self.metrics_client {
            metrics.fuse_read(returned, materialised, virtual_dds);
        }
    }
}

impl DdsRequestor for OrthoUnionCore {
    fn dds_client(&self) -> &Arc<dyn DdsClient> {
        &self.dds_client
    }

    fn generation_timeout(&self) -> Duration {
        self.generation_timeout
    }

    fn context_label(&self) -> &'static str {
        "ortho_union"
    }

    fn tile_request_callback(&self) -> Option<&TileRequestCallback> {
        self.tile_request_callback.as_ref()
    }

    fn request_coalescer(&self) -> Option<&Arc<RequestCoalescer>> {
        Some(&self.request_coalescer)
    }

    fn metrics_client(&self) -> Option<&crate::metrics::MetricsClient> {
        self.metrics_client.as_ref()
    }

    fn on_dds_response(&self, tile: TileCoord, cache_hit: bool) {
        if let Some(ref observer) = self.state_observer {
            observer.observe(tile, cache_hit);
        }
    }
}
