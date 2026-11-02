//! Shared support types for the FUSE filesystem (used by fuse3).
//!
//! This module provides:
//! - [`types`] - Request/response types for DDS generation
//! - [`inode`] - Inode allocation and management

// Shared types used by fuse3
pub mod attrs;
#[cfg(test)]
pub(crate) mod conformance;
pub mod core;
pub mod error;
pub mod inode;
pub mod node;
pub mod requestor;
mod types;
pub mod union;

// Re-export shared types for use by fuse3 and public API
pub use types::{DdsHandler, DdsRequest, DdsResponse};
