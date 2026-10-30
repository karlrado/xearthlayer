//! Shared support types for the FUSE filesystem (used by fuse3).
//!
//! This module provides:
//! - [`types`] - Request/response types for DDS generation
//! - [`inode`] - Inode allocation and management
//! - [`attrs`] - Neutral attribute seam + virtual-DDS configuration
//! - [`node`] - Backend-agnostic filesystem node identity
//! - [`error`] - Backend-agnostic error type
//! - [`requestor`] - Neutral DDS-generation request pipeline
//! - [`core`] - The backend-neutral ortho union core (state + builders + accessors)

// Shared types used by fuse3
pub mod inode;
mod types;

// Backend-agnostic core types (shared by the FUSE backend and, later,
// the WinFSP backend). Introduced ahead of the core that consumes them
// (docs/dev/windows-backend-split.md §6, commits 2–4); silence dead_code
// until those commits wire them in.
#[allow(dead_code)]
pub mod attrs;
#[allow(dead_code)]
pub mod error;
#[allow(dead_code)]
pub mod node;
pub mod requestor;

/// The backend-neutral ortho union core: state, builders, and accessors
/// shared by the FUSE backend and (Phase 2) the WinFSP backend.
pub mod core;

// Re-export shared types for use by fuse3 and public API
pub use types::{DdsHandler, DdsRequest, DdsResponse};
