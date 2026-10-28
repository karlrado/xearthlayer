//! The Windows (WinFSP) backend for the ortho union filesystem (Phase 2,
//! design `docs/dev/winfsp-backend.md` §4.1).
//!
//! Split by testability, not just by platform:
//!
//! - [`error`] — the `FsError → io::Error` half of the backend error seam.
//! - [`resolve`] — driver-path → components, and the component walk over a
//!   per-name lookup.
//!
//!   Both are pure and std-only (no `winfsp` types), so CI unit-tests them
//!   on every OS even though their caller is Windows-only (§7.6: the
//!   Windows-gated module stays outside the Linux build graph by design,
//!   while the pure helpers it extracts are tested where they live).
//!
//! - [`context`] — the `FileSystemContext` + `AsyncFileSystemContext` trait
//!   impls over the neutral core. Windows-only: the `winfsp` crate is a
//!   Windows dependency (§4.6).
//!
//! - [`WinFspOrthoUnionFS`] — the builder, and [`WinFspSession`] — the
//!   live-mount handle that owns the unmount (§4.2/§4.5). Windows-only.
#![allow(dead_code)]

pub(crate) mod error;
pub(crate) mod resolve;

#[cfg(windows)]
mod context;

#[cfg(windows)]
use context::OrthoUnionContext;
#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use winfsp::host::{FileSystemHost, VolumeParams};
#[cfg(windows)]
use winfsp::service::FileSystemService;
#[cfg(windows)]
use winfsp::service::FileSystemServiceBuilder;
#[cfg(windows)]
use winfsp::FspError;

/// The filesystem name the volume reports to the OS (Explorer's "File
/// system" column, and the `GetVolumeInformationW` answer for paths on the
/// volume): the WinFSP kernel reports a custom `filesystem_name` verbatim
/// (WinFSP 2.x `src/sys/volinfo.c`), so this is exactly what
/// `system/unmount.rs` matches when asking "is something WinFSP mounted at
/// this path".
///
/// Hard cap: `FSP_SERVICE_PARAMS.FileSystemName` is `[WCHAR; 16]` — keep
/// this at 15 characters or fewer (NUL-terminated). Longer names must
/// never reach `VolumeParams::filesystem_name`: winfsp 0.13.2's setter
/// bounds against 192 and panics for a 17-char name — inside the
/// service's `on_start` thread, which cannot unwind, so the process
/// aborts (hit during the commit-5 smoke run).
#[cfg(windows)]
pub const ORTHO_VOLUME_FS_NAME: &str = "XEarthLayer";

/// The error from [`WinFspOrthoUnionFS::mount`].
#[cfg(windows)]
#[derive(Debug)]
pub enum MountError {
    /// A WinFSP call failed (runtime absent, service creation, host start,
    /// or mount-point collision).
    Winfsp(FspError),
    /// The mount was not configured with a tokio runtime handle (see
    /// [`WinFspOrthoUnionFS::with_runtime_handle`]).
    NoRuntime,
    /// The service worker never reported a completed mount within the
    /// startup window (its start callback stalled or panicked).
    MountNotReady,
}

#[cfg(windows)]
impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Winfsp(e) => write!(f, "WinFSP error: {e}"),
            Self::NoRuntime => write!(
                f,
                "no tokio runtime handle configured (call with_runtime_handle before mount)"
            ),
            Self::MountNotReady => write!(f, "mount did not become ready"),
        }
    }
}

#[cfg(windows)]
impl std::error::Error for MountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Winfsp(e) => Some(e),
            Self::NoRuntime | Self::MountNotReady => None,
        }
    }
}

/// The filesystem context the service's start callback produces: the host
/// with its dispatcher running and mounted, and nothing else.
#[cfg(windows)]
struct MountedFs {
    host: FileSystemHost<OrthoUnionContext>,
}

/// `FileSystemService` wraps a raw C handle (`NonNull<FSP_SERVICE>`) and so
/// is neither `Send` nor `Sync` by construction, which would make an `Arc`
/// of it useless for sharing (clippy's `arc_with_non_send_sync`): a
/// single-threaded accessor is safe to move between threads, so the newtype
/// re-asserts that.
#[cfg(windows)]
struct ServiceBox(FileSystemService<MountedFs>);

// SAFETY: every access to the inner service is serialized by the `Mutex`
// around `ServiceBox` — `unmount` is the only entry point and holds the
// lock across `stop` + `join` (the WinFSP service API is callable from any
// thread), and the start/stop callbacks the service runs never touch this
// handle.
#[cfg(windows)]
unsafe impl Send for ServiceBox {}

#[cfg(windows)]
unsafe impl Sync for ServiceBox {}

/// A handle to a running WinFSP mount (design §4.2/§4.5).
///
/// Keeps the service worker alive and owns the unmount. [`unmount`](Self::unmount)
/// is idempotent and safe from any thread — including the panic handler —
/// and the last dropped handle tears the mount down anyway, so a forgotten
/// unmount cannot leak it (the FUSE `MountHandle` Drop does the equivalent).
#[cfg(windows)]
#[derive(Clone)]
pub struct WinFspSession(Arc<Mutex<ServiceBox>>);

#[cfg(windows)]
impl WinFspSession {
    fn new(service: FileSystemService<MountedFs>) -> Self {
        Self(Arc::new(Mutex::new(ServiceBox(service))))
    }

    /// Unmount and stop the service. Idempotent: WinFSP's stop is a guarded
    /// no-op after the first stop, and the host's `unmount`/`stop` are safe
    /// to repeat (the crate's Drop relies on the same).
    pub fn unmount(&self) {
        let mut service = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        service.0.stop();
        let _ = service.0.join();
    }
}

#[cfg(windows)]
impl std::fmt::Debug for WinFspSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinFspSession").finish_non_exhaustive()
    }
}

#[cfg(windows)]
impl Drop for WinFspSession {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// The WinFSP backend for the ortho union filesystem: the builder and the
/// Windows mount point of the production flow (design §4.2).
///
/// Mirrors [`Fuse3OrthoUnionFS`]'s surface (minus the FUSE-only knobs —
/// `with_fuse_limits` has no WinFSP equivalent; WinFSP bounds its own queue
/// via `IrpCapacity`, §4.2) so the call site in `manager/mounts.rs` reads
/// the same on both platforms. It owns the neutral core and the runtime
/// handle, exactly as the FUSE adapter owns its core (§4.2).
///
/// ```ignore
/// let session = WinFspOrthoUnionFS::new(index, dds_client, expected_dds_size)
///     .with_geo_index(geo_index)
///     .with_state_observer(observer)
///     .with_pinned_budget(budget)
///     .with_lit_source(source)
///     .with_metrics(metrics)
///     .with_timeout(Duration::from_secs(generation_timeout))
///     .with_runtime_handle(service.runtime_handle().clone())
///     .mount(mountpoint)?;
/// ```
#[cfg(windows)]
pub struct WinFspOrthoUnionFS {
    core: crate::fuse::support::core::OrthoUnionCore,
    /// The tokio runtime async reads are dispatched onto (design §3.3/§4.2).
    runtime: Option<tokio::runtime::Handle>,
}

#[cfg(windows)]
impl WinFspOrthoUnionFS {
    /// Build the backend from the same inputs the FUSE adapter takes.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(
        index: crate::ortho_union::OrthoUnionIndex,
        dds_client: Arc<dyn crate::executor::DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        Self {
            core: crate::fuse::support::core::OrthoUnionCore::new(
                index,
                dds_client,
                expected_dds_size,
            ),
            runtime: None,
        }
    }

    /// Account pinning against the runtime's shared budget (#246), as the
    /// FUSE adapter's `with_pinned_budget` does.
    pub fn with_pinned_budget(
        mut self,
        budget: Arc<crate::pinned_budget::PinnedTileBudget>,
    ) -> Self {
        self.core = self.core.with_pinned_budget(budget);
        self
    }

    /// Serve lit textures from `source` instead of the black default.
    pub fn with_lit_source(mut self, source: Arc<dyn crate::night::LitTextureSource>) -> Self {
        self.core = self.core.with_lit_source(source);
        self
    }

    /// Set the timeout for DDS generation.
    ///
    /// After this timeout, a placeholder texture is returned to prevent
    /// X-Plane from blocking indefinitely. This is the strict bound on read
    /// latency: WinFSP's `IrpTimeout` only bounds dispatcher pickup (spike
    /// F2/F4), so this stays well below the 5-minute ceiling we set.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.core = self.core.with_timeout(timeout);
        self
    }

    /// Report coalesced requests and other FUSE-level metrics to the metrics
    /// system, as the FUSE adapter's `with_metrics` does.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.core = self.core.with_metrics(metrics);
        self
    }

    /// Resolve files in patch-owned DSF regions from patch sources only,
    /// as the FUSE adapter's `with_geo_index` does.
    pub fn with_geo_index(mut self, geo_index: Arc<crate::geo_index::GeoIndex>) -> Self {
        self.core = self.core.with_geo_index(geo_index);
        self
    }

    /// Demote regions whose prefetch claim an on-demand generation
    /// contradicts, as the FUSE adapter's `with_state_observer` does.
    pub fn with_state_observer(
        mut self,
        observer: Arc<crate::prefetch::PrefetchStateObserver>,
    ) -> Self {
        self.core = self.core.with_state_observer(observer);
        self
    }

    /// The tokio runtime that async reads are dispatched onto (design §3.3).
    ///
    /// Required before [`Self::mount`]: the async read path spawns its tasks
    /// there, keeping the WinFSP dispatcher threads free while a tile
    /// generation is in flight. In production this is the service's runtime
    /// handle (the same one the FUSE flow blocks on).
    pub fn with_runtime_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Mount the filesystem at `mountpoint` and return the live handle.
    ///
    /// `mountpoint` is a **directory that must not exist yet**: WinFSP
    /// creates it on mount and removes it on unmount; a pre-existing name
    /// fails with `STATUS_OBJECT_NAME_COLLISION` (spike F1).
    ///
    /// Consumes the builder (like the FUSE adapter's `mount`), so the core
    /// is handed to the service exactly once.
    pub fn mount(self, mountpoint: &Path) -> Result<WinFspSession, MountError> {
        let runtime = self.runtime.ok_or(MountError::NoRuntime)?;

        // Resolve the WinFSP runtime now (standard search path first, then
        // the installed `HKLM\SOFTWARE\WinFsp\InstallDir` — option A): the
        // exe delay-loads the DLL, so starting is always fine, but a mount
        // where WinFSP is absent fails here with a clean error instead of
        // inside the driver.
        let init = winfsp::winfsp_init().map_err(MountError::Winfsp)?;

        let core = Arc::new(self.core);
        let mountpoint = mountpoint.to_path_buf();
        // The start callback reports its result to the mounting thread.
        // `FspServiceLoop` returns success even when the service's start
        // callback failed (the C service reports that as a SERVICE_STOPPED
        // state, not an NTSTATUS — verified in WinFSP src/dll/service.c,
        // 2026-10-28), so `join` cannot surface the failure and the
        // callback's own report is authoritative instead.
        let (report_tx, report_rx) = std::sync::mpsc::channel::<Result<(), MountError>>();
        // The start closure consumes its copy; `mountpoint` itself survives
        // for the log line below.
        let mountpoint_for_service = mountpoint.clone();

        let mut service = FileSystemServiceBuilder::new()
            .with_start(move || {
                // `new_async` selects the async read interface (design
                // §4.5); the default `FineGuard` strategy allows concurrent
                // callbacks on dispatcher threads, which is what the core's
                // internal synchronization is built for (§3.1). The closure
                // reports in the service's own error space (`FspError`).
                let mut host: FileSystemHost<OrthoUnionContext> = FileSystemHost::new_async(
                    volume_params(),
                    OrthoUnionContext::new(Arc::clone(&core), runtime.clone()),
                )?;
                host.start()?;
                host.mount(mountpoint_for_service.as_os_str())?;
                // Report success *after* the mount is established — the
                // driver created the mount-point directory by now.
                let _ = report_tx.send(Ok(()));
                Ok(MountedFs { host })
            })
            .with_stop(move |maybe_fs: Option<&mut MountedFs>| {
                // `None` when the start callback never succeeded — nothing
                // to tear down in that case.
                if let Some(fs) = maybe_fs {
                    fs.host.unmount();
                    fs.host.stop();
                }
                Ok(())
            })
            .build(service_name(), init)
            .map_err(MountError::Winfsp)?;

        // Run the service in console mode: the worker thread starts the
        // filesystem (the start callback above) and then parks until
        // `stop`.
        service.start().map_err(MountError::Winfsp)?;

        // Await the start callback's report (see the channel above).
        match report_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return teardown_failed(&mut service, e),
            Err(_) => return teardown_failed(&mut service, MountError::MountNotReady),
        }

        tracing::info!(mountpoint = %mountpoint.display(), "WinFSP mount established");
        Ok(WinFspSession::new(service))
    }
}

/// Tear down a service whose mount failed and surface the real error.
#[cfg(windows)]
fn teardown_failed(
    service: &mut FileSystemService<MountedFs>,
    error: MountError,
) -> Result<WinFspSession, MountError> {
    service.stop();
    let _ = service.join();
    Err(error)
}

/// The volume parameters for the ortho union mount (design §4.4, spike F2):
/// 512-byte sectors, the volume label, and an explicit 5-minute
/// `IrpTimeout` — a dispatcher-pickup liveness guard, not a generation
/// ceiling; the core's 10-second timeout remains the strict read-latency
/// bound.
#[cfg(windows)]
fn volume_params() -> VolumeParams {
    let mut params = VolumeParams::new();
    params
        .filesystem_name(ORTHO_VOLUME_FS_NAME)
        .sector_size(512)
        .irp_timeout(300_000);
    params
}

/// A unique service name per mount in this process — WinFSP allows one
/// service loop per process, and a stable prefix keeps our mounts
/// recognizable in service logs and the event log.
#[cfg(windows)]
fn service_name() -> String {
    static MOUNT_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = MOUNT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("xearthlayer_ortho_{}_{}", std::process::id(), seq)
}
