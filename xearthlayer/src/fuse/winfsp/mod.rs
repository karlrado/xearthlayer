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
//! - [`context`] — the `FileSystemContext` trait impl over the neutral
//!   core. Windows-only: the `winfsp` crate is a Windows dependency (§4.6).
//!
//! Nothing here has a caller until commits 5–6 wire the backend into the
//! mount flow (`manager/mounts.rs`), so dead code is allowed across this
//! module — the same precedent as `support/mod.rs`.
#![allow(dead_code)]

pub(crate) mod error;
pub(crate) mod resolve;

#[cfg(windows)]
pub(crate) mod context;
