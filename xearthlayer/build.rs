fn main() {
    // intel_tex_2 links a static ISPC library that requires the C++ runtime.
    // The crate's build script doesn't emit this linkage, so we add it here.
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=stdc++");

    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=c++");

    // WinFSP backend (Phase 2, docs/dev/winfsp-backend.md §2): the crate
    // helper (winfsp::build, `delayload` feature) emits the `delayimp`
    // link-lib and a bare `rustc-link-arg=/DELAYLOAD:winfsp-*.dll`.
    // The bare form applies only to this package's own targets — the
    // smoke/spike examples — because cargo rejects bin-scoped
    // `rustc-link-arg-bins` from a package without a bin target; the
    // downstream `xearthlayer` binary (package `xearthlayer-cli`) gets
    // the same directive from its own `build.rs`. Gated on the host OS:
    // the helper reads CARGO_CFG_TARGET_*, and host == target on every
    // build we run (local + the three CI jobs).
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
