//! Live-mount suite: real kernel FUSE, end to end, through the mount seam.
//!
//! Mounts the production `OrthoUnionCore` behind the seam's types —
//! `select_backend().mount(...)` — exactly as `xearthlayer run` does, then
//! drives it from ordinary file operations on the mountpoint: kernel →
//! fuse3 → adapter → core. This is the suite the backend split exists to
//! make possible: it names no backend, so the same file exercises Linux
//! (`LinuxFuse`, `fusermount3`) and macOS (`MacFuse`, macFUSE).
//!
//! Requires `/dev/fuse` + `fusermount3` (Linux) or macFUSE installed and the
//! kext loaded (macOS). Every test is `#[ignore]`d: run locally with
//! `make live-fuse`; CI runs it as a non-blocking probe step.
//!
//! Run this after bumping the pinned fuse3 fork revision or changing the
//! mount/session code.

use std::sync::Arc;
use std::time::Duration;

use xearthlayer::ortho_union::OrthoUnionIndex;
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::vfs::backend::{select_backend, FuseLimits, MountRequest};
use xearthlayer::vfs::common::conformance::FixtureDdsClient;
use xearthlayer::vfs::common::{OrthoUnionCore, UnionFilesystem};

fn fuse_limits() -> FuseLimits {
    FuseLimits {
        max_background: 12,
        congestion_threshold: 9,
    }
}

/// Mount and immediately drop: the environment probe. If GitHub runners (or
/// a developer box) cannot do unprivileged FUSE mounts, this fails first and
/// cheaply — every other test in this file would fail identically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires /dev/fuse + fusermount3 (Linux) or macFUSE (macOS)"]
async fn environment_supports_live_mounts() {
    let temp = tempfile::tempdir().expect("mountpoint tempdir");
    let fs: Arc<dyn UnionFilesystem> = Arc::new(OrthoUnionCore::new(
        OrthoUnionIndex::default(),
        Arc::new(FixtureDdsClient::new()),
        xearthlayer::vfs::EXPECTED_DDS_SIZE,
    ));
    let request = MountRequest {
        mountpoint: temp.path().to_path_buf(),
        fuse_limits: fuse_limits(),
    };
    let session = select_backend()
        .mount(fs, request)
        .await
        .expect("live mount (environment probe)");
    assert!(session.is_mounted(), "the probe mount must be live");
    drop(session);
    assert!(
        !xearthlayer::vfs::fuse3::is_path_mounted(temp.path()),
        "dropping the session must unmount"
    );
}

/// Read-through of a real mountpoint: a real passthrough file, a directory
/// listing, and a virtual DDS generated on demand through the DDS client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires /dev/fuse + fusermount3 (Linux) or macFUSE (macOS)"]
async fn live_mount_serves_real_files_readdir_and_virtual_dds() {
    let patches = tempfile::tempdir().expect("patches tempdir");
    let patch_dir = patches.path().join("TestPatch");
    let dsf_rel = "Earth nav data/+30-120/+33-119.dsf";
    let dsf_contents = b"fake dsf payload for the live mount suite".to_vec();
    std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
    std::fs::write(patch_dir.join(dsf_rel), &dsf_contents).unwrap();
    std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();
    std::fs::write(patch_dir.join("terrain/test.ter"), b"fake terrain").unwrap();
    // Real worlds always ship a textures/ directory; it is lazy-resolved
    // against the sources on disk, and the virtual DDS lookup needs the
    // intermediate `textures` path to exist somewhere (as in production).
    std::fs::create_dir_all(patch_dir.join("textures")).unwrap();
    std::fs::write(patch_dir.join("textures/dummy.png"), b"png-not-really").unwrap();

    let index = OrthoUnionIndexBuilder::new()
        .with_patches_dir(patches.path())
        .build()
        .expect("build ortho union index");
    assert!(index.file_count() > 0, "index must have scanned files");

    let client = Arc::new(FixtureDdsClient::new());
    let expected_dds = client.payload().clone();
    let fs: Arc<dyn UnionFilesystem> = Arc::new(OrthoUnionCore::new(
        index,
        client.clone(),
        xearthlayer::vfs::EXPECTED_DDS_SIZE,
    ));

    let mountpoint = tempfile::tempdir().expect("mountpoint tempdir");
    let request = MountRequest {
        mountpoint: mountpoint.path().to_path_buf(),
        fuse_limits: fuse_limits(),
    };
    let session = select_backend()
        .mount(fs, request)
        .await
        .expect("ortho union live mount");

    tokio::time::sleep(Duration::from_millis(250)).await;

    // 1. A real scenery file reads back byte-identical through the mount.
    let read_back = std::fs::read(mountpoint.path().join(dsf_rel))
        .expect("read scenery file through the live mount");
    assert_eq!(
        read_back, dsf_contents,
        "passthrough bytes must match source"
    );

    // 2. The root listing exposes the patch's top-level directory.
    let names: Vec<String> = std::fs::read_dir(mountpoint.path())
        .expect("readdir live mountpoint")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "Earth nav data"),
        "root listing must show the patch directory, got {names:?}"
    );

    // 3. A virtual DDS file: never on disk anywhere, generated through the
    //    DDS client and served at the exact fixture payload.
    let dds_read = std::fs::read(mountpoint.path().join("textures/25712_12960_BI16.dds"))
        .expect("read a virtual DDS through the live mount");
    assert_eq!(dds_read.len(), expected_dds.len(), "full DDS chain served");
    assert_eq!(
        dds_read.as_slice(),
        &expected_dds[..],
        "fixture payload bytes"
    );
    assert_eq!(client.requests(), 1, "one generation per demand");

    session
        .unmount()
        .await
        .expect("explicit unmount through the seam");
    assert!(
        !xearthlayer::vfs::fuse3::is_path_mounted(mountpoint.path()),
        "unmount() must leave the mount table clean"
    );
}

/// #305: Finder shows a mounted volume's name in place of its mount-point
/// folder; without `volname` the mount appears as `macFUSE Volume 0
/// (<process>)` instead of `zzXEL_ortho`. macOS-only behaviour of the
/// mount OPTION — the test compiles everywhere and answers on every host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires /dev/fuse + fusermount3 (Linux) or macFUSE (macOS)"]
async fn volume_name_follows_the_mount_point_folder() {
    #[cfg(target_os = "macos")]
    {
        let patches = tempfile::tempdir().expect("patches tempdir");
        std::fs::create_dir_all(patches.path().join("TestPatch/terrain")).unwrap();
        std::fs::write(patches.path().join("TestPatch/terrain/test.ter"), b"t").unwrap();
        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(patches.path())
            .build()
            .expect("build ortho union index");

        let scenery = tempfile::tempdir().expect("custom scenery tempdir");
        let mountpoint = scenery.path().join("zzXEL_ortho");
        std::fs::create_dir(&mountpoint).unwrap();

        let fs: Arc<dyn UnionFilesystem> = Arc::new(OrthoUnionCore::new(
            index,
            Arc::new(FixtureDdsClient::new()),
            xearthlayer::vfs::EXPECTED_DDS_SIZE,
        ));
        let request = MountRequest {
            mountpoint: mountpoint.clone(),
            fuse_limits: fuse_limits(),
        };
        let session = select_backend()
            .mount(fs, request)
            .await
            .expect("ortho union live mount");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let name = volume_name(&mountpoint);
        session.unmount().await.expect("union unmount");

        assert_eq!(name, "zzXEL_ortho");
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("volname is a macFUSE mount option; nothing to check on this host");
    }
}

/// The volume name Finder shows for `path`, read with `getattrlist`
/// (`ATTR_VOL_NAME`), which is what Foundation's `volumeNameKey` reads.
#[cfg(target_os = "macos")]
fn volume_name(path: &std::path::Path) -> String {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_NAME,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // u32 total length, then the attrreference_t, then the name it points at.
    let mut buf = [0u8; 1024];
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&mut request as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
        )
    };
    assert_eq!(rc, 0, "getattrlist: {}", std::io::Error::last_os_error());

    const REF_AT: usize = 4;
    let offset = i32::from_ne_bytes(buf[REF_AT..REF_AT + 4].try_into().unwrap()) as usize;
    let name = &buf[REF_AT + offset..];
    CStr::from_bytes_until_nul(name)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
