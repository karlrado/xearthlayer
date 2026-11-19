//! Types for the fuse3 filesystem implementation.

use crate::vfs::common::FsError;
use fuse3::Errno;
use std::io;
use thiserror::Error;

/// Result type for fuse3 operations.
pub type Fuse3Result<T> = Result<T, Fuse3Error>;

/// Errors that can occur in the fuse3 filesystem.
#[derive(Debug, Error)]
pub enum Fuse3Error {
    /// I/O error during filesystem operations
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Mount operation failed
    #[error("Mount failed: {0}")]
    MountFailed(String),

    /// Invalid path
    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// Map the core's neutral error onto FUSE's error space.
///
/// The whole core -> kernel failure translation in one place (the inline
/// `Errno::from(libc::…)` sites each op used to carry). I/O failures keep
/// their real OS error (`raw_os_error`) instead of flattening to `EIO`, so
/// a vanished source reads as `ENOENT` and a permission failure as
/// `EACCES`; only an error with no OS code behind it falls back to `EIO`.
/// errno_from lives in the fuse3 adapter because common code cannot
/// reference `Errno` (see `docs/dev/filesystem-backend-split.md`).
pub fn errno_from(err: FsError) -> Errno {
    match err {
        FsError::NotFound => Errno::from(libc::ENOENT),
        FsError::NotADirectory => Errno::from(libc::ENOTDIR),
        FsError::Io(e) => Errno::from(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse3_error_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: Fuse3Error = io_err.into();
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_fuse3_error_mount_failed() {
        let err = Fuse3Error::MountFailed("permission denied".to_string());
        assert!(err.to_string().contains("Mount failed"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn test_fuse3_error_invalid_path() {
        let err = Fuse3Error::InvalidPath("/invalid/path".to_string());
        assert!(err.to_string().contains("Invalid path"));
        assert!(err.to_string().contains("/invalid/path"));
    }

    #[test]
    #[allow(clippy::unnecessary_literal_unwrap)]
    fn test_fuse3_result_type() {
        // Test that Fuse3Result works as expected
        let ok_result: Fuse3Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        let err_result: Fuse3Result<i32> = Err(Fuse3Error::InvalidPath("test".to_string()));
        assert!(err_result.is_err());
    }

    #[test]
    fn errno_from_maps_the_neutral_variants() {
        assert_eq!(errno_from(FsError::NotFound), Errno::from(libc::ENOENT));
        assert_eq!(
            errno_from(FsError::NotADirectory),
            Errno::from(libc::ENOTDIR)
        );
    }

    #[test]
    fn errno_from_propagates_the_raw_os_error() {
        // The deliberate behaviour change over the pre-split flattening:
        // a real OS error keeps its identity instead of becoming EIO.
        let io = std::io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(errno_from(FsError::Io(io)), Errno::from(libc::EACCES));

        // No OS code behind it: EIO is the fallback.
        let io = std::io::Error::other("no os code");
        assert_eq!(errno_from(FsError::Io(io)), Errno::from(libc::EIO));
    }
}
