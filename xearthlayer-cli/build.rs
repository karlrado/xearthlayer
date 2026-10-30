fn main() {
    // WinFSP delay-load (Phase 2, docs/dev/winfsp-backend.md §2): the
    // `xearthlayer` binary must start where WinFSP is not installed, so
    // winfsp-x64.dll is delay-loaded instead of imported statically.
    // This is the same directive `xearthlayer`'s build.rs emits through
    // the crate helper (`winfsp::build::winfsp_link_delayload`, which
    // this package deliberately does not take as a build-dependency — it
    // only prints these two lines); the bare `rustc-link-arg` from that
    // package cannot reach this bin, since cargo scopes it to the
    // emitting package's own targets. The arch → DLL mapping mirrors the
    // helper's; host == target on every build we run (local + CI).
    #[cfg(windows)]
    {
        let dll = match std::env::consts::ARCH {
            "x86_64" => "winfsp-x64.dll",
            "x86" => "winfsp-x86.dll",
            "aarch64" => "winfsp-a64.dll",
            other => panic!("unsupported WinFSP architecture: {other}"),
        };
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
    }
}
