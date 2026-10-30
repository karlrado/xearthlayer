//! WinFSP mount-lifecycle smoke test (Phase 2 commit 5; design
//! `docs/dev/winfsp-backend.md` §6.5).
//!
//! Mounts the real ortho union backend over installed ortho packages and
//! verifies the headless part of the checklist: the volume appears at a
//! fresh mountpoint, the package root lists, a real passthrough file reads
//! byte-for-byte identical to the on-disk original, and the clean unmount
//! removes the mountpoint directory (WinFSP creates and removes it —
//! spike F1). It also exercises the platform probe end to end:
//! `system::unmount::is_path_mounted` is `true` on the live mount and
//! `false` after unmount (the registry is empty here, so the
//! `GetVolumeInformation` name check carries it).
//!
//! Virtual DDS reads answer with the core's placeholder under this test
//! (no DDS executor runs); the operator's interactive part — browsing the
//! mount in Explorer and opening a real file — is the printed checklist.
//!
//! Usage: `winfsp_smoke <packages_root> <mountpoint>`
//!
//! * `<packages_root>` - a directory containing installed scenery packages
//!   (e.g. an X-Plane install's `Custom Scenery`)
//! * `<mountpoint>` - a path that must **not** exist yet: WinFSP creates
//!   it on mount and removes it on unmount (spike F1)
//!
//! Windows in effect: it needs the WinFSP runtime and mounts a folder. On
//! other platforms it compiles to a no-op `main` — CI builds the example
//! on every OS but never runs it.

fn main() {
    #[cfg(windows)]
    smoke::run();

    #[cfg(not(windows))]
    {
        println!("winfsp_smoke is Windows-only (it needs the WinFSP runtime); nothing to do here.");
    }
}

/// The Windows smoke test, compiled only where the WinFSP bindings exist.
#[cfg(windows)]
mod smoke {
    use std::io::BufRead;
    use std::path::Path;
    use std::process;
    use std::sync::Arc;

    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;
    use xearthlayer::coord::TileCoord;
    use xearthlayer::executor::{DdsClient, DdsClientError, Priority};
    use xearthlayer::fuse::winfsp::WinFspOrthoUnionFS;
    use xearthlayer::fuse::EXPECTED_DDS_SIZE;
    use xearthlayer::manager::LocalPackageStore;
    use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
    use xearthlayer::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use xearthlayer_package::PackageType;
    use xearthlayer_package::{
        InstalledPackage as PackageInstalledPackage, Package as PackageCore,
    };

    /// A `DdsClient` that serves nothing: no DDS executor runs under the
    /// smoke test, so virtual DDS reads fall back to the core's placeholder.
    /// Real passthrough files never touch it.
    struct NullDdsClient;

    impl DdsClient for NullDdsClient {
        fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
            Err(DdsClientError::ChannelClosed)
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (_tx, rx) = oneshot::channel();
            rx
        }

        fn request_dds_with_options(
            &self,
            _tile: TileCoord,
            _priority: Priority,
            _origin: RequestOrigin,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (_tx, rx) = oneshot::channel();
            rx
        }

        fn is_connected(&self) -> bool {
            false
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let (Some(packages_root), Some(mountpoint)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: winfsp_smoke <packages_root> <mountpoint>");
            eprintln!(
                "  <packages_root> a directory of installed scenery packages (e.g. an X-Plane Custom Scenery)"
            );
            eprintln!(
                "  <mountpoint>    a path that must NOT exist yet (WinFSP creates and removes it)"
            );
            process::exit(2);
        };
        let packages_root = Path::new(packages_root);
        let mountpoint = Path::new(mountpoint);

        if mountpoint.exists() {
            eprintln!(
                "mountpoint must not pre-exist (WinFSP creates it — spike F1): {}",
                mountpoint.display()
            );
            process::exit(2);
        }

        // 1) Discover the installed ortho packages under the root.
        let store = LocalPackageStore::new(packages_root);
        let packages = store
            .list()
            .expect("list installed packages")
            .into_iter()
            .filter(|pkg| pkg.package_type() == PackageType::Ortho)
            .collect::<Vec<_>>();
        if packages.is_empty() {
            eprintln!("no ortho packages found under {}", packages_root.display());
            process::exit(1);
        }
        println!(
            "PACKAGES=OK {} ortho (first: {} {})",
            packages.len(),
            packages[0].region(),
            packages[0].version()
        );

        // 2) Build the union index over them, as the production mount flow
        //    does (manager/mounts.rs).
        let mut builder = OrthoUnionIndexBuilder::new();
        for pkg in &packages {
            let core = PackageCore::new(pkg.region(), pkg.package_type(), pkg.version().clone());
            builder = builder.add_package(PackageInstalledPackage::new(core, &pkg.path));
        }
        let index = builder.build().expect("build ortho union index");
        println!(
            "INDEX=OK {} source(s), {} file(s)",
            index.source_count(),
            index.file_count()
        );

        // 2b) Choose the passthrough read target before the mount consumes
        //     the index: the first indexed regular file. The union root only
        //     exposes the scenery directories (`Earth nav data/`,
        //     `terrain/`, `textures/`) — root-level files like the package
        //     manifest are deliberately filtered out — so the target must
        //     come from inside one of them.
        let passthrough: (std::path::PathBuf, std::path::PathBuf) = index
            .files()
            .find(|(_, source)| source.real_path.is_file())
            .map(|(virtual_path, source)| (virtual_path.clone(), source.real_path.clone()))
            .expect("at least one indexed regular file to read through the mount");
        println!(
            "TARGET=OK {} ({} bytes on disk)",
            passthrough.0.display(),
            std::fs::metadata(&passthrough.1)
                .expect("passthrough metadata")
                .len()
        );

        // 3) Mount through the commit-5 builder/session (declared before the
        //    runtime? no — the runtime first, so it outlives the session's
        //    handle: locals drop in reverse declaration order).
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let session = WinFspOrthoUnionFS::new(index, Arc::new(NullDdsClient), EXPECTED_DDS_SIZE)
            .with_runtime_handle(runtime.handle().clone())
            .mount(mountpoint)
            .unwrap_or_else(|e| {
                eprintln!("mount failed: {e}");
                process::exit(1);
            });
        println!("MOUNT=OK {}", mountpoint.display());

        // 4) Headless checks.
        // 4a) The volume appears where the driver says it should.
        let entries: Vec<String> = std::fs::read_dir(mountpoint)
            .expect("list mount root")
            .map(|e| {
                e.expect("read entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        println!(
            "LIST=OK {} entries (first 5: {:?})",
            entries.len(),
            &entries[..entries.len().min(5)]
        );

        // 4b) The chosen indexed file — a real passthrough, never generated
        //     under this test — must read byte-for-byte as the on-disk
        //     original.
        let via_mount =
            std::fs::read(mountpoint.join(&passthrough.0)).expect("read file via the mount");
        let on_disk = std::fs::read(&passthrough.1).expect("read file on disk");
        assert_eq!(
            via_mount, on_disk,
            "passthrough read must be byte-identical to the on-disk original"
        );
        println!(
            "READ=OK {} ({} bytes, byte-identical to on-disk)",
            passthrough.0.display(),
            via_mount.len()
        );

        // 4c) The platform probe agrees the path is mounted. The session is
        //     not in the in-process registry (this test never registered
        //     it), so `is_path_mounted` must be carried by the reparse-point
        //     check on the mountpoint — the end-to-end test of that
        //     classification.
        assert!(
            xearthlayer::system::unmount::is_path_mounted(mountpoint),
            "is_path_mounted must be true on the live mount"
        );
        println!("PROBE=OK is_path_mounted on the live mount");

        // 5) Interactive part (the operator's checklist).
        println!();
        println!("=== INTERACTIVE CHECK — do this in Explorer, then press Enter here ===");
        println!(
            "1. Open {} in Explorer: the package root lists (folders and files).",
            mountpoint.display()
        );
        println!("2. Come back here and press Enter.");
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .expect("read Enter");

        // 6) Clean unmount; the driver removes the mountpoint directory (F1).
        session.unmount();
        println!("UNMOUNT=OK");
        assert!(
            !mountpoint.exists(),
            "the mountpoint directory must be gone after unmount"
        );
        assert!(
            !xearthlayer::system::unmount::is_path_mounted(mountpoint),
            "is_path_mounted must be false after unmount"
        );
        println!("CLEANUP=OK mountpoint removed, probe now false");
        println!("SMOKE=PASS");
    }
}
