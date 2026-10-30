//! Vendored build script for `winfsp-sys` 0.12.1.
//!
//! The published crate's build script runs bindgen at build time, which
//! requires libclang. This copy instead uses the crate's own pre-generated
//! bindings (`src/bindings.rs`, produced by rust-bindgen 0.72.0 — the same
//! version the published build script pins) and emits only the link
//! directives. See README.md.

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // Bundled import libraries (winfsp-x64.lib, ...); no system WinFSP
    // installation is needed to link.
    println!("cargo:rustc-link-search={}/winfsp/lib", manifest_dir);

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" {
        panic!("WinFSP is only supported on Windows.");
    }

    // WinFSP ships one import library per MSVC architecture (mirrors the
    // published crate's triple table).
    let winfsp_lib = match (target_arch.as_str(), target_env.as_str()) {
        ("x86_64", "msvc") => "winfsp-x64",
        ("x86", "msvc") => "winfsp-x86",
        ("aarch64", "msvc") => "winfsp-a64",
        _ => panic!("unsupported target {target_os}-{target_arch}-{target_env}"),
    };

    // Kept from upstream for fidelity. In practice `rustc-link-arg` from a
    // transitive dependency's build script does not reach the final link, so
    // downstream binaries carry an immediate import of the driver DLL and
    // must find it on their DLL search path at startup (see README.md).
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-lib=dylib={winfsp_lib}");
    println!("cargo:rustc-link-arg=/DELAYLOAD:{winfsp_lib}.dll");
}
