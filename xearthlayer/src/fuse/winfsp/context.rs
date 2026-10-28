//! The WinFSP trait impl over the neutral core (design
//! `docs/dev/winfsp-backend.md` §4.3).
//!
//! Windows-only: this is the only module in the backend that names WinFSP
//! types. The mapping/resolution logic it delegates to lives in the pure,
//! cross-platform `error`/`resolve` siblings (CI-tested on every OS), and
//! the sync→async seam is `pollster::block_on` (§3.4) — safe because the
//! WinFSP dispatcher threads are never tokio workers.
//!
//! The driver invokes these from multiple dispatcher threads under the
//! default `FineGuard` strategy (exclusive-shared lock per op class, no
//! lock on reads), so every core call goes through the core's internally
//! synchronized API (`Arc` + `DashMap` + atomics) and no adapter lock is
//! needed (§3.1).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use crate::fuse::support::attrs::{DirEntry, FileAttributes, NodeKind};
use crate::fuse::support::core::OrthoUnionCore;
use crate::fuse::support::error::FsError;
use crate::fuse::support::node::FsNode;
use crate::fuse::winfsp::error::to_io_error;
use crate::fuse::winfsp::resolve::{split_components, walk};

/// Convert a core error into the WinFSP error space.
///
/// One line on top of `error.rs`'s table: `FspError::from` prefers the io
/// error's `raw_os_error()` (the exact WIN32 code the core carried) over
/// its kind, so real OS failures keep their code end to end, while
/// `NotFound`/`NotADirectory` map to `ERROR_FILE_NOT_FOUND`/`ERROR_NOT_DIR`
/// via the kind.
fn to_fsp(err: FsError) -> FspError {
    FspError::from(to_io_error(err))
}

/// The FILETIME encoding of `t`: 100 ns intervals since 1601-01-01.
fn filetime(t: SystemTime) -> u64 {
    const UNIX_TO_WINDOWS_EPOCH: u64 = 11_644_473_600_000_000; // 11644473600 s * 10^7
    let since_unix = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    since_unix
        .as_secs()
        .saturating_mul(10_000_000)
        .saturating_add(since_unix.subsec_nanos() as u64 / 100)
        .saturating_add(UNIX_TO_WINDOWS_EPOCH)
}

/// The neutral attributes as a WinFSP `FileInfo`.
///
/// The volume is read-only on every platform, so files report
/// `FILE_ATTRIBUTE_READONLY`; directories report `FILE_ATTRIBUTE_DIRECTORY`
/// (the same policy the spike used, design §4.4a). `uid`/`gid`/`rdev` are
/// unused by the driver and dropped (§4.3 mapping note).
fn file_info_from_attrs(attrs: &FileAttributes) -> FileInfo {
    FileInfo {
        file_attributes: match attrs.kind {
            NodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY.0,
            NodeKind::RegularFile => FILE_ATTRIBUTE_READONLY.0,
            // The union view creates no symlinks; report the fallback the
            // volume actually serves.
            NodeKind::Symlink => FILE_ATTRIBUTE_READONLY.0,
        },
        file_size: attrs.size,
        allocation_size: attrs.size.div_ceil(4096).saturating_mul(4096),
        creation_time: filetime(attrs.mtime),
        last_access_time: filetime(attrs.atime),
        last_write_time: filetime(attrs.mtime),
        change_time: filetime(attrs.mtime),
        ..Default::default()
    }
}

/// One open handle: the core node it refers to and the core's per-open file
/// handle.
pub struct FileEntry {
    /// The core node this handle refers to (from the `lookup` walk).
    pub node: FsNode,
    /// The core's per-open handle. `0` = stateless (real passthrough files
    /// and directories); atomically zeroed by `cleanup` so the release
    /// happens exactly once regardless of cleanup/close ordering.
    fh: AtomicU64,
}

/// The WinFSP backend context: a thin adapter over the neutral
/// [`OrthoUnionCore`].
///
/// The core is constructed by the caller (commit 5's builder mirrors the
/// FUSE adapter's construction), which is what keeps this struct free of
/// lifecycle concerns.
#[derive(Clone)]
pub struct WinFspOrthoUnionFS {
    core: Arc<OrthoUnionCore>,
}

impl WinFspOrthoUnionFS {
    /// Wrap an existing core.
    pub fn new(core: Arc<OrthoUnionCore>) -> Self {
        Self { core }
    }

    /// The core this context serves (the commit-5 mount flow builds the
    /// host around the same core).
    pub fn core(&self) -> &Arc<OrthoUnionCore> {
        &self.core
    }

    /// Walk a driver path through the core — the sync→async seam (§3.4).
    fn resolve_node(&self, file_name: &U16CStr) -> Result<FsNode> {
        // The lossy-UTF-8 view is the same path the core's lookup takes on
        // FUSE; a name that cannot round-trip simply does not match the
        // index and is reported missing.
        let components = split_components(&file_name.to_string_lossy());
        let core = Arc::clone(&self.core);
        walk(&components, move |parent, name| {
            let core = Arc::clone(&core);
            pollster::block_on(async move { core.lookup(parent, name).await })
        })
        .map_err(to_fsp)
    }

    /// The attributes to report for one listed entry.
    ///
    /// Parity with the FUSE `readdirplus`: file entries arrive with
    /// placeholder attributes, and this is the one op that upgrades them to
    /// real metadata — accurate sizes are what DSF and other passthrough
    /// files need reported to X-Plane. WinFSP has only this one enumeration
    /// path, so it always upgrades (plain `readdir` stays I/O-free on FUSE
    /// precisely because `readdirplus` exists; there is no equivalent
    /// escape hatch here). If the source vanished, fall back to the
    /// placeholder, exactly as `readdirplus` does.
    fn entry_attrs(&self, entry: &DirEntry) -> FileAttributes {
        if entry.is_dir {
            return entry.attrs.clone();
        }
        match pollster::block_on(self.core.attrs(&entry.node)) {
            Ok(real) => real,
            Err(_) => entry.attrs.clone(),
        }
    }
}

impl FileSystemContext for WinFspOrthoUnionFS {
    type FileContext = Arc<FileEntry>;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let node = self.resolve_node(file_name)?;
        Ok(FileSecurity {
            reparse: false,
            // No ACL policy: the driver applies its default ACL (the
            // documented opt-out for filesystems without ACLs — the parent
            // user opens the files, design §4.3).
            sz_security_descriptor: 0,
            attributes: if node.is_directory() {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_READONLY.0
            },
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let node = self.resolve_node(file_name)?;

        // Report the node's attributes so the driver can cache them; sizes
        // matter for passthrough DSF/TER files.
        let attrs = pollster::block_on(self.core.attrs(&node)).map_err(to_fsp)?;
        let info = file_info_from_attrs(&attrs);
        let fi = file_info.as_mut();
        fi.file_attributes = info.file_attributes;
        fi.allocation_size = info.allocation_size;
        fi.file_size = info.file_size;
        fi.creation_time = info.creation_time;
        fi.last_access_time = info.last_access_time;
        fi.last_write_time = info.last_write_time;
        fi.change_time = info.change_time;

        // Per-open state for virtual DDS (memoised tile), 0 otherwise.
        let fh = self.core.open(&node);
        Ok(Arc::new(FileEntry {
            node,
            fh: AtomicU64::new(fh),
        }))
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, _flags: u32) {
        // Release the per-open state exactly once, regardless of the
        // cleanup/close order the driver picks (§4.3).
        let fh = context.fh.swap(0, Ordering::AcqRel);
        if fh != 0 {
            self.core.release(fh);
        }
    }

    fn close(&self, _context: Self::FileContext) {
        // `cleanup` already released the core handle; dropping the Arc
        // frees the entry (§4.3).
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        let attrs = pollster::block_on(self.core.attrs(&context.node)).map_err(to_fsp)?;
        *file_info = file_info_from_attrs(&attrs);
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        let fh = context.fh.load(Ordering::Acquire);
        let data =
            pollster::block_on(
                self.core
                    .read(&context.node, fh, offset, buffer.len() as u32),
            )
            .map_err(to_fsp)?;

        // The core serves whatever the source has: a short read at the end,
        // empty past it. `0` is the driver's EOF signal, so an EOF probe
        // costs the core's (already bounded) answer and nothing more.
        let len = data.len().min(buffer.len());
        buffer[..len].copy_from_slice(&data[..len]);
        Ok(len as u32)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        // The core's listing is one-shot; pagination is the driver's job.
        // It sorts the staging buffer by name (ordering "." and ".." first)
        // and, on each call, resumes strictly after `marker` — verified
        // against the WinFSP 2.1 `dirbuf.c` implementation of
        // `FspFileSystemReadDirectoryBuffer`. So we write the full listing
        // and hand the marker back to the driver.
        //
        // The pattern is left to the driver's own matching, as the vtable
        // contract allows.
        let listing =
            pollster::block_on(self.core.list_directory(&context.node)).map_err(to_fsp)?;

        let dir_buffer = DirBuffer::new();
        {
            // A capacity hint only (the staging buffer grows as needed);
            // sized to the full listing so a busy directory fills without
            // regrowth.
            let hint = listing
                .len()
                .saturating_mul(std::mem::size_of::<DirInfo<255>>() + 512)
                .min(u32::MAX as usize) as u32;
            let lock = dir_buffer.acquire(true, Some(hint))?;
            for entry in &listing {
                let mut dir_info = DirInfo::<255>::default();
                let attrs = self.entry_attrs(entry);
                *dir_info.file_info_mut() = file_info_from_attrs(&attrs);
                dir_info.set_name(&entry.name)?;
                lock.write(&mut dir_info)?;
            }
            // Releasing the lock hands the buffer to the driver (which
            // sorts it before the read below).
        }
        Ok(dir_buffer.read(marker, buffer))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        // The core's constant volume figures, exactly as the FUSE `statfs`
        // op reports them.
        let stat = self.core.statfs();
        let block = stat.bsize.max(1) as u64;
        out_volume_info.total_size = stat.blocks.saturating_mul(block);
        out_volume_info.free_size = stat.bfree.saturating_mul(block);
        let _ = out_volume_info.set_volume_label("XEarthLayer Ortho");
        Ok(())
    }

    fn dispatcher_stopped(&self, normally: bool) {
        tracing::debug!(normally, "WinFSP dispatcher stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_maps_the_unix_epoch_to_the_windows_epoch() {
        // 1970-01-01 is 11644473600 s after 1601-01-01, i.e.
        // 11644473600 * 10^7 100-ns units.
        assert_eq!(filetime(UNIX_EPOCH), 11_644_473_600_000_000);
    }

    #[test]
    fn filetime_counts_100ns_units() {
        let t = UNIX_EPOCH + std::time::Duration::from_nanos(123_456_789);
        assert_eq!(filetime(t), 11_644_473_600_000_000 + 1_234_567);
    }

    #[test]
    fn regular_files_report_read_only() {
        let attrs = FileAttributes {
            kind: NodeKind::RegularFile,
            ..test_attrs(0)
        };
        assert_eq!(
            file_info_from_attrs(&attrs).file_attributes,
            FILE_ATTRIBUTE_READONLY.0
        );
    }

    #[test]
    fn directories_report_the_directory_attribute() {
        let attrs = FileAttributes {
            kind: NodeKind::Directory,
            ..test_attrs(0)
        };
        assert_eq!(
            file_info_from_attrs(&attrs).file_attributes,
            FILE_ATTRIBUTE_DIRECTORY.0
        );
    }

    #[test]
    fn sizes_round_to_the_allocation_unit() {
        let attrs = test_attrs(10_000);
        let info = file_info_from_attrs(&attrs);
        assert_eq!(info.file_size, 10_000);
        assert_eq!(info.allocation_size, 12_288); // 3 x 4096
    }

    fn test_attrs(size: u64) -> FileAttributes {
        FileAttributes {
            size,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            kind: NodeKind::RegularFile,
            perm: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
        }
    }
}
