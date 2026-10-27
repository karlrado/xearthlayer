# Windows Backend Split — Phase 1 Design

Date: 2026-10-27
Status: design, ready for implementation (Karl to confirm §10 before coding starts)
Base commit: `13f27b0` (develop/0.5.0 line; branch `bionic/windows-backend-split`)
Parent doc: `docs/dev/windows-support.md` (§4.9 phase structure, §4.11 gate list)

---

## 1. Purpose and scope

This is the **Step 2 deliverable** of the Windows port: a method-level split
analysis of the FUSE layer, tagged *portable core* / *fuse3-bound* / *needs
WinFSP re-expression*, plus the concrete Phase 1 design that implements the
split.

**In scope (Phase 1):** extract the backend-neutral core of the ortho union
filesystem out of `fuse/fuse3/` so that a `winfsp` backend (Phase 2) can call
the same core. Linux-only diff, no behaviour change, no new dependencies,
`Fuse3OrthoUnionFS`'s public API unchanged, `manager/mounts.rs` untouched.

**Out of scope (later phases):** the WinFSP backend itself (Phase 2), the
gate-list plumbing — `flock` (`manager/queue/lock.rs`), `statvfs`
(`system/filesystem.rs`), symlinks (`manager/symlinks.rs`), `/proc/*`
(`system/hardware.rs`, `diagnostics/report.rs`), `rlimit`
(`preflight/system.rs`) — and packaging (Phase 4). See
`docs/dev/windows-support.md` §4.11 for the full gate list.

Governing constraints (from the parent doc §4.9, Karl-approved):
- **Linux stays green** at every commit (fork CI `verify` job, `make pre-commit`
  equivalent: fmt + clippy + tests, ≥80% coverage).
- **Additive refactor** so upstream changes keep merging into the fork:
  extract a neutral core, keep `fuse3` a thin adapter, don't rewrite shared
  code, don't add new dependencies.
- **On-demand ortho end-to-end first**, exe before installer.

---

## 2. Baseline: what already exists

Key finding from the code read: **upstream already did the first cut of this
split.** Commit `8338c92` ("refactor(fuse): rename async_passthrough module to
support") created the `fuse/support/` neutral module, and `fuse3/inode.rs` is
now a 5-line re-export shim. Current layout at base commit:

```text
xearthlayer/src/fuse/
  mod.rs                27   re-exports (shared)
  coalesce.rs          540   RequestCoalescer            ✅ neutral
  filename.rs          611   DdsFilename, parse_*        ✅ neutral
  placeholder.rs       474   magenta placeholder         ✅ neutral
  support/
    mod.rs              12   re-exports
    inode.rs           426   InodeManager (DashMap)      ✅ neutral (already extracted)
    types.rs           125   DdsRequest/DdsHandler       ✅ neutral (legacy; untouched)
  fuse3/
    mod.rs              34   re-exports
    inode.rs             5   re-export shim → support/inode.rs
    ortho_union_fs.rs  3221  Fuse3OrthoUnionFS + impl Filesystem + tests
    shared.rs          601   VirtualDdsConfig, FileAttrBuilder, DdsRequestor
    types.rs           436   Fuse3Error, MountHandle, SpawnedMountHandle
```

The public API surface the rest of the codebase actually uses (verified by
grep — production caller is `manager/mounts.rs:573-597`, plus the
`macfuse_smoke` test):

```rust
Fuse3OrthoUnionFS::new(index, dds_client, expected_dds_size)
    .with_geo_index(...).with_state_observer(...)
    .with_fuse_limits(mb, ct)                 // FUSE-only
    .with_timeout(d).with_pinned_budget(b)
    .with_lit_source(s).with_metrics(m)
    .mount_spawned(path) -> SpawnedMountHandle
// accessors: index(), disk_io_limiter(), peak_dds_handles(),
//            peak_pinned_tile_bytes(), open_dds_handles(), pinned_tile_bytes()
```

**Phase 1 must preserve this surface exactly** — that is what keeps
`manager/mounts.rs` untouched and the diff mergeable.

Dependencies confirmed neutral (the core may hold them): `DdsClient`
(executor), `LitTextureSource` (night/service.rs:37 — `BoxFuture`, `Bytes`),
`GeoIndex`, `PrefetchStateObserver`, `StorageConcurrencyLimiter`,
`PinnedTileBudget`, `RequestCoalescer`, `OrthoUnionIndex`, `TileRequestCallback`,
`MetricsClient`. None reference fuse3/nix/libc.

---

## 3. Method-level split analysis

Tag legend:

- ✅ **Portable core** — backend-neutral logic; moves to the core in Phase 1.
- 🔒 **fuse3-bound** — FUSE/fuse3-specific; stays in the `fuse3/` adapter.
- 🔁 **Re-expressed per backend** — the *logic* is portable (goes to the core);
  the surrounding kernel op is a thin per-backend shell (FUSE now, WinFSP in
  Phase 2).

### 3.1 `ortho_union_fs.rs` — `Fuse3OrthoUnionFS` struct fields

| Field | Tag | Note |
|---|---|---|
| `index: Arc<OrthoUnionIndex>` | ✅ | core |
| `geo_index: Option<Arc<GeoIndex>>` | ✅ | core |
| `state_observer: Option<Arc<PrefetchStateObserver>>` | ✅ | core |
| `dds_client: Arc<dyn DdsClient>` | ✅ | core |
| `virtual_dds_config: VirtualDdsConfig` | ✅ | core (config is neutral) |
| `generation_timeout: Duration` | ✅ | core |
| `disk_io_limiter: Arc<StorageConcurrencyLimiter>` | ✅ | core |
| `tile_request_callback: Option<TileRequestCallback>` | ✅ | core |
| `request_coalescer: Arc<RequestCoalescer>` | ✅ | core |
| `lit_source: Arc<dyn LitTextureSource>` | ✅ | core (ortho **and** night-lit tree) |
| `dds_handles: DashMap<u64, Arc<DdsHandle>>` | ✅ | core (per-open memoisation) |
| `next_fh: AtomicU64` | ✅ | core (handle numbering) |
| `pinned: Arc<PinnedTileBudget>` | ✅ | core |
| `peak_handles_open: AtomicU64` | ✅ | core |
| `budget_warning_logged: AtomicBool` | ✅ | core |
| `metrics_client: Option<MetricsClient>` | ✅ | core |
| `fuse_max_background: Option<u16>` | 🔒 | FUSE init handshake only; WinFSP has its own queue |
| `fuse_congestion_threshold: Option<u16>` | 🔒 | FUSE init handshake only |
| `inode_manager: InodeManager` | 🔒 | FUSE *identity* (kernel passes ino numbers); adapter-owned. The type itself lives in neutral `support/inode.rs` and is **unchanged** |

**17 of 20 fields are already neutral** — they move; 2 stay (FUSE kernel
limits); 1 stays as an adapter-owned *instance* of a neutral type.

### 3.2 `ortho_union_fs.rs` — `impl Fuse3OrthoUnionFS` (lines 203–585)

| # | Method | Lines | Tag | Disposition |
|---|---|---|---|---|
| 1 | `new()` | 215–252 | ✅ | becomes `OrthoUnionCore::new`; adapter wraps |
| 2 | `with_disk_io_limiter()` | 259–289 | ✅ | core builder |
| 3 | `with_pinned_budget()` | 297 | ✅ | core builder |
| 4 | `with_lit_source()` | 301 | ✅ | core builder |
| 5 | `lit_dds_attr(ino, size)` | 305–311 | 🔁 | core computes lit size + neutral attrs; adapter maps to `FileAttr` |
| 6 | `virtual_attr_for(ino, coords)` | 314–321 | 🔁 | core picks ortho/lit attrs; adapter maps |
| 7 | `with_timeout()` | 328 | ✅ | core builder |
| 8 | `with_tile_request_callback()` | 336 | ✅ | core builder |
| 9 | `with_metrics()` | 345 | ✅ | core builder |
| 10 | `with_geo_index()` | 355 | ✅ | core builder |
| 11 | `with_state_observer()` | 364 | ✅ | core builder |
| 12 | `with_fuse_limits()` | 374 | 🔒 | stays adapter-only (FUSE kernel params) |
| 13 | `disk_io_limiter()` | 380 | ✅ | core accessor |
| 14 | `is_geo_filtered()` | 390–406 | ✅ | core (GeoIndex + filename math) |
| 15 | `resolve_tile()` | 418–423 | ✅ | core (placeholder fallback) |
| 16 | `claim_budget_warning()` | 428 | ✅ | core |
| 17 | `report_handle_gauge()` | 430–444 | ✅ | core |
| 18 | `peak_dds_handles()` | 448 | ✅ | core accessor |
| 19 | `peak_pinned_tile_bytes()` | 452 | ✅ | core accessor |
| 20 | `open_dds_handles()` | 456 | ✅ | core accessor |
| 21 | `pinned_tile_bytes()` | 460 | ✅ | core accessor |
| 22 | `record_read()` | 469–474 | ✅ | core |
| 23 | `resolve_lazy_geo()` | 476–484 | ✅ | core (GeoIndex + index composition) |
| 24 | `index()` | 488 | ✅ | core accessor |
| 25 | `mount()` | 514–543 | 🔒 | `fuse3::Session`, `MountOptions`, unprivileged mount; WinFSP gets its own mount |
| 26 | `mount_spawned()` | 554–575 | 🔒 | same; `SpawnedMountHandle` is fuse3-bound |
| 27 | `request_dds()` (wrapper) | 580–584 | ✅ | core (parse + `request_dds_impl`) |

### 3.3 `ortho_union_fs.rs` — module-level items

| Item | Lines | Tag | Note |
|---|---|---|---|
| `DdsHandle { coords, tile: OnceCell<Bytes> }` | 80–87 | ✅ | moves to core verbatim (per-open memoisation state) |
| `FOPEN_DIRECT_IO` | 96 | 🔒 | FUSE open flag |
| `VIRTUAL_DDS_OPEN_FLAGS` | 112–115 | 🔒 | incl. the macOS page-cache carve-out |
| `mount_options()` | 587–597 | 🔒 | `MountOptions` (fuse3) |
| `volume_name_option()` | 605–611 | 🔒 | macFUSE `volname` only; WinFSP: volume name is a mount property (Phase 2) |
| `impl FileAttrBuilder` | 621–625 | 🔒 | one-line delegation to core config; trait stays in `fuse3/` |
| `impl DdsRequestor` | 627–657 | 🔁 | trait moves to `support/` (supertrait dropped); impl moves to core |
| 44 test functions | 1372–3221 | — | stay at adapter level; a subset is additionally ported to core level (§6 test strategy) |

### 3.4 `ortho_union_fs.rs` — `impl Filesystem` (14 ops, lines 659–1280)

Every op is 🔒 as a shell; the table shows what each reduces to in the core:

| Op | Lines | Tag | Core call (Phase 1) | WinFSP op (Phase 2) |
|---|---|---|---|---|
| `init` | 660–674 | 🔒 | — (logging; `max_background`/`congestion_threshold` negotiation is FUSE-only) | mount setup / `FilesystemOptions` |
| `destroy` | 676–678 | 🔒 | — | unmount |
| `lookup` | 680–772 | 🔁 | `core.lookup(parent, name) → (FsNode, FileAttributes)`; adapter allocates ino + `ReplyEntry`; ENOENT | `ParseName` → `FileContext` |
| `getattr` | 774–833 | 🔁 | `core.attrs(node) → FileAttributes` | `GetAttributes` |
| `read` | 835–952 | 🔁 | `core.read(node, fh, offset, size) → Bytes` — lit source, handle memoisation, `resolve_tile`, zero-copy slice; real file open/seek/take | `Read` (async supported by the crate) |
| `readdir` | 954–1040 | 🔁 | `core.list_directory(node) → Vec<DirEntry>` (incl. `.`/`..`); adapter applies offset, maps inos | `ReadDirectory` (batched/offset model — real rework, Phase 2) |
| `readdirplus` | 1042–1143 | 🔁 | same + per-entry `attrs` (dirs virtual, files real metadata, fallbacks preserved) | same `ReadDirectory` carries attrs |
| `open` | 1145–1213 | 🔁 | `core.open(node) → fh` — budget check, `DdsHandle` allocation, lit/real → fh 0 | `CreateFile` (file context) |
| `release` | 1215–1231 | 🔁 | `core.release(fh)` — unpin + gauge | `Cleanup` |
| `opendir` | 1233–1242 | 🔒 | — (stateless fh 0) | N/A (stateless dir I/O) |
| `access` | 1244–1246 | 🔒 | — (no-op allow) | `GetSecurityContext` (default allow) |
| `flush` | 1248–1256 | 🔒 | — (no-op) | N/A |
| `fsync` | 1258–1266 | 🔒 | — (no-op) | N/A |
| `statfs` | 1268–1280 | 🔁 | `core.statfs() → StatFsInfo` | `GetVolumeInfo` |

**The 14 ops contain no logic that is not already factored into the
✅/🔁 methods above** — `lookup`'s water-mask guard (#68), geo-filtered lazy
resolution, and DDS-name parsing are the only "sub-logic" pieces and all three
are ✅ (they call `resolve_lazy_geo` / `parse_dds_filename`).

### 3.5 `shared.rs` (lines 1–601)

| Item | Lines | Tag | Disposition |
|---|---|---|---|
| `TTL` (1 s attr cache) | 30 | 🔒 | FUSE attr-cache TTL; stays (WinFSP has its own validity model) |
| `VirtualDdsConfig` | 42–84 | ✅ | pure (size + blksize); **move to `support/`** |
| `FileAttrBuilder` trait | 109–252 | 🔒/🔁 | trait stays in `fuse3/shared.rs` (its surface is `fuse3::FileAttr`); its *default impls* route through the core's neutral `FileAttributes` mappers so there is one mapping path (§4.4) |
| `chunk_to_tile_coords()` | 271–281 | ✅ | pure (`DdsFilename` → `TileCoord`); **move to `support/`** |
| `DdsRequestor` trait | 295–352 | ✅ | **move to `support/`**; drop the `: FileAttrBuilder` supertrait (the pipeline never uses it) |
| `request_dds_impl()` (default) | 355–460 | ✅ | neutral pipeline: lit guard, coalesce, submit, timeout, validate — moves to core as the `DdsRequestor` impl |
| `do_request()` (default) | 472–508 | ✅ | neutral: `DdsClient` + `CancellationToken` + placeholder fallback |
| tests | 510–601 | ✅ | `VirtualDdsConfig` + `chunk_to_tile_coords` tests move with their types |

### 3.6 `types.rs` (lines 1–436)

| Item | Tag | Note |
|---|---|---|
| `Fuse3Error` / `Fuse3Result` | 🔒 | stays; WinFSP adapter gets its own error type (Phase 2). `FsError` (§4.2) covers the *core's* surface; `Fuse3Error::MountFailed` etc. stay adapter-local |
| `MountHandle` | 🔒 | wraps `fuse3::MountHandle` |
| `SpawnedMountHandle` | 🔒 | task + unmount channel + **fusermount** escalation — intrinsically FUSE. Phase 2: WinFSP adapter has its own handle; `manager/mounts.rs` then needs a neutral mount-handle seam (explicitly **not** Phase 1) |

### 3.7 `support/` (existing neutral)

| Item | Status |
|---|---|
| `inode.rs` — `InodeManager` | already neutral; **unchanged**. Becomes adapter-owned (FUSE identity only; WinFSP uses file contexts instead) |
| `types.rs` — `DdsRequest`/`DdsHandler` | legacy (referenced only by their own tests + re-exports); **unchanged** |

### 3.8 Tally

| Tag | Count |
|---|---|
| ✅ portable core (moves in Phase 1) | 17 fields, 20 methods, `DdsHandle`, `VirtualDdsConfig`, `chunk_to_tile_coords`, `DdsRequestor` pipeline, neutral attr mappers |
| 🔒 fuse3-bound (stays) | 14 op shells, `mount`/`mount_spawned`, `types.rs`, `FileAttrBuilder` surface, `with_fuse_limits`, `FOPEN_*` flags, `TTL`, `inode_manager` instance, `volume_name_option` |
| 🔁 re-expressed per backend (core logic + thin shells both sides) | `lookup`, `getattr`, `read`, `readdir`, `readdirplus`, `open`, `release`, `statfs`, attr building, `DdsRequestor` impl |

**The split is lopsided in the right direction:** essentially all *behaviour*
(17/20 fields, 20/27 methods) is portable; what stays fuse3-bound is kernel
plumbing (identity numbers, attr structs, mount lifecycle, two init
handshake parameters) — exactly the set that Phase 2 re-expresses against
WinFSP.

---

## 4. Target architecture (Phase 1)

### 4.1 Module layout after Phase 1

```text
xearthlayer/src/fuse/
  mod.rs                # re-exports — unchanged (paths stable)
  coalesce.rs           # unchanged
  filename.rs           # unchanged
  placeholder.rs        # unchanged
  support/              # backend-neutral core (extended)
    mod.rs              # re-exports — extended
    inode.rs            # unchanged (adapter-owned at runtime)
    types.rs            # unchanged (legacy)
    node.rs             # NEW — FsNode
    error.rs            # NEW — FsError
    attrs.rs            # NEW — FileAttributes, DirEntry, StatFsInfo, VirtualDdsConfig (moved)
    requestor.rs        # NEW — DdsRequestor (moved, supertrait dropped), chunk_to_tile_coords (moved)
    core.rs             # NEW — OrthoUnionCore (state + neutral ops + DdsHandle)
  fuse3/                # thin FUSE adapter
    mod.rs              # re-exports — unchanged (paths stable)
    inode.rs            # unchanged (shim → support/inode.rs)
    ortho_union_fs.rs   # shrinks: adapter struct + impl Filesystem + mount + flags + tests
    shared.rs           # shrinks: FileAttrBuilder (routes via neutral attrs), TTL, re-exports of moved items
    types.rs            # unchanged
```

Design decisions:

1. **The core extends `support/`** rather than creating a parallel
   `fuse/core/` directory — `support/` is the established neutral home
   (upstream created it; `fuse3/inode.rs` already re-exports from it), and
   extending it is the most mergeable shape.
2. **`Fuse3OrthoUnionFS` keeps its name and public API** — it becomes a thin
   adapter over `OrthoUnionCore`. `manager/mounts.rs`, `macfuse_smoke`, and
   all docs referencing the type stay valid.
3. **`InodeManager` moves out of the core** into the adapter. Inode numbers
   are a FUSE kernel concept; WinFSP identifies files by context objects.
   Making `InodeManager` adapter-owned (rather than core-owned) is what makes
   the core genuinely neutral — see §4.2's `FsNode`.
4. **No new dependencies**, no new traits beyond what exists. `DdsRequestor`
   loses its `FileAttrBuilder` supertrait (unused by the pipeline — verified:
   `request_dds_impl`/`do_request` call only their own accessors).

### 4.2 The neutral identity seam: `FsNode`

This is the core of the design and the answer to the inode→context problem
(parent doc §4.3A): instead of letting *identity* be a backend concept, the
core operates on a neutral node identity that **both** backends map their
kernel identity onto.

```rust
/// support/node.rs
pub enum FsNode {
    /// A directory in the union view. Root = empty path.
    Directory(PathBuf),
    /// A real file from a source, addressed by its *virtual* path.
    /// (Resolution to the source's real path happens inside core ops.)
    RealFile(PathBuf),
    /// A generated texture: ortho or lit (`DdsFilename::kind`).
    VirtualDds(DdsFilename),
}
```

| Backend | Kernel identity | Mapping |
|---|---|---|
| FUSE (today) | numeric `ino` (u64) | adapter: `ino → FsNode` via `InodeManager` (virtual inodes → `FsNode::VirtualDds`, path inodes → `Directory`/`RealFile`); core result → `ino` via `get_or_create_inode` / `create_virtual_inode` |
| WinFSP (Phase 2) | file-context object | adapter: `ParseName` → `core.lookup` → store `FsNode` **in the context**; `Read`/`GetAttributes`/`Cleanup` take the context → `FsNode` → core call. No inodes anywhere |

Because the core's ops all take/return `FsNode`, the "inode model" the parent
doc flagged as *the biggest conceptual lift* is reduced to **two thin
adapter-level translation tables**, and the core itself carries no FUSE
concept at all. (FUSE `lookup`'s ino allocation, `readdir`'s `.`/`..` ino
math, and `open`'s fh numbering: inos stay in the FUSE adapter; fh numbering
and `DdsHandle` memoisation move to the core, since both backends need
per-open state.)

### 4.3 Neutral seam types

```rust
/// support/error.rs — the core's whole error surface (3 variants)
pub enum FsError {
    NotFound,      // FUSE: ENOENT   WinFSP: ERROR_FILE_NOT_FOUND
    NotADirectory, // FUSE: ENOTDIR WinFSP: ERROR_NOT_DIR
    Io(#[from] io::Error), // FUSE: EIO  WinFSP: FSP_ERROR_IO
}

/// support/attrs.rs
pub struct FileAttributes {
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub kind: NodeKind, // Directory | RegularFile | Symlink
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
}
// neutral mappers in core:
//   attrs_from_metadata(&std::fs::Metadata) -> FileAttributes
//   virtual_dds_attrs(size) -> FileAttributes   (perm 0o444, nlink 1, …)
//   virtual_dir_attrs() / root_dir_attrs()      (perm 0o555, nlink 2, …)
// The FUSE adapter maps FileAttributes → fuse3::FileAttr (incl. the
// #[cfg(target_os = "macos")] crtime/fields); Phase 2 maps to WinFSP's types.

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub node: FsNode,          // carries the child's identity (for `.`/`..` mapping)
    pub attrs: FileAttributes,
}

pub struct StatFsInfo { /* blocks, bfree, bavail, files, ffree, bsize, namelen, frsize */ }
```

`uid`/`gid` stay in the neutral struct (harmless on Windows — the WinFSP
adapter can map to the launching user or drop them); `libc::getuid/getgid`
and `MetadataExt` calls stay in the **FUSE adapter's** mapper only, so they
never appear in core code.

### 4.4 `OrthoUnionCore` — the seam API

```rust
/// support/core.rs
pub struct OrthoUnionCore { /* the 17 neutral fields from §3.1 + DdsHandle map */ }

impl OrthoUnionCore {
    // construction / builders (all move verbatim from Fuse3OrthoUnionFS)
    pub fn new(index: OrthoUnionIndex, dds_client: Arc<dyn DdsClient>,
               expected_dds_size: usize) -> Self;
    pub fn with_disk_io_limiter(...) -> Self;
    pub fn with_pinned_budget(...) -> Self;
    pub fn with_lit_source(...) -> Self;
    pub fn with_timeout(...) -> Self;
    pub fn with_tile_request_callback(...) -> Self;
    pub fn with_metrics(...) -> Self;
    pub fn with_geo_index(...) -> Self;
    pub fn with_state_observer(...) -> Self;
    // with_fuse_limits() deliberately NOT here — FUSE adapter only

    // --- the neutral ops: every backend calls exactly these ---
    pub fn lookup(&self, parent: &FsNode, name: &str)
        -> Result<(FsNode, FileAttributes), FsError>;
    pub fn attrs(&self, node: &FsNode) -> Result<FileAttributes, FsError>;
    pub async fn read(&self, node: &FsNode, fh: u64, offset: u64, size: u32)
        -> Result<Bytes, FsError>;
    pub fn list_directory(&self, node: &FsNode) -> Result<Vec<DirEntry>, FsError>;
    pub fn open(&self, node: &FsNode) -> u64;   // fh; 0 = no handle state
    pub fn release(&self, fh: u64);
    pub fn statfs(&self) -> StatFsInfo;

    // accessors (adapter + manager code call these through the adapter)
    pub fn index(&self) -> &OrthoUnionIndex;
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter>;
    pub fn peak_dds_handles(&self) -> u64;
    pub fn peak_pinned_tile_bytes(&self) -> u64;
    pub fn open_dds_handles(&self) -> usize;
    pub fn pinned_tile_bytes(&self) -> u64;
    pub fn virtual_dds_config(&self) -> &VirtualDdsConfig;
}

impl DdsRequestor for OrthoUnionCore { /* moved verbatim from the trait default impls */ }
```

Behavioural equivalences that the core must preserve exactly (each has an
existing test to port):

- `lookup`: index-resolve → directory → geo-lazy → `.dds` (water-mask guard
  #68 first, then `parse_dds_filename`) → `NotFound`. Lit sizes come from
  `lit_source.size`.
- `read`: lit → `lit_source.read` (never executor/coalescer — R8); ortho →
  handle memoisation (`get_or_init` on `DdsHandle.tile`), no-handle fallback
  resolves per call, zero-copy `Bytes::slice`; real file → index/lazy resolve,
  disk-IO permit, open/seek/`take(size)` (short reads at EOF, EIO mapping).
- `open`: budget check (`pinned.would_exceed`), metrics on every refusal,
  latched warning; lit/real/unknown → fh 0.
- `release`: unpin + gauge.
- `list_directory`: `.`/`..` synthesis (root's parent is itself), then
  `index.list_directory`; per-entry attrs exactly as `readdirplus` does today
  (dirs virtual, files real-metadata with the `virtual_dds_attr` fallback).
- `statfs`: `files = index.file_count()`, current constants otherwise.
- `request_dds_impl`/`do_request`: lit guard, coalesce register/complete,
  timeout + cancel + placeholder, `validate_dds_or_placeholder`,
  `on_dds_response` (#176 hook).

### 4.5 The thin FUSE adapter after Phase 1

```rust
pub struct Fuse3OrthoUnionFS {
    core: OrthoUnionCore,
    inode_manager: InodeManager,          // was a core field; now adapter-owned
    fuse_max_background: Option<u16>,     // was a core field
    fuse_congestion_threshold: Option<u16>,
}
```

- Public API: **identical signatures** — `new`, all `with_*` (incl.
  `with_fuse_limits`, now adapter-local), `mount`, `mount_spawned`, all
  accessors — each a one-line delegation where the state moved.
- `impl Filesystem`: each op becomes `ino → FsNode` (via `InodeManager`) →
  core call → `FsError → Errno` / `FileAttributes → FileAttr` → `Reply*`.
  The op shells shrink from 140 lines (`lookup`) to ~30.
- `FileAttrBuilder` stays in `fuse3/shared.rs`; its default impls
  (`metadata_to_attr`, `virtual_dds_attr`, `root_dir_attr`,
  `virtual_dir_attr`) re-route through the core's neutral `FileAttributes`
  mappers so there is exactly one place that knows how `std::fs::Metadata`
  and virtual configs become attributes — the macOS `crtime`/`flags` fields
  live in the adapter's `FileAttributes → FileAttr` mapper.
- `shared.rs` keeps `TTL` and re-exports `VirtualDdsConfig`,
  `chunk_to_tile_coords`, `DdsRequestor` from `support/` so every existing
  import path (`fuse::fuse3::shared::*`, tests' `super::super::shared::*`)
  keeps compiling unchanged.

### 4.6 Phase 2 preview — how WinFSP uses the same core

(Recorded here so the Phase 1 seams are judged against this use; not
implemented in Phase 1.)

| WinFSP op (idiomatic `winfsp` crate) | Shell over |
|---|---|
| `ParseName(parent_ctx, name)` | `core.lookup` → store `FsNode` in the new `FileContext` |
| `GetAttributes(ctx)` | `core.attrs(node)` |
| `Read(ctx, offset, size)` (async supported) | `core.read(node, fh, offset, size)` — the block-and-generate `await` ports as-is (parent doc §4.4 #1: X-Plane does plain `ReadFile`, no mmap) |
| `ReadDirectory(ctx, offset)` | `core.list_directory(node)`; batched/offset semantics + `.`/`..` contexts are the one genuine rework (parent doc §4.4 #3) |
| `CreateFile(ctx)` | `core.open(node)` (budget + handle state) |
| `Cleanup(ctx)` | `core.release(fh)` |
| `GetVolumeInfo` | `core.statfs()` |
| errors | `FsError → FspError` mapping (3 variants) |
| mount lifecycle | `winfsp` session + its own `SpawnedMountHandle`-equivalent; `manager/mounts.rs` gets a neutral mount-handle seam then |
| `with_fuse_limits` | dropped — WinFSP manages its own request queue |

The two tree types (ortho + night-lit) are already handled by the core
(`FsNode::VirtualDds` carries `TextureKind`); WinFSP sees one file system
with two kinds of virtual files, exactly like FUSE does.

---

## 5. Explicitly out of scope for Phase 1

- Any `winfsp`/`winfsp-sys` dependency or `fuse/winfsp/` module (Phase 2).
- Target-gating `fuse3`/`nix`/`rlimit` in `Cargo.toml` (Phase 2 — that is the
  step that "adds Windows"; Phase 1 must not change the dependency graph).
- The gate-list plumbing (§4.11 of the parent doc: `flock`, `statvfs`,
  `MetadataExt` in `manager/installer.rs`, symlinks, `/proc/*`, `rlimit`).
- `manager/mounts.rs` neutral mount-handle seam (Phase 2, with the WinFSP
  handle).
- The two "verify" items from Phase 0 (`memory-stats`, `crossterm
  use-dev-tty`) — they surface in the first real Windows build (Phase 2),
  not in a pure refactor.
- The optional WinFSP blocked-read spike (parent doc §4.7 step 1) — Phase 2.

---

## 6. Implementation plan (TDD; every commit green on Linux)

Work happens on `bionic/windows-backend-split`. "Green" = fork CI `verify`
(ubuntu-latest: fmt + clippy + tests + coverage) passes, since the Windows
box cannot build the crate at all until Phase 2's target-gating (fuse3
fails to compile for `x86_64-pc-windows-msvc` by design).

| # | Commit | Content | Why this order |
|---|---|---|---|
| 1 | `refactor(fuse): add neutral FsError + FsNode to fuse/support` | `support/error.rs`, `support/node.rs` + unit tests (node path math: parent/`..` incl. root) | pure addition; zero risk |
| 2 | `refactor(fuse): move VirtualDdsConfig, chunk_to_tile_coords, DdsRequestor to support` | `support/attrs.rs` (types + neutral attr mappers + `StatFsInfo`/`DirEntry`), `support/requestor.rs`; `fuse3/shared.rs` shrinks to re-exports + `FileAttrBuilder` (supertrait dropped); tests move with their types | moves the neutral pieces before the core exists, so commit 3 is smaller |
| 3 | `refactor(fuse): extract OrthoUnionCore state + builders + accessors` | `support/core.rs` with the 17 fields, all `with_*` builders, all accessors, `DdsHandle`, gauge/budget/`record_read`/`resolve_tile`/`is_geo_filtered`/`resolve_lazy_geo`/`request_dds`; `Fuse3OrthoUnionFS` wraps the core and delegates; **public API unchanged**; `DdsRequestor` impl lands on core | state first: the adapter already delegates, ops untouched |
| 4 | `refactor(fuse): route the 14 FUSE ops through core neutral ops` | `core.lookup/attrs/read/list_directory/open/release/statfs` implemented (bodies moved from the ops); ops become thin shells; `FileAttrBuilder` default impls route via neutral mappers; `FsError → Errno` mapping | the big move; op-level tests (which call `impl Filesystem` directly) are the regression net |
| 5 | `test(fuse): port behavioural tests to the core level` | water-mask guard, geo-filtered lookup (patched region), lit read/size, once-per-open memoisation, budget-exhaustion trio, range reads incl. EOF clamp, readdir `.`/`..` math — as core-level tests (no `fuse3::Request` fixtures needed) | TDD close: core covered directly, not only through the FUSE shell |
| 6 | `docs(fuse): update architecture notes for the backend split` | `CLAUDE.md` (module map, trait notes), `docs/dev/fuse-filesystem.md` (module table), `docs/dev/windows-support.md` (Phase 1 status + pointer to this doc) | keeps docs truthful (project rule) |

Commits 3 and 4 may each split into two if a commit grows beyond reviewable
size (3a state/builders, 3b pipeline; 4a lookup/attrs/list, 4b read/open/
release). The split analysis in §3 is the checklist that each commit is
verified against.

**Test strategy (TDD + coverage):**
- Existing op-level tests (44 in `ortho_union_fs.rs`, plus `shared.rs`'s 7)
  **stay and keep passing unchanged** — they exercise the full
  `Filesystem → core` path and are the behavioural regression net.
- Commit 5 ports the highest-value behavioural assertions to core level so
  the core is directly covered (the adapter shells then stay covered by the
  op-level tests). ≥80% coverage is CI-enforced; the moved code is
  line-for-line the same statements, so coverage cannot regress if the test
  split is done.
- New neutral types (`FsNode`, `FsError`, `FileAttributes`, mappers) get
  their own unit tests in commits 1–2.

**Windows-box note:** `cargo fmt` + `cargo clippy` for the *neutral* modules
can be sanity-checked locally with `cargo check` on the Linux target only if
a Linux toolchain/WSL is available; otherwise CI is the gate. The Windows
build state does not change in Phase 1 (still fails at `fuse3`, by design).

---

## 7. Verification and exit criteria (Phase 1)

1. Fork CI `verify` green on every commit (fmt, clippy, tests, ≥80% coverage).
2. `manager/mounts.rs`, `xearthlayer-cli`, and all non-fuse modules: **zero
   diff**.
3. `Fuse3OrthoUnionFS` public API: byte-identical signatures (verified by
   compiling `manager/` + `macfuse_smoke` unmodified).
4. No new dependencies (`Cargo.lock` diff: none).
5. Linux behaviour unchanged: the 44 op-level tests + 7 shared tests +
   ported core tests all pass; the magenta-pathway invariants (R4/R8, §8.1 of
   the parent doc) covered.
6. Windows build state: unchanged (still fails at `fuse3` — expected until
   Phase 2's target-gating).
7. Docs updated (commit 6).

**Checkpoint question (pending Karl's OK, per the 2026-10-27 handoff):**
"clean Windows build after Phase 1" is recommended **as a checkpoint, not a
hard gate** — a clean Windows build is structurally impossible in Phase 1
(fuse3 is not target-gated yet; that is Phase 2's job), so the meaningful
Phase 1 check is *no new Windows build failures and no dependency-graph
change*, which is item 6 above. The real clean-build gate is Phase 2's
exit.

---

## 8. Risks and mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Behavioural regression in the read path (the heart of the app) during the move | Low | bodies move verbatim (not rewrites); op-level tests stay as the net; commit 4 is the only commit touching behaviour-adjacent code and lands after the state move |
| Upstream merge friction | Low | additive files in the established `support/` home; `Fuse3OrthoUnionFS` name/API stable; `fuse3/shared.rs` keeps its import path via re-exports; no Cargo changes |
| `FileAttrBuilder` re-routing changes macOS attr fields (`crtime`) | Low | the macOS fields move *with the mapper* into the adapter's `FileAttributes → FileAttr` function; `macfuse_smoke` (macOS CI) + field-by-field diff in review |
| Core grows a hidden FUSE dependency (e.g. `TTL`, ino math leaking in) | Medium | §4.2's rule: core code may not name `fuse3::`, `libc::` Unix symbols, or `InodeManager`; reviewed per commit against the §3 tables |
| `readdirplus` fallback subtleties (files not in index get `virtual_dds_attr` fallback) lost in the move | Low | preserved in `core.list_directory` spec (§4.4); covered by `test_readdirplus_returns_correct_file_sizes_for_passthrough_files` (stays) + a core-level port |
| fh/inode conflation when both adapters coexist | Low | core's `fh` numbering is backend-agnostic (just a u64 key into `DdsHandle`); WinFSP reuses the same `open`/`release` pair, so no second handle model is invented |

---

## 9. Open questions for Karl

1. **Clean-build checkpoint** (§7, last paragraph): OK with "checkpoint, not a
   hard gate" for Phase 1? (Pending from the handoff — the one decision
   still not explicitly approved.)
2. **Core home:** `fuse/support/` (this doc's proposal) vs. a new
   `fuse/core/` directory. Cosmetic; `support/` is chosen for mergeability.
3. **CI as the green gate:** commits are pushed to `bionic/windows-backend-split`
   on `origin` (your fork) so the ubuntu `verify` job runs. Confirm pushing
   each commit is fine (vs. batching a few).

---

## 10. References

- `docs/dev/windows-support.md` — parent scoping doc: §1 goal, §4.3A the split,
  §4.4 unknowns, §4.8 OS rules, §4.9 phases, §4.11 gate list.
- `docs/dev/fuse-filesystem.md` — current FUSE architecture (pre-split).
- `docs/dev/night-lighting-design.md` — R4/R7/R8 lit-texture invariants
  that `core.read`/`core.lookup` must preserve.
- `docs/dev/job-executor-design.md` — `DdsClient` contract the core pipeline
  calls.
- Upstream commits: `8338c92` (support module), `b944e49` (Bytes end-to-end),
  `0df8b8c` (lit inodes) — the prior extraction this design builds on.
