//! Shared constants for the fuse3 filesystem implementations.
//!
//! The `FileAttrBuilder` trait, the virtual-DDS configuration and the DDS
//! request pipeline that used to live here moved to the backend-neutral
//! [`crate::vfs::common`] module with the [`OrthoUnionCore`] extraction
//! (docs/dev/filesystem-backend-split.md step 3a); the attribute shape is
//! now assembled only at the fuse3 boundary (`ortho_union_fs::to_file_attr`).
//! The neutral items are re-exported below so imports that still address
//! them through this module keep working.
//!
//! [`OrthoUnionCore`]: crate::vfs::common::OrthoUnionCore

use std::time::Duration;

/// Time-to-live for FUSE attribute caching.
///
/// Kernel attribute-cache policy is fuse3-side, so this stays in the
/// backend rather than joining the neutral types.
pub const TTL: Duration = Duration::from_secs(1);

// =============================================================================
// Neutral items, re-exported (see the module docs)
// =============================================================================

pub use crate::vfs::common::attrs::{chunk_to_tile_coords, VirtualDdsConfig};
pub use crate::vfs::common::requestor::DdsRequestor;
