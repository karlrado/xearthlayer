#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::missing_safety_doc)]

// The published crate generates this file at build time with bindgen (which
// requires libclang). This vendored copy uses the crate's own pre-generated
// version verbatim, so no code generation happens at all. See README.md.
include!("bindings.rs");

#[allow(non_camel_case_types)]
pub type FILE_ACCESS_RIGHTS = u32;
#[allow(non_camel_case_types)]
pub type FILE_FLAGS_AND_ATTRIBUTES = u32;
