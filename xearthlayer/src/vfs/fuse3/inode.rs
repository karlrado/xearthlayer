//! Inode management for the fuse3 filesystem.
//!
//! Re-exports the thread-safe [`InodeManager`] from [`crate::vfs::support`].

pub use crate::vfs::support::inode::InodeManager;
