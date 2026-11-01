//! Shared support types for the FUSE filesystem (used by fuse3).
//!
//! This module provides:
//! - [`types`] - Request/response types for DDS generation
//! - [`inode`] - Inode allocation and management

// Shared types used by fuse3
pub mod inode;
mod types;

// Re-export shared types for use by fuse3 and public API
pub use types::{DdsHandler, DdsRequest, DdsResponse};
