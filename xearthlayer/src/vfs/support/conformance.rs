//! The neutral conformance suite.
//!
//! This is the one contract test suite that runs against every adapter: the
//! FUSE adapters today, `WinFsp` once that backend lands, and `FsKit` on
//! macOS in the future. It drives only the [`UnionFilesystem`] operations and
//! asserts on the neutral types, so by construction it carries no `fuse3::`
//! symbol, no inode, no kernel flag, and no caching policy. Each adapter's
//! test module builds a core from [`conformance_core`] and calls
//! [`run_conformance_suite`]; adding a backend is adding one invocation, not
//! a second copy of these tests.
//!
//! What stays out of this suite, deliberately:
//!
//! - **White-box checks** — zero-copy aliasing, budget refusal, peak gauges,
//!   and DDS request counts. They reach into core internals or the test
//!   double, so they are not part of the neutral contract.
//! - **Adapter-translation checks** — FUSE reply flags, the `readdirplus`
//!   size upgrade, and the macFUSE volume-name option. These test how a
//!   specific kernel sees the union, which is exactly what a per-backend test
//!   module is for.
//!
//! Those live in each adapter's own `mod tests`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xearthlayer_package::{InstalledPackage, Package, PackageType};

use crate::coord::TileCoord;
use crate::executor::{DdsClient, DdsClientError, Priority};
use crate::night;
use crate::ortho_union::OrthoUnionIndexBuilder;
use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
use crate::vfs::support::attrs::NodeKind;
use crate::vfs::support::core::OrthoUnionCore;
use crate::vfs::support::error::FsError;
use crate::vfs::support::node::FsNode;
use crate::vfs::support::union::UnionFilesystem;
use crate::vfs::{TextureKind, EXPECTED_DDS_SIZE};

/// A `DdsClient` test double for the conformance fixture.
///
/// It answers every DDS request with a valid tile immediately, so the suite
/// can drive the virtual-DDS read path without an executor. The neutral
/// contract does not observe request counts, so this is deliberately simpler
/// than the counting client the FUSE white-box tests use.
struct ConformanceDdsClient;

impl ConformanceDdsClient {
    /// A DDS that passes `validate_dds_or_placeholder`: the right magic, the
    /// right length, and a body whose byte at index `i` is `i as u8`, so a
    /// read's window proves which range was served.
    fn valid_dds() -> Vec<u8> {
        let mut data: Vec<u8> = (0..EXPECTED_DDS_SIZE).map(|i| i as u8).collect();
        data[0..4].copy_from_slice(b"DDS ");
        data
    }
}

impl DdsClient for ConformanceDdsClient {
    fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
        Ok(())
    }

    fn request_dds(
        &self,
        _tile: TileCoord,
        _cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(DdsResponse::cache_hit(
            Self::valid_dds().into(),
            Duration::from_millis(1),
        ));
        rx
    }

    fn request_dds_with_options(
        &self,
        tile: TileCoord,
        _priority: Priority,
        _origin: RequestOrigin,
        cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        self.request_dds(tile, cancellation)
    }

    fn is_connected(&self) -> bool {
        true
    }
}

/// The shared conformance fixture: one core over an index that covers the
/// whole neutral contract.
///
/// The layout is a single `na_ortho` package holding a real passthrough DSF,
/// a real terrain file, and a water-mask PNG. The `textures` / `terrain` /
/// `Earth nav data` directories are recognised by the index, so the generable
/// ortho and lit names resolve without being present on disk.
///
/// The returned `TempDir` must outlive the core: real files are read from it.
pub fn conformance_core() -> (OrthoUnionCore, TempDir) {
    let temp = TempDir::new().unwrap();
    let pkg_dir = temp.path().join("na_ortho");

    // A real passthrough file of known content and size (byte `i` is `i % 251`).
    let dsf_dir = pkg_dir.join("Earth nav data/+40-080");
    std::fs::create_dir_all(&dsf_dir).unwrap();
    let dsf: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(dsf_dir.join("+40-074.dsf"), &dsf).unwrap();

    // A real terrain file, so a directory listing has a regular file entry.
    let terrain_dir = pkg_dir.join("terrain");
    std::fs::create_dir_all(&terrain_dir).unwrap();
    std::fs::write(terrain_dir.join("+40-074_sea.ter"), b"terrain data").unwrap();

    // A water-mask PNG on disk: its paired `.dds` name must be reported
    // missing so X-Plane falls back to the PNG (the water-mask guard).
    let textures_dir = pkg_dir.join("textures");
    std::fs::create_dir_all(&textures_dir).unwrap();
    std::fs::write(textures_dir.join("99999_88888_BI16.png"), b"fake png").unwrap();

    let pkg = InstalledPackage::new(
        Package::new("na", PackageType::Ortho, Version::new(1, 0, 0)),
        &pkg_dir,
    );
    let index = OrthoUnionIndexBuilder::new()
        .add_package(pkg)
        .build()
        .unwrap();

    let core = OrthoUnionCore::new(index, Arc::new(ConformanceDdsClient), EXPECTED_DDS_SIZE);
    (core, temp)
}

/// Drive the neutral contract against `fs`, the union view as exposed by
/// whatever adapter is under test.
pub async fn run_conformance_suite(fs: &impl UnionFilesystem) {
    check_lookup(fs).await;
    check_attrs(fs).await;
    check_read_real_file(fs).await;
    check_read_ortho_dds(fs).await;
    check_read_lit(fs).await;
    check_open_release(fs).await;
    check_list_directory(fs).await;
    check_statfs(fs);
}

/// `lookup` resolves each node kind to the right neutral identity, and reports
/// missing names as `NotFound` rather than an I/O error.
async fn check_lookup(fs: &impl UnionFilesystem) {
    let root = FsNode::root();

    // A directory in the union view.
    let nav = fs.lookup(&root, "Earth nav data").await.unwrap();
    assert!(
        nav.is_directory(),
        "a union directory resolves to a directory node"
    );
    let dir = fs.lookup(&nav, "+40-080").await.unwrap();
    assert!(dir.is_directory());

    // A real file from a source.
    let dsf = fs.lookup(&dir, "+40-074.dsf").await.unwrap();
    assert!(
        matches!(dsf, FsNode::RealFile(_)),
        "a source file resolves to a real-file node"
    );

    // A generable ortho texture resolves to a virtual DDS node.
    let textures = fs.lookup(&root, "textures").await.unwrap();
    assert!(textures.is_directory());
    let ortho = fs.lookup(&textures, "12754_5279_BI16.dds").await.unwrap();
    assert!(
        matches!(ortho, FsNode::VirtualDds(c) if c.kind == TextureKind::Ortho),
        "a generable ortho name resolves to an ortho virtual DDS"
    );

    // A lit name resolves to a virtual DDS node of the lit kind.
    let lit = fs
        .lookup(&textures, "25712_12960_BI16_LIT.dds")
        .await
        .unwrap();
    assert!(
        matches!(lit, FsNode::VirtualDds(c) if c.kind == TextureKind::Lit),
        "a lit name resolves to a lit virtual DDS"
    );

    // A name that is nowhere is missing.
    assert!(
        matches!(
            fs.lookup(&root, "does_not_exist").await,
            Err(FsError::NotFound)
        ),
        "an unknown name is not found"
    );

    // The water-mask guard: a `.png` on disk makes the paired `.dds` missing,
    // so X-Plane falls back to the PNG instead of satellite imagery.
    assert!(
        matches!(
            fs.lookup(&textures, "99999_88888_BI16.dds").await,
            Err(FsError::NotFound)
        ),
        "a water-masked .dds is not found"
    );
    // ...while the PNG itself is a real file.
    let png = fs.lookup(&textures, "99999_88888_BI16.png").await.unwrap();
    assert!(
        matches!(png, FsNode::RealFile(_)),
        "the water-mask PNG is a real file"
    );
}

/// `attrs` reports each node kind in the union's terms: real files their
/// on-disk size, virtual DDS their configured size, and directories as such.
async fn check_attrs(fs: &impl UnionFilesystem) {
    let root = FsNode::root();
    let textures = fs.lookup(&root, "textures").await.unwrap();

    // A real file reports its on-disk size and kind.
    let nav = fs.lookup(&root, "Earth nav data").await.unwrap();
    let dir = fs.lookup(&nav, "+40-080").await.unwrap();
    let dsf = fs.lookup(&dir, "+40-074.dsf").await.unwrap();
    let attrs = fs.attrs(&dsf).await.unwrap();
    assert_eq!(
        attrs.size,
        100 * 1024,
        "a real file reports its on-disk size"
    );
    assert_eq!(attrs.kind, NodeKind::RegularFile);

    // An ortho virtual DDS reports the configured expected size.
    let ortho = fs.lookup(&textures, "12754_5279_BI16.dds").await.unwrap();
    let attrs = fs.attrs(&ortho).await.unwrap();
    assert_eq!(
        attrs.size, EXPECTED_DDS_SIZE as u64,
        "an ortho DDS reports the expected size"
    );

    // A lit virtual DDS reports the lit source's size (black by default).
    let lit = fs
        .lookup(&textures, "25712_12960_BI16_LIT.dds")
        .await
        .unwrap();
    let attrs = fs.attrs(&lit).await.unwrap();
    assert_eq!(
        attrs.size,
        night::minimal_black_dds().len() as u64,
        "a lit DDS reports the lit size"
    );

    // The root is a directory.
    let attrs = fs.attrs(&root).await.unwrap();
    assert_eq!(attrs.kind, NodeKind::Directory);

    // A path that resolves nowhere is missing, not an I/O error.
    let missing = FsNode::RealFile(PathBuf::from("no_such_file.dsf"));
    assert!(
        matches!(fs.attrs(&missing).await, Err(FsError::NotFound)),
        "a missing node is not found"
    );
}

/// A ranged read of a real file returns exactly the requested window, short at
/// the end and empty at or past it, and serves the file's own bytes.
async fn check_read_real_file(fs: &impl UnionFilesystem) {
    let root = FsNode::root();
    let nav = fs.lookup(&root, "Earth nav data").await.unwrap();
    let dir = fs.lookup(&nav, "+40-080").await.unwrap();
    let dsf = fs.lookup(&dir, "+40-074.dsf").await.unwrap();

    // A ranged read returns exactly the requested window.
    let data = fs.read(&dsf, 0, 0, 1024).await.unwrap();
    assert_eq!(
        data.len(),
        1024,
        "a ranged read returns the requested length"
    );
    // Byte `i` of the fixture is `i % 251`, so the content proves the window.
    assert_eq!(data[0], 0u8, "the first byte of the window");
    assert_eq!(
        data[1023],
        (1023 % 251) as u8,
        "the last byte of the window"
    );

    // A range spanning the end is a short read, not an error.
    let size = 100 * 1024u64;
    let tail = fs.read(&dsf, 0, size - 50, 1000).await.unwrap();
    assert_eq!(tail.len(), 50, "a range past the end is a short read");

    // At or past the end the read is empty.
    let past = fs.read(&dsf, 0, size, 4096).await.unwrap();
    assert!(past.is_empty(), "a read at or past the end is empty");
}

/// A virtual ortho DDS open gets a memoising handle, and its reads serve the
/// requested window of the tile, short at the tile end and empty past it.
async fn check_read_ortho_dds(fs: &impl UnionFilesystem) {
    let root = FsNode::root();
    let textures = fs.lookup(&root, "textures").await.unwrap();
    let ortho = fs.lookup(&textures, "12754_5279_BI16.dds").await.unwrap();
    let fh = fs.open(&ortho).unwrap();
    assert_ne!(fh, 0, "an ortho DDS open gets a memoising handle");

    let size = EXPECTED_DDS_SIZE as u64;

    // A ranged read serves the requested window of the tile.
    let data = fs.read(&ortho, fh, 4096, 256).await.unwrap();
    assert_eq!(
        data.len(),
        256,
        "a ranged read returns the requested length"
    );
    assert_eq!(data[0], (4096 % 256) as u8, "the first byte of the window");
    assert_eq!(
        data[255],
        ((4096 + 255) % 256) as u8,
        "the last byte of the window"
    );

    // The last window is short; past the tile end is empty.
    let tail = fs.read(&ortho, fh, size - 100, 4096).await.unwrap();
    assert_eq!(tail.len(), 100, "a range past the tile end is a short read");
    let past = fs.read(&ortho, fh, size, 4096).await.unwrap();
    assert!(past.is_empty(), "a read at or past the tile end is empty");

    fs.release(fh).unwrap();
}

/// A lit texture is served from the lit source alone: black by default, and
/// without any DDS generation.
async fn check_read_lit(fs: &impl UnionFilesystem) {
    let root = FsNode::root();
    let textures = fs.lookup(&root, "textures").await.unwrap();
    let lit = fs
        .lookup(&textures, "25712_12960_BI16_LIT.dds")
        .await
        .unwrap();

    let data = fs.read(&lit, 0, 0, 4096).await.unwrap();
    assert_eq!(
        data,
        night::minimal_black_dds(),
        "a lit read serves the lit source"
    );
}

/// `open` hands a per-open handle only to a virtual ortho DDS; lit textures
/// and real files use the kernel's default caching and get handle `0`.
async fn check_open_release(fs: &impl UnionFilesystem) {
    let root = FsNode::root();
    let textures = fs.lookup(&root, "textures").await.unwrap();

    // An ortho virtual DDS open gets a non-zero memoising handle.
    let ortho = fs.lookup(&textures, "12754_5279_BI16.dds").await.unwrap();
    let fh = fs.open(&ortho).unwrap();
    assert_ne!(fh, 0, "an ortho DDS open gets a memoising handle");
    fs.release(fh).unwrap();

    // A lit texture holds no per-open state.
    let lit = fs
        .lookup(&textures, "25712_12960_BI16_LIT.dds")
        .await
        .unwrap();
    assert_eq!(
        fs.open(&lit).unwrap(),
        0,
        "a lit texture open gets no handle"
    );

    // A real passthrough file uses default kernel caching, no handle.
    let nav = fs.lookup(&root, "Earth nav data").await.unwrap();
    let dir = fs.lookup(&nav, "+40-080").await.unwrap();
    let dsf = fs.lookup(&dir, "+40-074.dsf").await.unwrap();
    assert_eq!(fs.open(&dsf).unwrap(), 0, "a real file open gets no handle");
}

/// `list_directory` returns `.`, `..`, and the union's entries, and the root's
/// `..` points back at the root.
async fn check_list_directory(fs: &impl UnionFilesystem) {
    let root = FsNode::root();

    // The root lists itself, its parent, and its top-level sources.
    let entries = fs.list_directory(&root).await.unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."), "a directory lists itself: {names:?}");
    assert!(
        names.contains(&".."),
        "a directory lists its parent: {names:?}"
    );
    assert!(
        names.contains(&"Earth nav data"),
        "the union's sources are listed: {names:?}"
    );

    // The root's `..` is the root itself.
    let dotdot = entries
        .iter()
        .find(|e| e.name == "..")
        .expect("the .. entry");
    assert_eq!(dotdot.node, FsNode::root(), "the root's .. is the root");

    // A real subdirectory lists its files.
    let nav = fs.lookup(&root, "Earth nav data").await.unwrap();
    let dir = fs.lookup(&nav, "+40-080").await.unwrap();
    let entries = fs.list_directory(&dir).await.unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"+40-074.dsf"),
        "a subdirectory lists its files: {names:?}"
    );
}

/// `statfs` reports the union's file count and the documented block figures.
fn check_statfs(fs: &impl UnionFilesystem) {
    let info = fs.statfs();

    assert!(
        info.files > 0,
        "the union reports its file count: {}",
        info.files
    );
    assert_eq!(info.bsize, 4096, "the block size is documented");
    assert_eq!(
        info.frsize, 4096,
        "the fundamental block size is documented"
    );
    assert_eq!(info.namelen, 255, "the name length is documented");
    assert_eq!(info.bavail, 0, "no free blocks are reported");
    assert_eq!(info.ffree, 0, "no free inodes are reported");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite runs against the core directly: a stand-in for the invocation
    /// each adapter's test module makes.
    #[tokio::test]
    async fn the_suite_runs_against_the_core() {
        let (core, _temp) = conformance_core();
        run_conformance_suite(&core).await;
    }
}
