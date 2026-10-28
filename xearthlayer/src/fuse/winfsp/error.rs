//! The cross-platform half of the WinFSP backend error seam.
//!
//! The neutral core reports every failure with the three-variant
//! [`FsError`](crate::fuse::support::error::FsError); each backend maps
//! those onto its kernel's error space (the table in
//! `support/error.rs`). This module produces the neutral
//! `std::io::Error` form; the Windows adapter applies the one-line
//! `FspError::from(...)` on top (see `context.rs`).
//!
//! | `FsError`     | `io::ErrorKind`   | WinFSP result (via `FspError`)        |
//! |---------------|-------------------|---------------------------------------|
//! | `NotFound`    | `NotFound`        | `ERROR_FILE_NOT_FOUND`                |
//! | `NotADirectory`| `NotADirectory`   | `ERROR_NOT_DIR`                       |
//! | `Io(e)`       | preserved         | `e`'s raw WIN32 code, or `FSP_ERROR_IO`|
//!
//! Why route through `io::Error` instead of mapping straight to WinFSP
//! types: `FspError::IO` accepts only seven `ErrorKind`s (winfsp 0.13.2
//! `src/error.rs`); anything else degrades to
//! `STATUS_UNEXPECTED_IO_ERROR`. Going through `io::Error` lets the
//! Windows shim use `FspError::from(io::Error)`, which prefers the error's
//! `raw_os_error()` — the exact WIN32 code the core already carries — over
//! its kind. And because this module stays std-only, CI unit-tests the
//! table on every OS even though the adapter that consumes it is
//! Windows-only (design `docs/dev/winfsp-backend.md` §4.3, §7.6).

use std::io;

use crate::fuse::support::error::FsError;

/// Map a core error onto the neutral `io::Error` form each backend converts
/// into its kernel error space.
///
/// Takes the error by value: the mapping consumes it. The `Io` variant is
/// moved through unchanged — the core already holds the platform error, and
/// its `raw_os_error()` is what the Windows shim keys off.
pub fn to_io_error(err: FsError) -> io::Error {
    match err {
        FsError::NotFound => io::Error::new(io::ErrorKind::NotFound, "not found"),
        FsError::NotADirectory => io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
        FsError::Io(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn not_found_maps_to_the_not_found_kind() {
        let mapped = to_io_error(FsError::NotFound);
        assert_eq!(mapped.kind(), ErrorKind::NotFound);
        // The message stays the core's stable string for logs and metrics.
        assert!(mapped.to_string().contains("not found"));
    }

    #[test]
    fn not_a_directory_maps_to_the_not_a_directory_kind() {
        let mapped = to_io_error(FsError::NotADirectory);
        assert_eq!(mapped.kind(), ErrorKind::NotADirectory);
        assert!(mapped.to_string().contains("not a directory"));
    }

    #[test]
    fn io_error_kind_is_preserved() {
        let source = io::Error::new(ErrorKind::Other, "disk said no");
        let mapped = to_io_error(FsError::Io(source));
        assert_eq!(mapped.kind(), ErrorKind::Other);
        assert!(mapped.to_string().contains("disk said no"));
    }

    #[test]
    fn io_error_raw_os_error_survives() {
        // The Windows shim depends on this: FspError::from prefers
        // raw_os_error() (the exact WIN32 code) over the kind, so a real
        // OS failure the core carried must keep its code end to end.
        let source = io::Error::from_raw_os_error(5); // ACCESS_DENIED
        let mapped = to_io_error(FsError::Io(source));
        assert_eq!(mapped.raw_os_error(), Some(5));
    }

    #[test]
    fn every_variant_maps_without_panic() {
        // The table is total: a backend must never have a core error it
        // cannot express.
        for err in [
            FsError::NotFound,
            FsError::NotADirectory,
            FsError::Io(io::Error::other("boom")),
        ] {
            let _ = to_io_error(err);
        }
    }
}
