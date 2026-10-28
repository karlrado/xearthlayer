//! FUSE filesystem for on-demand DDS texture generation.
//!
//! Provides a virtual filesystem that intercepts X-Plane texture reads
//! and generates satellite imagery DDS files on demand.
//!
//! # Implementation
//!
//! Uses [`fuse3::Fuse3OrthoUnionFS`] - an async multi-threaded union filesystem that
//! merges all ortho sources into one mount while generating DDS textures on-demand.

// Internal support modules: shared types + inode management (used by fuse3)
pub(crate) mod support;

mod coalesce;
mod filename;

// The FUSE adapter is Unix-only: Windows serves the same neutral core
// through WinFSP (Phase 2, docs/dev/winfsp-backend.md §4.1), so the fuse3
// crate and everything bound to it stay out of the Windows build graph.
#[cfg(not(target_os = "windows"))]
pub mod fuse3;

// The WinFSP backend (Phase 2, docs/dev/winfsp-backend.md §4.1). Its pure
// helpers are cross-platform (CI-tested on every OS); the trait impl is
// Windows-only.
pub(crate) mod winfsp;

mod placeholder;

// Re-export types for public API
pub use coalesce::{CoalesceResult, CoalescedResult, CoalescerStats, RequestCoalescer};
pub use filename::{parse_dds_filename, DdsFilename, ParseError, TextureKind};
#[cfg(not(target_os = "windows"))]
pub use fuse3::{Fuse3Error, Fuse3Result, MountHandle, SpawnedMountHandle};
pub use placeholder::{
    generate_default_placeholder, generate_magenta_placeholder, get_default_placeholder,
    init_placeholder_cache, validate_dds_or_placeholder, EXPECTED_DDS_SIZE,
};
pub use support::{DdsHandler, DdsRequest, DdsResponse};
