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
// the WinFSP backend), wired in by docs/dev/windows-backend-split.md §6
// commits 2–4. (A couple of pure API-surface helpers in `node` carry their
// own scoped `#[allow(dead_code)]` until the WinFSP backend uses them.)
pub mod attrs;
pub mod error;
pub mod node;
pub mod requestor;

/// The backend-neutral ortho union core: state, builders, and accessors
/// shared by the FUSE backend and (Phase 2) the WinFSP backend.
pub mod core;

// Re-export shared types for use by fuse3 and public API
pub use types::{DdsHandler, DdsRequest, DdsResponse};
