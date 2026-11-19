//! Per-platform FUSE behavior and policy within the fuse3 backend.
//!
//! The complete content of this file is the four decisions of
//! `docs/dev/filesystem-backend-split.md` §FusePlatform: the open flags
//! reported for virtual DDS files, the mount API call, the `volname` mount
//! option, and the `FileAttr` fields that exist only in macFUSE. The rest of
//! the fuse3 backend — the shared translation in `ortho_union_fs` and (after
//! the split's mount step) `adapters` — is platform-neutral.
//!
//! This is the one place in `vfs/` allowed to name a platform: the rest of
//! the tree keeps `#[cfg(target_os)]` out entirely, and the mount seam's
//! `select_backend()` may use `cfg!` but no cfg attributes. Where both
//! answers to a decision compile on both OSes — true of every decision here
//! on the pinned fuse3 fork with its `unprivileged` feature — the choice is
//! made with `cfg!`, so the Linux-only answer still type-checks and the rule
//! still tests on macOS, and vice versa. Field accesses that exist only in
//! one platform's `FileAttr` (decision 4) need a `#[cfg(target_os)]` block;
//! the split's acceptance rules admit exactly that exception, here.

use std::path::Path;

use fuse3::raw::reply::FileAttr;
use fuse3::{FileType, MountOptions};

use crate::vfs::common::attrs::{FileAttributes, NodeKind};

// =============================================================================
// Decision 1: open flags for virtual DDS files
// =============================================================================

/// FUSE open flag: bypass the kernel page cache for this file.
///
/// From `linux/fuse.h`: `#define FOPEN_DIRECT_IO (1 << 0)`
///
/// When set in `ReplyOpen::flags`, the kernel sends every `read()` through
/// the FUSE handler instead of serving from its page cache. Used for virtual
/// DDS files so that every X-Plane read is counted (`MetricEvent::FuseRead`)
/// and none is answered from a cache this filesystem cannot invalidate.
pub const FOPEN_DIRECT_IO: u32 = 1;

/// The flags to report when opening a virtual DDS file.
///
/// Linux uses [`FOPEN_DIRECT_IO`]: every `read()` reaches the handler, so
/// the FUSE read counters are exact and the page cache never holds a second
/// copy of a tile the memory cache already has.
///
/// **macOS must not.** X-Plane memory-maps texture files, and macFUSE faults
/// with `EXC_BAD_ACCESS` when a `direct_io` file is `mmap`ed, because `mmap`
/// needs the page cache that `direct_io` bypasses. macOS therefore serves
/// virtual DDS through the page cache, trading per-read visibility for not
/// crashing the sim's texture loader. The one consequence is that the FUSE
/// read counters under-report there: a tile's cold first read reaches the
/// handler and repeat reads are answered by the kernel. Nothing functional
/// depends on seeing every read (#204): prefetch is driven by telemetry, and
/// the cold reads that trigger generation still arrive.
///
/// The adapter applies these flags keyed on the [`FsNode`] variant — only
/// `VirtualDds` opens carry them; `RealFile` and `Directory` opens carry
/// none (contract §open: the handle is `0` for those, and kernel caching is
/// what passthrough files want).
///
/// [`FsNode`]: crate::vfs::common::FsNode
pub fn virtual_dds_open_flags() -> u32 {
    if cfg!(target_os = "macos") {
        0
    } else {
        FOPEN_DIRECT_IO
    }
}

// =============================================================================
// Decision 2: the mount API call
// =============================================================================

/// Mount `fs` at `mountpoint` through the platform's mount API:
/// `Session::mount_with_unprivileged` on Linux, `Session::mount` on macOS.
///
/// Both methods compile on both OSes on the pinned fuse3 fork (the
/// `unprivileged` feature carries the Linux one), so the decision is a
/// `cfg!` branch and both arms type-check on every platform — the fork's
/// macOS behavior stays under CI regardless of which arm runs.
///
/// Returns the raw mount handle; the caller wraps it in its session type.
pub async fn mount<FS>(
    options: MountOptions,
    fs: FS,
    mountpoint: &Path,
) -> std::io::Result<fuse3::raw::MountHandle>
where
    FS: fuse3::raw::Filesystem + Send + Sync + 'static,
{
    if cfg!(target_os = "macos") {
        fuse3::raw::Session::new(options)
            .mount(fs, mountpoint)
            .await
    } else {
        fuse3::raw::Session::new(options)
            .mount_with_unprivileged(fs, mountpoint)
            .await
    }
}

// =============================================================================
// Decision 3: the `volname` mount option
// =============================================================================

/// The macFUSE `volname` option naming the volume after its mount-point
/// folder (#305), or `None` where no volume name applies.
///
/// Finder shows a mounted volume's name in place of the folder it is mounted
/// on, and macFUSE's default is `macFUSE Volume 0 (<process>)`, so without
/// this `zzXEL_ortho` appears in Custom Scenery under that default name.
pub fn volume_name_option(mount_path: &Path) -> Option<String> {
    volume_name_for(mount_path, cfg!(target_os = "macos"))
}

/// The rule under an explicit platform answer, so both sides are testable
/// on one host. `None` off macOS, which has no volume names, and for a
/// folder name containing a comma, which macFUSE would split into a second
/// option.
fn volume_name_for(mount_path: &Path, macos: bool) -> Option<String> {
    if !macos {
        return None;
    }
    let name = mount_path.file_name()?.to_str()?;
    (!name.contains(',')).then(|| format!("volname={name}"))
}

// =============================================================================
// Decision 4: FileAttr fields that exist only in macFUSE
// =============================================================================

/// Neutral attributes plus the kernel inode → FUSE `FileAttr`.
///
/// The one place in the crate where the fuse3 attribute shape is assembled,
/// which is why decision 4's field difference is settled right here:
/// `crtime` (creation time) and `flags` (BSD file flags) exist only in
/// macFUSE's `FileAttr`, and `FileAttr` has no `Default` to build around —
/// the cfg'd fields below are the split's single permitted `#[cfg]` outside
/// this file's own decisions' `cfg!` (see §FusePlatform and §Acceptance in
/// `docs/dev/filesystem-backend-split.md`).
///
/// `ino` is the kernel identity the core does not know. The core supplies
/// `crtime` in its neutral [`FileAttributes`]; the platform decides only
/// whether it reaches the kernel at all, and `flags` is always 0 — the union
/// view has no BSD flags, as in the `FileAttrBuilder` methods the neutral
/// types replaced. Every other field is a copy: the core's constructors
/// already fill `ctime`/`crtime` and the process owner exactly as the FUSE
/// answers did.
pub fn file_attr(attrs: &FileAttributes, ino: u64) -> FileAttr {
    FileAttr {
        ino,
        size: attrs.size,
        blocks: attrs.blocks,
        atime: attrs.atime.into(),
        mtime: attrs.mtime.into(),
        ctime: attrs.ctime.into(),
        kind: match attrs.kind {
            NodeKind::Directory => FileType::Directory,
            NodeKind::RegularFile => FileType::RegularFile,
            NodeKind::Symlink => FileType::Symlink,
        },
        perm: attrs.perm,
        nlink: attrs.nlink,
        uid: attrs.uid,
        gid: attrs.gid,
        rdev: attrs.rdev,
        blksize: attrs.blksize,
        #[cfg(target_os = "macos")]
        crtime: attrs.crtime.into(),
        #[cfg(target_os = "macos")]
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount_path() -> &'static Path {
        Path::new("/X-Plane 12/Custom Scenery/zzXEL_ortho")
    }

    /// #305: Finder shows a mounted volume's name in place of its mount-point
    /// folder, and macFUSE's default is `macFUSE Volume 0 (<process>)`.
    #[test]
    fn on_macos_the_volume_is_named_after_the_mount_point_folder() {
        assert_eq!(
            volume_name_for(mount_path(), true).as_deref(),
            Some("volname=zzXEL_ortho")
        );
    }

    #[test]
    fn off_macos_there_is_no_volume_name() {
        assert_eq!(volume_name_for(mount_path(), false), None);
    }

    /// macFUSE splits its option list on commas, so a name containing one
    /// would leak its tail in as a separate option.
    #[test]
    fn a_folder_name_with_a_comma_is_not_passed_as_a_volume_name() {
        let mount = Path::new("/Custom Scenery/zzXEL_ortho,allow_other");
        assert_eq!(volume_name_for(mount, true), None);
    }

    /// The decision follows the host, and the rule it encodes is the doc's
    /// table verbatim: direct I/O off macOS, page cache on macOS (#204).
    #[test]
    fn virtual_dds_open_flags_follow_the_platform_rule() {
        if cfg!(target_os = "macos") {
            assert_eq!(
                virtual_dds_open_flags(),
                0,
                "macOS must page-cache virtual DDS: direct_io crashes X-Plane's mmap loader (#204)"
            );
        } else {
            assert_eq!(
                virtual_dds_open_flags(),
                FOPEN_DIRECT_IO,
                "Linux must send every virtual DDS read to the handler"
            );
        }
    }

    /// The host-visible answer equals the rule function's, so the public
    /// entry point cannot drift from the tested rule.
    #[test]
    fn volume_name_option_answers_for_the_host() {
        assert_eq!(
            volume_name_option(mount_path()).is_some(),
            cfg!(target_os = "macos")
        );
    }
}
