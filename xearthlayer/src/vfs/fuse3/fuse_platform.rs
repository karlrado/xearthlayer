//! Per-platform FUSE behavior.
//!
//! The fuse3 translation is shared between Linux and macOS; the small set of
//! platform-specific behaviors that translation uses lives here, behind the
//! [`FusePlatform`] policy, so the shared code carries no cross-OS `cfg`
//! branches. The one exception is `crtime`, a struct-shape difference that
//! stays in the attribute builder.

use std::path::Path;
use std::sync::Arc;

/// FUSE open flag: bypass kernel page cache for this file.
///
/// From `linux/fuse.h`: `#define FOPEN_DIRECT_IO (1 << 0)`
///
/// When set in `ReplyOpen::flags`, the kernel sends every `read()` through
/// the FUSE handler instead of serving from its page cache. Used for virtual
/// DDS files so that every X-Plane read is counted (`MetricEvent::FuseRead`)
/// and none is answered from a cache this filesystem cannot invalidate.
const FOPEN_DIRECT_IO: u32 = 1;

/// Platform-specific behavior of the shared fuse3 translation.
pub trait FusePlatform: Send + Sync {
    /// Open flags reported for virtual DDS files.
    ///
    /// Linux uses [`FOPEN_DIRECT_IO`]: every `read()` reaches the handler, so
    /// the FUSE read counters are exact and the page cache never holds a
    /// second copy of a tile the memory cache already has.
    ///
    /// **macOS must not.** X-Plane memory-maps texture files, and macFUSE
    /// faults with `EXC_BAD_ACCESS` when a `direct_io` file is `mmap`ed,
    /// because `mmap` needs the page cache that `direct_io` bypasses. macOS
    /// therefore serves virtual DDS through the page cache, trading per-read
    /// visibility for not crashing the sim's texture loader. The one
    /// consequence is that the FUSE read counters under-report there: a
    /// tile's cold first read reaches the handler and repeat reads are
    /// answered by the kernel. Nothing functional depends on seeing every
    /// read (#204): prefetch is driven by telemetry, and the cold reads that
    /// trigger generation still arrive.
    fn virtual_dds_open_flags(&self) -> u32;

    /// The macFUSE `volname` option naming the volume after its mount-point
    /// folder (#305), if the platform has volume names.
    ///
    /// Finder shows a mounted volume's name in place of the folder it is
    /// mounted on, and macFUSE's default is `macFUSE Volume 0 (<process>)`,
    /// so without this `zzXEL_ortho` appears in Custom Scenery under that
    /// name. Linux has no volume names and reports `None`. A folder name
    /// containing a comma is also `None`, because macFUSE would split it
    /// into a second option.
    fn volume_name_option(&self, mount_path: &Path) -> Option<String>;
}

/// FUSE behavior on Linux.
pub struct Linux;

impl FusePlatform for Linux {
    fn virtual_dds_open_flags(&self) -> u32 {
        FOPEN_DIRECT_IO
    }

    fn volume_name_option(&self, _mount_path: &Path) -> Option<String> {
        None
    }
}

/// FUSE behavior on macOS.
pub struct Mac;

impl FusePlatform for Mac {
    fn virtual_dds_open_flags(&self) -> u32 {
        0
    }

    fn volume_name_option(&self, mount_path: &Path) -> Option<String> {
        let name = mount_path.file_name()?.to_str()?;
        (!name.contains(',')).then(|| format!("volname={name}"))
    }
}

/// The FUSE platform policy for the OS this binary runs on.
///
/// This is the only place that maps "the current OS" to a policy. Callers
/// that must not name a platform ask for this instead of picking one.
pub fn current_platform() -> Arc<dyn FusePlatform> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(Mac)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(Linux)
    }
}

#[cfg(test)]
mod tests {
    use super::{FusePlatform, Linux, Mac, FOPEN_DIRECT_IO};
    use std::path::Path;

    #[test]
    fn linux_bypasses_the_page_cache_for_virtual_dds() {
        assert_eq!(Linux.virtual_dds_open_flags(), FOPEN_DIRECT_IO);
    }

    #[test]
    fn macos_serves_virtual_dds_through_the_page_cache() {
        assert_eq!(Mac.virtual_dds_open_flags(), 0);
    }

    /// #305: Finder shows a mounted volume's name in place of its mount-point
    /// folder, and macFUSE's default is `macFUSE Volume 0 (<process>)`.
    #[test]
    fn on_macos_the_volume_is_named_after_the_mount_point_folder() {
        let mount = Path::new("/X-Plane 12/Custom Scenery/zzXEL_ortho");

        assert_eq!(
            Mac.volume_name_option(mount).as_deref(),
            Some("volname=zzXEL_ortho")
        );
    }

    #[test]
    fn on_linux_there_is_no_volume_name() {
        let mount = Path::new("/X-Plane 12/Custom Scenery/zzXEL_ortho");

        assert_eq!(Linux.volume_name_option(mount), None);
    }

    /// macFUSE splits its option list on commas, so a name containing one
    /// would leak its tail in as a separate option.
    #[test]
    fn a_folder_name_with_a_comma_is_not_passed_as_a_volume_name() {
        let mount = Path::new("/Custom Scenery/zzXEL_ortho,allow_other");

        assert_eq!(Mac.volume_name_option(mount), None);
    }
}
