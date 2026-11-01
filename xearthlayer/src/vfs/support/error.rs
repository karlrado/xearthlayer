use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    /// The requested node does not exist in the union view.
    #[error("not found")]
    NotFound,

    /// The requested node is not a directory (e.g. a directory listing was
    /// asked for a file).
    #[error("not a directory")]
    NotADirectory,

    /// An I/O failure while reading a real file from its source.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_stable() {
        // Backends and logs key off these strings; keep them exact.
        assert_eq!(FsError::NotFound.to_string(), "not found");
        assert_eq!(FsError::NotADirectory.to_string(), "not a directory");
    }

    #[test]
    fn io_error_converts_via_from() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file missing");
        let fs_err: FsError = io_err.into();

        match fs_err {
            FsError::Io(e) => assert_eq!(e.to_string(), "file missing"),
            _ => panic!("expected Io variant"),
        }
    }

    #[test]
    fn io_variant_reports_the_cause() {
        let io_err = io::Error::other("disk error");
        assert!(FsError::Io(io_err).to_string().contains("disk error"));
    }
}
