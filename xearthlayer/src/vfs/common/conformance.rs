//! Backend-neutral conformance suite for the [`UnionFilesystem`] contract.
//!
//! The contract ([`super::union`]) is the seam every mount backend drives;
//! this module pins down what "driving it correctly" answers mean, so core
//! behaviour is locked once here rather than re-litigated inside each
//! backend. [`check_union_filesystem`] names no kernel: it speaks only in
//! [`FsNode`]s, [`FsError`]s and byte equality, and the test at the bottom
//! runs it against [`OrthoUnionCore`](super::core) through an
//! `Arc<dyn UnionFilesystem>`. A future backend (WinFSP, FSKit) runs its own
//! object — the same core, or a re-hosted one — through the same battery.
//!
//! # The four `0`-handle cases
//!
//! [`UnionFilesystem::open`] lists four valid zero-handle answers. Three
//! appear directly on the neutral surface: real files (case 1), lit textures
//! (case 2) and the exhausted pinned budget (case 4 — the fixture's
//! [`PinnedTileBudget`] ceiling selects it, so the suite runs once per
//! policy). Case 3, "unknown virtual inodes", is a backend-identity notion:
//! at this level a virtual texture is named by its coordinates, not by an
//! inode, so the neutral shape of the case is *a handle the filesystem never
//! issued* — [`UnionFilesystem::read`] must still serve the tile (resolved
//! per call) rather than fail. The `NotFound` the contract names is what a
//! backend holding an inode table answers for an inode it cannot map; the
//! core never sees such a request (rehearsal finding F11 in
//! `docs/dev/filesystem-backend-split.md`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use semver::Version;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xearthlayer_package::{InstalledPackage, Package, PackageType};

use crate::cache::integrity::EXPECTED_DDS_SIZE;
use crate::coord::TileCoord;
use crate::executor::{DdsClient, DdsClientError, Priority};
use crate::night::minimal_black_dds;
use crate::ortho_union::{OrthoUnionIndex, OrthoUnionIndexBuilder};
use crate::pinned_budget::PinnedTileBudget;
use crate::runtime::DdsResponse;
use crate::vfs::{DdsFilename, TextureKind};

use super::attrs::NodeKind;
use super::error::FsError;
use super::node::FsNode;
use super::union::UnionFilesystem;

// =============================================================================
// Test doubles
// =============================================================================

/// A [`DdsClient`] that answers every tile request immediately with a fixed,
/// validation-passing DDS payload, counting the generations it served.
///
/// The counter is the point: memoisation and the per-call `fh = 0` path look
/// identical through [`UnionFilesystem::read`] alone, but not through the
/// question "N reads, how many tile generations".
pub struct FixtureDdsClient {
    requests: AtomicUsize,
    payload: Bytes,
}

impl Default for FixtureDdsClient {
    fn default() -> Self {
        // `DDS ` magic plus a fixed filler at the canonical size: exactly
        // what `validate_dds_or_placeholder` accepts, so the suite asserts
        // against these bytes rather than an opaque placeholder.
        let mut payload = Vec::with_capacity(EXPECTED_DDS_SIZE);
        payload.extend_from_slice(b"DDS ");
        payload.resize(EXPECTED_DDS_SIZE, 0xA5);
        Self {
            requests: AtomicUsize::new(0),
            payload: Bytes::from(payload),
        }
    }
}

impl FixtureDdsClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tile generations served so far (not reads).
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// The exact bytes every generation is answered with.
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }

    fn answer(&self) -> oneshot::Receiver<DdsResponse> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(DdsResponse::new(
            self.payload.clone(),
            false,
            Duration::ZERO,
            true,
        ));
        rx
    }
}

impl DdsClient for FixtureDdsClient {
    fn submit(&self, _request: crate::runtime::JobRequest) -> Result<(), DdsClientError> {
        // The contract drives demand requests only.
        Ok(())
    }

    fn request_dds(
        &self,
        _tile: TileCoord,
        _cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        self.answer()
    }

    fn request_dds_with_options(
        &self,
        _tile: TileCoord,
        _priority: Priority,
        _origin: crate::runtime::RequestOrigin,
        _cancellation: CancellationToken,
    ) -> oneshot::Receiver<DdsResponse> {
        self.answer()
    }

    fn is_connected(&self) -> bool {
        true
    }
}

// =============================================================================
// The fixture
// =============================================================================

/// The sources and shared services a filesystem under test is built from.
///
/// The union view is one tiny ortho package laid out as an installed world
/// would be:
///
/// ```text
/// <root>/test_ortho/
///     Earth nav data/+40-080/+40-074.dsf   fx.file_bytes — the real file
///     Earth nav data/+39-079/+39-074.dsf   sibling region (a directory child)
///     terrain/package.ter                  source validity
/// ```
///
/// The fixture owns no filesystem itself: the caller builds the object under
/// test from [`UnionFixture::build_index`], [`UnionFixture::client`] and
/// [`UnionFixture::budget`], and keeps whatever directory backs `root` alive
/// (in tests, a `TempDir`).
pub struct UnionFixture {
    /// Directory holding the `test_ortho` source written by [`Self::new`].
    pub root: PathBuf,
    /// The budget the filesystem under test must share
    /// (`OrthoUnionCore::with_pinned_budget`). Swapping this `Arc` for one
    /// with a tiny ceiling between runs selects the exhausted-budget policy
    /// (contract case 4) instead of memoisation.
    pub budget: Arc<PinnedTileBudget>,
    client: Arc<FixtureDdsClient>,
    /// Bytes of the fixture's real file, at `file_dir`/`file_name`. Every
    /// ranged read has an unambiguous expected answer against these.
    pub file_bytes: Vec<u8>,
    /// Expected DDS size the filesystem under test is configured with — the
    /// canonical tile size, matching [`FixtureDdsClient::payload`].
    pub dds_size: usize,
    /// Entries a freshly built fixture index reports (`StatFsInfo::files`).
    pub file_count: usize,
    /// Top-level union directory.
    pub top_dir: &'static str,
    /// Directory holding the real file (two levels below the root).
    pub file_dir: &'static str,
    /// Last component of `file_dir`.
    pub file_leaf: &'static str,
    pub file_name: &'static str,
    /// Sibling region directory of `file_leaf` (a listing kind check).
    pub dir_sibling: &'static str,
    /// Virtual ortho texture name (parsable, not on disk).
    pub dds_name: &'static str,
    /// Virtual lit texture name.
    pub lit_name: &'static str,
}

impl UnionFixture {
    /// Materialise the sources under `root` and probe the entry count.
    pub fn new(root: &Path) -> Self {
        let pkg = root.join("test_ortho");
        let dsf = pkg.join("Earth nav data/+40-080/+40-074.dsf");
        let sibling = pkg.join("Earth nav data/+39-079/+39-074.dsf");
        let ter = pkg.join("terrain/package.ter");
        for file in [&dsf, &sibling, &ter] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        }
        // Distinct-ish byte pattern so every ranged read has a unique answer.
        let file_bytes: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&dsf, &file_bytes).unwrap();
        std::fs::write(&sibling, b"sibling dsf").unwrap();
        std::fs::write(&ter, b"fixture terrain").unwrap();

        let mut fixture = Self {
            root: root.to_path_buf(),
            budget: Arc::new(PinnedTileBudget::new()),
            client: Arc::new(FixtureDdsClient::new()),
            file_bytes,
            dds_size: EXPECTED_DDS_SIZE,
            file_count: 0,
            top_dir: "Earth nav data",
            file_dir: "Earth nav data/+40-080",
            file_leaf: "+40-080",
            file_name: "+40-074.dsf",
            dir_sibling: "+39-079",
            dds_name: "12_34_BI12.dds",
            lit_name: "12_34_BI12_LIT.dds",
        };
        fixture.file_count = fixture.build_index().file_count();
        fixture
    }

    /// Scan the fixture's sources into the index the filesystem under test
    /// is built with. Repeatable and side-effect free.
    pub fn build_index(&self) -> OrthoUnionIndex {
        OrthoUnionIndexBuilder::new()
            .add_package(InstalledPackage::new(
                Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
                self.root.join("test_ortho"),
            ))
            .build()
            .expect("fixture sources scan")
    }

    pub fn client(&self) -> Arc<FixtureDdsClient> {
        Arc::clone(&self.client)
    }

    pub fn file_dir_node(&self) -> FsNode {
        FsNode::Directory(PathBuf::from(self.file_dir))
    }

    pub fn file_node(&self) -> FsNode {
        FsNode::RealFile(PathBuf::from(self.file_dir).join(self.file_name))
    }

    pub fn dds_node(&self) -> FsNode {
        FsNode::VirtualDds(DdsFilename {
            row: 12,
            col: 34,
            zoom: 12,
            map_type: "BI".to_string(),
            kind: TextureKind::Ortho,
        })
    }

    pub fn lit_node(&self) -> FsNode {
        FsNode::VirtualDds(DdsFilename {
            row: 12,
            col: 34,
            zoom: 12,
            map_type: "BI".to_string(),
            kind: TextureKind::Lit,
        })
    }
}

// =============================================================================
// The battery
// =============================================================================

/// Run the full [`UnionFilesystem`] contract battery against `fs`.
///
/// Assertions (panics with a named contract clause) on the first violation.
/// The handle-policy section adapts to the fixture's budget: when the
/// [`PinnedTileBudget`] can admit another memoised tile the suite requires
/// non-zero handles and one generation per open; when it cannot, the suite
/// requires case 4 (handle `0`, per-call generation, nothing pinned). Run
/// the suite once per budget policy to exercise all four `open` answers.
pub async fn check_union_filesystem(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    check_lookup_and_attrs(fs, fx).await;
    check_lookup_errors(fs, fx).await;
    check_listing(fs, fx).await;
    check_real_file_reads(fs, fx).await;
    check_virtual_handle_policy(fs, fx).await;
    check_unknown_handle_serves(fs, fx).await;
    check_lit_texture(fs, fx).await;
    check_release_zero(fs, fx).await;
    check_read_not_found(fs, fx).await;
    check_statfs(fs, fx);
}

/// `lookup` answers with node and attributes in one pass, and they agree
/// with a later `attrs` for the same node (contract §lookup).
async fn check_lookup_and_attrs(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let root_attrs = fs.attrs(&FsNode::root()).await.expect("root attrs");
    assert_eq!(root_attrs.kind, NodeKind::Directory, "root is a directory");

    let dir_entry = fs
        .lookup(&FsNode::root(), fx.top_dir)
        .await
        .expect("top directory resolves from root");
    assert_eq!(dir_entry.node, FsNode::Directory(PathBuf::from(fx.top_dir)));
    assert_eq!(dir_entry.attrs.kind, NodeKind::Directory);

    let entry = fs
        .lookup(&fx.file_dir_node(), fx.file_name)
        .await
        .expect("fixture file resolves");
    assert_eq!(entry.node, fx.file_node(), "lookup names the file node");
    assert_eq!(entry.attrs.kind, NodeKind::RegularFile);
    assert_eq!(
        entry.attrs.size,
        fx.file_bytes.len() as u64,
        "a real file reports its on-disk size, not the virtual DDS size"
    );

    let attrs = fs.attrs(&entry.node).await.expect("attrs of file node");
    assert_eq!(attrs, entry.attrs, "Entry.attrs == attrs(node)");

    // Virtual textures resolve from their name alone. Their attributes carry
    // fresh timestamps each call (there is no source to date), so agreement
    // is asserted on the load-bearing fields; a real file above must agree
    // field-for-field.
    let dds_entry = fs
        .lookup(&FsNode::root(), fx.dds_name)
        .await
        .expect("virtual DDS resolves from root");
    assert_eq!(
        dds_entry.node,
        fx.dds_node(),
        "lookup yields the texture's coordinates"
    );
    assert_eq!(dds_entry.attrs.kind, NodeKind::RegularFile);
    assert_eq!(
        dds_entry.attrs.size, fx.dds_size as u64,
        "virtual size is the configured expected DDS size"
    );
    let attrs = fs.attrs(&dds_entry.node).await.expect("virtual attrs");
    assert_eq!(attrs.kind, dds_entry.attrs.kind);
    assert_eq!(attrs.size, dds_entry.attrs.size);
    assert_eq!(attrs.perm, dds_entry.attrs.perm);
    assert_eq!(attrs.nlink, dds_entry.attrs.nlink);
}

/// Failure answers: a missing name is `NotFound`, and a `lookup` inside a
/// non-directory is `NotADirectory` (contract §lookup).
async fn check_lookup_errors(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    assert!(
        matches!(
            fs.lookup(&FsNode::root(), "nothing-here.xyz").await,
            Err(FsError::NotFound)
        ),
        "a name the union does not have is NotFound"
    );
    assert!(
        matches!(
            fs.lookup(&fx.file_node(), fx.file_name).await,
            Err(FsError::NotADirectory)
        ),
        "lookup inside a non-directory is NotADirectory"
    );
}

/// `list_directory` answers with real children only — no `.`/`..` — each
/// with its node and kind: directories as virtual directories, files with
/// placeholder attributes whose upgrade is the backend's business
/// (contract §list_directory).
async fn check_listing(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let top = FsNode::Directory(PathBuf::from(fx.top_dir));
    let listing = fs
        .list_directory(&top)
        .await
        .expect("listing the top directory");
    for child in [fx.file_leaf, fx.dir_sibling] {
        let names: Vec<String> = listing
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        let e = listing
            .iter()
            .find(|e| e.name == *child)
            .unwrap_or_else(|| panic!("{} missing from listing {:?}", child, names));
        assert_eq!(
            e.entry.node,
            FsNode::Directory(PathBuf::from(fx.top_dir).join(child))
        );
        assert_eq!(
            e.entry.attrs.kind,
            NodeKind::Directory,
            "directory children are directories"
        );
    }
    assert!(
        listing.iter().all(|e| e.name != "." && e.name != ".."),
        "the core lists real children only"
    );

    let listing = fs
        .list_directory(&fx.file_dir_node())
        .await
        .expect("listing the region directory");
    let e = listing
        .iter()
        .find(|e| e.name == fx.file_name)
        .expect("fixture file is listed");
    assert_eq!(e.entry.node, fx.file_node());
    assert_eq!(e.entry.attrs.kind, NodeKind::RegularFile);
    assert_eq!(
        e.entry.attrs.size, fx.dds_size as u64,
        "listed files carry placeholder attributes; the backend upgrades them"
    );

    assert!(
        matches!(
            fs.list_directory(&fx.file_node()).await,
            Err(FsError::NotADirectory)
        ),
        "listing a file is NotADirectory"
    );
}

/// Case 1: real files open with no handle state, and ranged reads agree
/// byte-for-byte with the file — full, mid, short at EOF, empty past it.
async fn check_real_file_reads(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let node = fx.file_node();
    let fh = fs.open(&node).expect("open a real file");
    assert_eq!(fh, 0, "case 1: a real file carries no handle state");

    let whole = fs
        .read(&node, 0, 0, fx.file_bytes.len())
        .await
        .expect("whole read");
    assert_eq!(
        &whole[..],
        &fx.file_bytes[..],
        "read(0, len) equals the file"
    );

    let mid = fs.read(&node, 0, 1000, 64).await.expect("mid read");
    assert_eq!(&mid[..], &fx.file_bytes[1000..1064], "a ranged read slices");

    let tail = (fx.file_bytes.len() - 2) as u64;
    let short = fs.read(&node, 0, tail, 64).await.expect("short read");
    assert_eq!(
        &short[..],
        &fx.file_bytes[fx.file_bytes.len() - 2..],
        "a read shortens at EOF"
    );

    let past = fs
        .read(&node, 0, fx.file_bytes.len() as u64, 64)
        .await
        .expect("past-EOF read");
    assert!(past.is_empty(), "a read at EOF is empty");

    assert!(
        fs.release(0).is_ok(),
        "release(0) after a real file is a no-op"
    );
}

/// The memoising path and the exhausted-budget path (case 4) of a virtual
/// ortho open, told apart by handle identity, tile-generation counts and the
/// shared pin budget — never by core internals.
async fn check_virtual_handle_policy(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let memoises = !fx.budget.would_exceed(fx.dds_size as u64);
    let base = fx.client.requests();

    let fh = fs.open(&fx.dds_node()).expect("open a virtual DDS");
    if memoises {
        assert_ne!(fh, 0, "a memoising open allocates a handle");
    } else {
        assert_eq!(fh, 0, "case 4: an exhausted budget opens with no handle");
    }

    let payload = fx.client.payload();
    let first = fs
        .read(&fx.dds_node(), fh, 0, 64)
        .await
        .expect("first read");
    assert_eq!(
        &first[..],
        &payload[..64],
        "the read carries the tile bytes, sliced"
    );
    let second = fs
        .read(&fx.dds_node(), fh, 64, 64)
        .await
        .expect("second read");
    assert_eq!(
        &second[..],
        &payload[64..128],
        "a second range of the same tile"
    );

    let generations = fx.client.requests() - base;
    if memoises {
        assert_eq!(
            generations, 1,
            "two reads on one handle are one generation (memoisation)"
        );
    } else {
        assert_eq!(generations, 2, "fh 0 resolves the tile per call (case 4)");
    }

    // A tile reads like a file to its end: a short final window, and
    // nothing past the end (contract §read).
    let len = payload.len() as u64;
    let tail = fs
        .read(&fx.dds_node(), fh, len - 100, 4096)
        .await
        .expect("tail window");
    assert_eq!(tail.len(), 100, "the last window is short, not an error");
    let past = fs
        .read(&fx.dds_node(), fh, len, 4096)
        .await
        .expect("past-the-end window");
    assert!(past.is_empty(), "a read at the tile end is empty");

    if memoises {
        assert_eq!(
            fx.budget.pinned_bytes(),
            payload.len() as u64,
            "the tile is pinned once"
        );
        assert!(fs.release(fh).is_ok(), "release the memoising handle");
        assert_eq!(fx.budget.pinned_bytes(), 0, "release unpins the tile");
        assert!(
            fs.release(fh).is_ok(),
            "releasing an already-released handle is still Ok"
        );
    } else {
        assert_eq!(
            fx.budget.pinned_bytes(),
            0,
            "the per-call path pins nothing"
        );
    }
}

/// Case 3 in its neutral shape (see the module docs): a handle the
/// filesystem never issued must not fail the read — it serves the tile by
/// resolving per call.
async fn check_unknown_handle_serves(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let base = fx.client.requests();
    let data = fs
        .read(&fx.dds_node(), u64::MAX, 32, 32)
        .await
        .expect("an unknown handle still serves");
    assert_eq!(&data[..], &fx.client.payload()[32..64]);
    assert_eq!(
        fx.client.requests() - base,
        1,
        "no handle means a per-call resolve"
    );
}

/// Case 2: a lit texture opens with no handle, pins nothing, and reads
/// through the lit source — never the DDS executor (contract §open case 2,
/// §read).
async fn check_lit_texture(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let black = minimal_black_dds();
    let entry = fs
        .lookup(&FsNode::root(), fx.lit_name)
        .await
        .expect("lit name resolves");
    assert_eq!(entry.node, fx.lit_node());
    assert_eq!(
        entry.attrs.size,
        black.len() as u64,
        "lit attrs carry the lit source's size"
    );

    let base = fx.client.requests();
    let fh = fs.open(&entry.node).expect("open a lit texture");
    assert_eq!(fh, 0, "case 2: a lit texture carries no handle state");

    let data = fs
        .read(&entry.node, 0, 0, black.len() + 16)
        .await
        .expect("lit read");
    assert_eq!(&data[..], &black[..], "the lit source is served by range");
    assert_eq!(
        fx.client.requests(),
        base,
        "lit reads never reach the DDS client"
    );
    assert_eq!(fx.budget.pinned_bytes(), 0, "lit textures pin nothing");
    assert!(fs.release(0).is_ok());
}

/// `release(0)` is a no-op a backend may call blindly (contract §release).
async fn check_release_zero(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let pinned = fx.budget.pinned_bytes();
    assert!(
        fs.release(0).is_ok() && fs.release(0).is_ok(),
        "release(0) is a no-op, repeatedly"
    );
    assert_eq!(
        fx.budget.pinned_bytes(),
        pinned,
        "release(0) releases nothing"
    );
}

/// A real-file read of a path the union does not have is `NotFound` — the
/// answer a backend must translate to its kernel's "no such file".
async fn check_read_not_found(fs: &dyn UnionFilesystem, _fx: &UnionFixture) {
    let nowhere = FsNode::RealFile(PathBuf::from("Earth nav data/+01-001/+01-000.dsf"));
    assert!(
        matches!(fs.read(&nowhere, 0, 0, 8).await, Err(FsError::NotFound)),
        "an unknown real path reads NotFound"
    );
}

/// `statfs` reports the union's entry count with sane volume figures.
fn check_statfs(fs: &dyn UnionFilesystem, fx: &UnionFixture) {
    let info = fs.statfs();
    assert_eq!(
        info.files, fx.file_count as u64,
        "statfs reports the union's entry count"
    );
    assert!(
        info.blocks > 0 && info.bsize > 0 && info.namelen > 0,
        "sane volume figures"
    );
}

// =============================================================================
// The one invocation: the core through the seam's own shape
// =============================================================================

#[cfg(test)]
mod tests {
    use super::super::OrthoUnionCore;
    use super::*;

    fn build_core(fx: &UnionFixture) -> Arc<dyn UnionFilesystem> {
        Arc::new(
            OrthoUnionCore::new(fx.build_index(), fx.client(), fx.dds_size)
                .with_pinned_budget(Arc::clone(&fx.budget)),
        )
    }

    #[tokio::test]
    async fn ortho_union_core_satisfies_the_union_contract() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut fx = UnionFixture::new(temp.path());

        // Run 1: the default (generous) budget — ortho opens memoise.
        let fs = build_core(&fx);
        check_union_filesystem(fs.as_ref(), &fx).await;

        // Run 2: the same battery against the same core code with a pinned
        // budget that cannot admit a single tile — contract case 4 replaces
        // memoisation everywhere the policy shows.
        fx.budget = Arc::new(PinnedTileBudget::with_ceiling(1));
        let fs = build_core(&fx);
        check_union_filesystem(fs.as_ref(), &fx).await;
    }
}
