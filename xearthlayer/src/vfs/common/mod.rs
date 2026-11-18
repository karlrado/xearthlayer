//! The backend-neutral half of the virtual filesystem.
//!
//! Everything here is written once for every mount backend: the node,
//! attribute and error types, the [`UnionFilesystem`] contract its core
//! implements, and (as the split proceeds) the core itself and the neutral
//! conformance suite that pins its behaviour. Nothing in this module may
//! name a backend: no `fuse3::` types, no kernel inodes, no cache-policy
//! flags — those belong to the backend adapters (see
//! `docs/dev/filesystem-backend-split.md` §Acceptance).

pub mod attrs;
pub mod error;
pub mod node;

pub use attrs::{DirEntry, Entry, FileAttributes, NodeKind, StatFsInfo};
pub use error::FsError;
pub use node::FsNode;
