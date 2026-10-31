# Windows Support (WinFSP backend) — Design

**Status:** Specification for a phased, behavior-preserving addition of Windows
support. Not yet implemented on this branch; the phases below are the build
plan.
**Applies to:** the active upstream development line (0.5.0 / 0.6.0). The design
is written **base-agnostic** — it names the modules and seams it relies on but
avoids line numbers, which drift. Re-verify every anchor on the base you land
on before regenerating a phase.
**Companion:** the feature-request issue *"Add Windows support (WinFSP
backend)"*.

---

## 1. Purpose

Add a **Windows** target so XEarthLayer serves on-demand ortho and night-lit
scenery the same way it already does on Linux and macOS: a virtual filesystem
that intercepts X-Plane's texture reads and synthesizes DDS in response, with
**no user-facing pre-warm step**. The scenery package installer is unblocked on
Windows as part of the same effort.

This is an **additive backend, not a rewrite**. The union-filesystem logic is
extracted into a backend-neutral core; the existing FUSE backend becomes a thin
adapter over that core (serving **Linux and macOS**); a new `winfsp` adapter
(Windows only) implements the same core. Existing Linux and macOS behavior is
unchanged.

---

## 2. Governing requirement

**Behavioral parity with the Linux build.** The defining feature is that *a file
that does not exist on disk can still be served, because something intercepts
the read and synthesizes it.* That interception is the product.

Consequences that fix the architecture:

- X-Plane issues **plain synchronous reads** (`ReadFile` on Windows; no `mmap`).
  There is no user-mode hook to pause a missing-file open on Windows — the OS
  returns `ERROR_FILE_NOT_FOUND` synchronously. So the synthesis **must** happen
  at the filesystem layer.
- Pre-generation of real files ("Path B") is therefore out: it removes the
  interception and forces a pre-warm. The only path that meets the requirement
  is a user-mode filesystem provider. On Windows that is **WinFSP** (or Dokan);
  WinFSP is chosen as the established, user-installed framework.

---

## 3. One core, two adapters — the shape

```text
                        ┌─────────────────────────────────────────────┐
   X-Plane (any OS) ───►│  neutral core (fuse/support)                │
   reads virtual DDS ──►│   · FsNode / FsError (neutral identity & err)│
                        │   · OrthoUnionCore: state + the ops          │
                        │   · DdsRequestor / attrs / request coalescing│
                        └───────────────┬─────────────────────────────┘
                                        │  (backend-neutral op surface)
                     ┌──────────────────┴──────────────────┐
                     ▼                                     ▼
          ┌────────────────────┐                 ┌────────────────────┐
          │  fuse3 adapter     │                 │  winfsp adapter    │
          │  (Linux, macOS)    │                 │  (Windows)         │
          │  ino ⇄ FsNode      │                 │  ctx ⇄ FsNode      │
          │  FUSE Filesystem   │                 │  WinFSP service    │
          └────────────────────┘                 └────────────────────┘
```

- **Neutral core** (`fuse/support/`): owns the union-filesystem state and
  expresses the operations against a neutral identity (`FsNode`) and a neutral
  error (`FsError`). Carries **no** FUSE, WinFSP, or Unix symbol.
- **Adapters**: each maps its kernel identity onto `FsNode`, implements the
  op surface for its provider, and maps `FsError` onto its provider's error
  type. The FUSE adapter keeps its public type name and API so nothing that
  references it changes.

---

## 4. Guiding constraints

### 4.1 Additive, not a rewrite
The core is extracted by **moving existing code**, not re-implementing it.
Operation bodies move verbatim (or as close to verbatim as the type
change allows); the op-level tests stay as the safety net. The FUSE backend
keeps its name and public API.

### 4.2 One core, two adapters
The union-FS logic lives exactly once (the core). `fuse3` and `winfsp` are
sibling adapters over it. No union-FS behavior is duplicated per backend; the
only per-backend code is identity translation, provider-specific op plumbing,
and error mapping.

### 4.3 OS dependency rule (compile/link boundary)
The codebase compiles to two distinct binaries from one source tree:

1. **OS-specific symbols → conditional compilation.** Any code naming an
   OS-only symbol must be `#[cfg(target_os = "...")]`-gated **or** in a
   separate compilation unit. The compiler cannot link a symbol that does not
   exist on the target; this is not optional.
   - Foot-gun: `cfg!(windows)` inside a runtime `if` still *compiles both*
     branches (it folds the condition, not the code). An OS-only symbol must
     never appear in a `cfg!`/runtime branch expecting it to be stripped.
2. **Pure-Rust behavior → runtime branching is allowed and preferred.** Where
   neither branch names an OS-only symbol, a runtime check is fine — e.g.
   picking a default path, a config key, or a log message from
   `std::env::consts::OS`.
3. **Target-specific dependencies.** Crates that pull in OS-only symbols
   (`fuse3`, `winfsp`, `nix`, `rlimit`) live under
   `[target.'cfg(...)'.dependencies]` so the other OS never compiles or links
   them. Pure-Rust crates stay in `[dependencies]`.
4. **One source tree, no forks.** Both platforms build from the same
   repository; the difference is the target triple plus the cfg-gated backend.

**Dependency matrix** (move to target-gated where "absent"):

| Crate | Linux / macOS | Windows | Note |
|---|---|---|---|
| `fuse3` | required | **absent** | FUSE kernel module; the shared Linux + macOS backend |
| `winfsp` / `winfsp-sys` | absent | **required** | WinFSP user-mode API + bindings |
| `nix`, `rlimit` | required | **absent** | Unix primitives (`Flock`, `statvfs`, `rlimit`) |

### 4.4 WinFSP is a user-installed prerequisite

WinFSP is a third-party, user-installed prerequisite — the
kind of dependency the Windows ortho/sim audience already lives with (e.g.
users of AutoOrtho). XEarthLayer treats it as a **runtime dependency the user
installs**, and does **not** bundle, sign, or re-sign the driver.

**Desired experience:** when WinFSP is absent, the app **starts, detects the
absence, and points the user at the install instructions** — the same
preflight-prerequisite pattern already used for macFUSE.

**What is and isn't free.** The bindings crate ships its own import
libraries, so **building** needs no WinFSP install (a CI job can build). Two
runtime concerns sit behind "detect absence":

- **Start even when the DLL is absent.** A normal (immediate) PE import is
  resolved by the Windows loader **at process start**, and if the DLL is
  missing the loader refuses to start *before any of our code runs*. The
  `winfsp` crate's `delayload` feature emits `/DELAYLOAD:winfsp-x64.dll` at the
  final link (see §8.5), so the process starts regardless; a failed import then
  surfaces as a typed `MountError::Winfsp` at mount time rather than a raw
  loader failure.
- **Find the DLL when it is present.** A standard WinFSP install puts
  `winfsp-x64.dll` in `C:\Program Files (x86)\WinFsp\bin`, which is neither on
  `PATH` nor a default DLL search path. The crate's `system` feature handles
  this: `winfsp_init()` resolves the DLL from the standard search path first,
  then from `HKLM\SOFTWARE\WinFsp\InstallDir` — no hardcoded location, no
  manual path plumbing.

The genuinely remaining work item (Phase 4, item 1; §8.5) is the **early,
friendly "install WinFSP" message**: a Windows-gated preflight check mirroring
macFUSE so the user gets a clean prompt *before* a mount is attempted, instead
of the `MountError::Winfsp` backstop at mount time. The two DLL-resolution
sub-goals above are already solved by the crate; this is UX polish, not a
loader problem.

### 4.5 Clean, focused, independently-green commits
Each phase lands as a focused, well-described commit (or small set) that is
**independently green** (build + test on every target that phase is expected to
build). The history reads as a sequence of small, reviewable steps; no phase
depends on an unmerged later phase.

---

## 5. Current seam (base-agnostic)

The platform-specific surface is bounded. This is why the work is a
re-extraction rather than a rewrite.

### 5.1 Where the platform coupling lives

- **The FUSE backend** — `fuse/fuse3/`:
  - `ortho_union_fs.rs`: `Fuse3OrthoUnionFS` — the union-FS logic and the
    `impl Filesystem` (the ~14 operations, plus `mount`/`mount_spawned`).
      This is the code the split extracts from.
  - `shared.rs`: `DdsRequestor`, `VirtualDdsConfig`, `FileAttrBuilder`,
    `chunk_to_tile_coords`, `TTL` — shared helpers currently nested in the
    backend.
  - `inode.rs`: `InodeManager` (FUSE inode allocation).
  - `types.rs`: `Fuse3Error`, `Fuse3Result`, `MountHandle`, `SpawnedMountHandle`.
- **The mount manager** — `manager/mounts.rs`: owns the mount runtime and
  lifecycle; today hard-wired to the FUSE backend.
- **Unix primitives** scattered across the engine (the "gate list", §6.5):
  `nix::Flock` (`manager/queue/lock.rs`), `nix::statvfs`
  (`system/filesystem.rs`), `std::os::unix::fs::{MetadataExt, PermissionsExt,
  symlink}` (several sites), `/proc/*` reads (`manager/local.rs`,
  `metrics/memory_probe.rs`, `diagnostics/report.rs`), `rlimit` (CLI preflight
  + `run`).
- **Dependency graph** — `fuse3`/`libfuse3` are currently unconditionally
  linked; they must become Linux/macOS-only (that step is what "adds Windows").

### 5.2 What is already cross-platform (do not touch)
Tile resolution, `DdsRequestor`/request coalescing semantics, the download
orchestrator, cache tiers, prefetch, the job executor, and the ISPC/wgpu DDS
compressor are already pure-Rust and cross-platform. `xplane/paths.rs` already
carries a Windows branch. These are reused as-is by the core and both adapters.

---

## 6. The design

### 6.1 The neutral core (`fuse/support/`)

The core **extends the existing `support/` module** (the established neutral
home in the base) rather than introducing a parallel directory. New, backend-
neutral members:

- **`FsNode`** (neutral identity) — the core's notion of "a thing in the union
  view":
  ```rust
  pub enum FsNode {
      Directory(PathBuf),        // root = empty path
      RealFile(PathBuf),         // a source file, addressed by its virtual path
      VirtualDds(DdsFilename),   // generated texture: ortho or night-lit (kind)
  }
  ```
- **`FsError`** (neutral error surface, small):
  ```rust
  pub enum FsError {
      NotFound,       // FUSE: ENOENT      WinFSP: ERROR_FILE_NOT_FOUND
      NotADirectory,  // FUSE: ENOTDIR     WinFSP: ERROR_NOT_DIR
      Io,             // other I/O failures
  }
  ```
- **Seam types** (`attrs`): `FileAttributes`, `DirEntry`, `StatFsInfo`,
  `VirtualDdsConfig` (moved out of the backend) — provider-neutral
  representations the adapters map to/from their provider types.
- **`DdsRequestor`** + `chunk_to_tile_coords` (moved into the core): the
  on-demand generation requestor both backends drive.
- **`OrthoUnionCore`** (`core`): the union-FS state (index, requestor,
  budget/handle bookkeeping, config) plus the **neutral operations** and a
  per-open `DdsHandle`. Its op surface (each op takes/returns `FsNode`):
  - `lookup(node, name) -> FsNode`
  - `attrs(node) -> FileAttributes`
  - `read(node, fh, offset, size) -> bytes`   (may block-and-generate)
  - `list_directory(node) -> Vec<DirEntry>`
  - `open(node) -> fh` / `release(fh)`        (per-open budget + handle state)
  - `statfs() -> StatFsInfo`
  - `resolve_tile(coords) -> bytes`           (generation, with timeout → placeholder)

**Rule:** core code may not name `fuse3::`, `libc::`/`nix::` Unix symbols, or
`InodeManager`. `fh` numbering and `DdsHandle` memoization live in the core
(both backends need per-open state); inode allocation and the FUSE `readdir`
`.`/`..` inode math stay in the FUSE adapter.

### 6.2 The adapters and the op mapping

**FUSE adapter** (`fuse/fuse3/`) — thin after the split:
- `ino ⇄ FsNode` via `InodeManager`; core result → `ino` via
  `get_or_create_inode` / `create_virtual_inode`.
- Keeps `Fuse3OrthoUnionFS`, `impl Filesystem`, `mount`/`mount_spawned`, and
  the FUSE-specific `FileAttrBuilder` (including the macOS `crtime` fields,
  which move *with the mapper* into the adapter's `FileAttributes → FileAttr`).
- `with_fuse_limits` (FUSE `max_background`/`congestion_threshold`) stays
  adapter-specific.

**WinFSP adapter** (`fuse/winfsp/`) — the new backend, target-gated to
Windows:

| WinFSP op | Delegates to core |
|---|---|
| `ParseName(parent_ctx, name)` | `lookup` → store the `FsNode` **in the file context** |
| `GetAttributes(ctx)` | `attrs(node)` |
| `Read(ctx, offset, size)` (async) | `read(node, fh, offset, size)` — the block-and-generate future ports as-is |
| `ReadDirectory(ctx, offset)` | `list_directory(node)`; batched/offset + `.`/`..` contexts are the one genuine rework |
| `CreateFile(ctx)` | `open(node)` (budget + handle state) |
| `Cleanup(ctx)` | `release(fh)` |
| `GetVolumeInfo` | `statfs()` |
| errors | `FsError → FspError` mapping (3 variants) |
| mount lifecycle | a WinFSP service + a `SpawnedMountHandle`-equivalent; the mount manager gains a neutral mount-handle seam |
| `with_fuse_limits` | dropped — WinFSP manages its own request queue |

The two tree kinds (ortho + night-lit) are handled by the core (`FsNode::
VirtualDds` carries the kind); WinFSP sees one filesystem with two kinds of
virtual files, exactly as FUSE does.

### 6.3 Platform backend selection + mount flow
The consolidated mount flow (`manager/mounts.rs` + the `run` command) selects
the backend by platform and drives a **neutral mount-handle seam**:
- Linux / macOS → `fuse3` adapter.
- Windows → `winfsp` adapter.

The mount manager's public shape is preserved so the CLI, tests, and docs that
reference it keep working; only the backend it instantiates changes per target.

### 6.4 Cross-platform seams (the "installer/enablement" plumbing)
These are the non-FUSE seams that block a working Windows build and are solved
portably, not per-OS:

- **Advisory file lock (package queue):** replace `nix::Flock` with `fs2`
  (`File::lock_exclusive`/`try_lock_exclusive`), which works on both POSIX and
  Windows — one shared implementation, no fork.
- **Volume / disk-capacity identity:** device identity for storage detection
  and `statfs` comes from the volume serial (`GetVolumeInformation` on
  Windows; `MetadataExt::dev` on POSIX) with a path-compare fallback.
- **Scenery package download/install (Tier 1):** the split-part concatenation,
  decompression, and untar are done in **portable Rust** (streaming), not by
  shelling out to `tar`/`split` — so it runs identically on Windows. (Debug
  builds need `overflow-checks` matched to release for large streams — see
  §8.9.)

### 6.5 The Unix-only gate list (site → Windows plan)
Every Unix-only site and its Windows resolution (the concrete "gate list"):

| Site (base) | Unix-only | Windows plan |
|---|---|---|
| `manager/queue/lock.rs` | `nix::fcntl::Flock` | `fs2` (portable) — see §6.4 |
| `system/filesystem.rs` (`available_bytes`) | `nix::statvfs` | `GetDiskFreeSpaceExW` |
| `system/storage_detect.rs` (device identity) | `MetadataExt::dev()` | `GetVolumeInformation` volume serial; `0` + path-compare fallback |
| `manager/local.rs` (chmod) | `PermissionsExt` | cfg no-op (read-only sources; no exec bits on Windows) |
| `night/service.rs`, `night/store.rs` | `PermissionsExt` | cfg no-op (same reason) |
| `metrics/memory_probe.rs` | `/proc/self/status` `RssAnon` | `GetProcessMemoryInfo` (`PROCESS_MEMORY_COUNTERS`); documented `None` fallback if flaky (probe is best-effort) |
| `diagnostics/report.rs` | `/proc/meminfo` | reuse the `hardware.rs` detection (DRY) |
| `fuse/support/attrs.rs` (attr fields) | `MetadataExt` (dev/rdev/blksize/nlink) | map from `std::fs::Metadata`: `dev=rdev=0`, `blksize=4096`, `nlink=1` |
| `manager/symlinks.rs` | `std::os::unix::fs::symlink` | **Phase 4** — Windows leaf becomes a real-file copy (§8.11); Unix keeps symlinks |
| `manager/installer.rs` (`filesystem_of`) | `MetadataExt::dev()` | **Phase 4** (app installer) — covered by the capacity-identity seam |
| `xearthlayer-cli` preflight `FdLimit` | `rlimit` raise | cfg satisfied no-op |
| `xearthlayer-cli` / `xworld_pro` tests | `unix symlink` in tests | `#[cfg(not(target_os = "windows"))]` the affected tests |

Also to verify on the first real Windows build: `memory-stats` on MSVC, and
`crossterm` `use-dev-tty` (console-allocation differences).

---

## 7. Phases

Each phase has a **gate** (what must be true to start) and **completion
criteria** (what must be true to call it done). Every phase is independently
green.

### Phase 1 — Backend split (no behavior change)
**Gate:** none (first).
**Changes:**
- Extract the neutral core into `fuse/support/` per §6.1 (`FsNode`, `FsError`,
  seam types, `DdsRequestor` + `chunk_to_tile_coords`, `OrthoUnionCore` +
  `DdsHandle`).
- Reduce `fuse/fuse3/` to a thin adapter per §6.2; `Fuse3OrthoUnionFS` keeps
  its name and API.
- Port the op-level behavioural tests to the core level.
**Not in Phase 1 (explicitly):** any `winfsp`/`winfsp-sys` dependency or
`fuse/winfsp/` module; any `Cargo.toml` target-gating (that is Phase 2 —
Phase 1 must not change the dependency graph); the gate-list plumbing
(Phase 3); the neutral mount-handle seam (Phase 2/3).
**Completion criteria:**
- Linux and macOS build + test green; behavior preserved (op-level tests pass
  at the core level).
- The core contains no `fuse3::`/Unix symbol (§6.1 rule).
- **No dependency-graph change** and **no new build failures** on any target.
  A clean *Windows* build is **not** a Phase 1 gate (the split is not
  target-gated yet) — that is Phase 2's exit.

### Phase 2 — WinFSP backend
**Gate:** Phase 1 merged and green.
**Changes:**
- Add the `winfsp` adapter (`fuse/winfsp/`) per §6.2: file-context ⇄ `FsNode`,
  the op mapping, `FspError` mapping, the async-read path, the `open`
  sync/async seam (§8.3), and the mount lifecycle/session.
- Target-gate the OS-specific dependencies (`fuse3`/`nix`/`rlimit` →
  Linux/macOS; `winfsp`/`winfsp-sys` → Windows) per §4.3. This is the step
  that "adds Windows" to the dependency graph.
- Add the neutral mount-handle seam to `manager/mounts.rs`.
- Add a **Windows CI build** job (build + test; it must **not** mount — the
  DLL is a runtime dependency absent on the runner).
**Completion criteria:**
- `xearthlayer` + `xearthlayer-cli` **build and test on Windows** (MSVC),
  alongside green Linux + macOS.
- The WinFSP adapter compiles and implements the full op surface against the
  core.
- A local, manual mount (on a machine with WinFSP installed) serves a real
  scenery tree and synthesizes a virtual DDS on read (manual integration test, §9).

### Phase 3 — Wiring + cross-platform enablement
**Gate:** Phase 2 merged and green.
**Changes:**
- Select the backend by platform in the consolidated mount flow (§6.3).
- Implement the cross-platform seams (§6.4): `fs2` advisory lock,
  volume-serial capacity identity, and the Tier‑1 package download/install in
  portable Rust.
- Resolve the remaining gate-list sites for the read path (§6.5).
**Completion criteria:**
- `run` on a WinFSP-equipped machine mounts, serves ortho + night-lit, and the
  package installer downloads/installs a package — with Linux and macOS still
  green.
- No `Unix`-only code path is reached on Windows for the enabled surface.

### Phase 4 — Not-yet-prototyped items (risk-ordered, lowest first)
These are in scope but **were not exercised in the reference implementation**,
so they still need to be built and proven.

1. **WinFSP prerequisite handling** (low risk) — detect a missing WinFSP and
   point the user at the installer, using the existing preflight-prerequisite
   framework (the macFUSE check is the model): a Windows-gated preflight
   `inspect()` that probes for WinFSP and returns an unsatisfied status with an
   install hint, so the user is told *before* a mount is attempted. The
   DLL-resolution that lets the app start without the DLL and find it when
   present is **already handled by the `winfsp` crate's `delayload` + `system`
   features** (§8.5) — this item is just the early, friendly prompt, with the
   mount-time `MountError::Winfsp` remaining a typed backstop. XEarthLayer does
   not bundle or re-sign the driver.
2. **Windows installer / packaging** (low risk) — a Windows build + installer
   for XEarthLayer alongside the existing macOS installer work (signing,
   SmartScreen, and the `winfsp` DLL's availability at install time).
3. **Overlay path** (low risk) — materialise the consolidated
   `yzXEL_overlay/` folder on Windows as **real file copies** from the overlay
   packages, rather than the Unix symlink leaves. The folder, its contents, and
   its behaviour are otherwise identical to Unix:

   - **The user-visible contract is unchanged.** Same folder name, same
     per-region contents, same rebuild semantics — the rebuild already removes
     and recreates the whole folder, so user-modified files are ephemeral on
     every platform, and the copy changes nothing about that. `disable_overlays`
     and `xworld_pro` (SimHeaven X-World Pro coexistence) operate at the folder
     level through pure file I/O, so they work on Windows without modification.
   - **No privilege surface.** Windows symlink creation requires Developer Mode
     or `SeCreateSymbolicLinkPrivilege` on each machine — a new per-machine
     variable, plus detection and user guidance, to save disk space. Copies
     work on any NTFS and match the platform convention of the Windows ortho
     ecosystem (AutoOrtho ships real files into Custom Scenery).
   - **The disk cost is negligible.** Overlay packages are nav-data patches,
     not imagery — the NA-USA overlay is ≈ 5.7 MB. Copying every installed
     overlay is small beside the multi-GB ortho packages they accompany.
   - **Implementation.** The `symlink()` stub in `manager/symlinks.rs` becomes
     a copy-based leaf for `cfg(windows)`; the existing ownership marker
     (`.xearthlayer_symlink`) is kept in a form appropriate to real files so
     the "don't clobber a user folder" guard and the cleanup paths keep working.
     Unix keeps symlinks; nothing in the rest of the pipeline knows the
     difference.
4. **Documentation close-out** (do last, after items 1–3 are merged and
   proven) — not a feature; a **repo-wide documentation consistency pass** that
   makes Windows a first-class supported platform, not just a platforms-list
   update. The obvious platform statements first: `README.md` (the "Linux and
   macOS support" bullet, the install block, and the "Linux with FUSE / macOS
   with macFUSE" requirement line — add Windows + the WinFSP prerequisite);
   `docs/getting-started.md` (the line currently reading "…(Windows support
   planned)"); a new `docs/windows.md` (analog of `docs/macos.md`) with the
   WinFSP install + one-time setup walkthrough; the `docs/README.md` index
   (add the Windows guide link). Then **sweep the rest of the docs** for
   platform assumptions that read Linux/macOS-only and no longer hold —
   install/build paths, log-file locations, temp-dir and disk-space guidance,
   command examples, and any "not supported on" / "not applicable" wording —
   and reconcile them so Windows is treated consistently. Leave
   `.github/release-notes/*` as historical (do not back-edit). Much of this
   pass is judgment-driven and best done by a human (or with the maintainer),
   since it touches tone, examples, and product framing, not just facts.

**Completion criteria (per item):** the item builds green on Windows, is
exercised end-to-end on a real machine, and its failure/edge cases (notably the
overlay copy path — rebuild over user-modified files, and the ownership marker
on a real folder — and the WinFSP-absent preflight path) are covered by
tests or a documented manual check. For item 4 specifically: no in-repo doc
still says "Windows support planned" or omits Windows where it lists the other
platforms, and a human has confirmed the consistency pass is complete. *(If the
project's web page / wiki is maintained outside this repo, that update is a
separate out-of-repo step for the maintainer.)*

---

## 8. Key design decisions and rationale

### 8.1 The `FsNode` neutral identity seam
Identity is the one thing that differs fundamentally between the backends:
FUSE uses numeric inodes (a kernel concept); WinFSP identifies open files by
**file-context objects**. Rather than let identity be a backend concept, the
core operates on `FsNode`, and **both** backends map their kernel identity
onto it. This collapses the "inode model" into two thin adapter-level
translation tables and keeps the core free of any FUSE or WinFSP notion.

### 8.2 Async reads — the dispatcher never blocks
WinFSP's async read path lets the dispatcher thread return `STATUS_PENDING`
immediately; the tile-generation future runs on the mount runtime. So N
dispatcher threads can have N in-flight generations on the tokio runtime — the
same concurrency model the FUSE/tokio backend has. `spawn_task` is the mount
runtime's `RuntimeHandle::spawn` (callable from any thread, which is exactly
the situation: the WinFSP dispatcher thread is not a tokio worker). The
blocking `read` path would instead park a dispatcher thread for up to the
generation budget per read — strictly worse.

### 8.3 The one sync/async seam: `open`
`open` has no async variant, but it must resolve the full path through the
core, whose `lookup` is async (it may stat real source files). The seam is the
**mount runtime's `Handle::block_on`** inside `open` — *not* a bare
`pollster::block_on`. The core does its blocking I/O through `tokio::fs`, which
resolves the blocking pool from the ambient tokio runtime context; a bare
`block_on` provides no such context, so the first `tokio::fs` call panics
("no reactor running"), and on a WinFSP dispatcher thread (which cannot
unwind) that becomes an opaque NTSTATUS to the caller. `Handle::block_on`
installs the mount runtime's context. This is safe because we are on a WinFSP
dispatcher thread, never a tokio worker, so the runtime cannot be nested or
deadlocked — it mirrors what the FUSE adapter does implicitly.

### 8.4 Read-transaction ceiling vs the generation budget (top risk — resolved)
If a read blocks in generation past WinFSP's pending-IRP ceiling, the IRP
errors → black/magenta texture. Findings:
- The ceiling is `VolumeParams::IrpTimeout` (configurable ~60 000–600 000 ms;
  default 60 s). Our per-tile budget is `generation_timeout_secs` (10 s), and
  the core **already** times out generation and falls back to the magenta
  placeholder. So nominal generation sits inside the ceiling with large margin.
- The async-read path (§8.2) means the IRP simply stays pending until the tokio
  task completes or the core's own timeout fires first.
- **Resolution (validated by a blocked-read spike):** `IrpTimeout` does not
  abort in-flight reads; the core's 10 s timeout is the sole effective bound
  and already degrades to a placeholder. Keep `generation_timeout_secs`
  strictly below the IRP ceiling and set `IrpTimeout` explicitly so the margin
  is contractual, not default-dependent.

### 8.5 WinFSP DLL resolution (already solved by the crate)
The bindings crate ships its own import libraries, so **building** does not
need WinFSP. **Running** the exe has two DLL-resolution concerns, and the
reference implementation shows **both are already handled by the `winfsp`
crate's features** — this is not a design open item:

- **Start without the DLL present.** `winfsp = { features = [
  "async-io", "handle-util", "delayload", "system" ] }`. The `delayload`
  feature has `winfsp::build::winfsp_link_delayload()` (called from both
  `xearthlayer-cli/build.rs` and `xearthlayer/build.rs`) emit
  `cargo:rustc-link-lib=dylib=delayimp` + `/DELAYLOAD:winfsp-x64.dll` at the
  **final binary link**, so the process starts even where WinFSP is absent.
  A failed import then surfaces as the typed `MountError::Winfsp` at
  `winfsp_init()` (the mount path), not a raw loader failure.
- **Find the DLL when present.** The `system` feature makes `winfsp_init()`
  resolve the DLL from the standard search path first, then from
  `HKLM\SOFTWARE\WinFsp\InstallDir`. No hardcoded path, no `PATH`
  mutation, no `LoadLibrary` plumbing in our code. (`winfsp_init()` is called
  once on the mount path: `winfsp::winfsp_init().map_err(MountError::Winfsp)`.)

The vendored `third-party/winfsp-sys` (via `[patch.crates-io]`) is **only**
to skip bindgen/libclang at build time so Windows builds stay hermetic; the
high-level `winfsp` crate (from crates.io) provides the `delayload` + `system`
behaviour above. Re-verify the `delayload`/`system` feature names and the
registry key against the crate version you land on (the reference pins
`winfsp 0.13`).

What is **not** solved by the crate — and is the real Phase 4, item 1 work —
is the *early, friendly "install WinFSP" prompt*: a Windows-gated preflight
check (mirroring the macFUSE preflight) so the user is told before a mount is
attempted, instead of hitting the `MountError::Winfsp` backstop at mount time.

### 8.6 Case insensitivity
Windows paths are case-insensitive while the index keys are case-sensitive.
X-Plane reads back the exact names we enumerate, so this is normally
transparent; `case_preserved_names` is enabled, and a lookup mismatch surfaces
in e2e with a case-insensitive-compare fallback in the WinFSP resolve path.

### 8.7 Security / ACL
The volume is read-only; `sz_security_descriptor = 0` (driver default ACL),
same-user access. No `get_security_by_name`/custom-ACL work is expected.

### 8.8 Magenta placeholder parity
All magenta output is XEarthLayer synthesizing a valid DDS in response to an
image-data failure (never X-Plane behavior — a missing DDS is fatal to
X-Plane). Two forms: a whole-tile solid-magenta placeholder (`fuse/placeholder`,
4096×4096 BC1, `255,0,255`) on generation timeout/channel failure/invalid
bytes, and per-chunk magenta fill in an otherwise valid tile (chunk
download/decode failure; such a tile is served but **never** cached, gated on
`is_complete()`). The core's `resolve_tile` preserves this exact fallback
behavior, so Windows parity holds.

### 8.9 Debug/release overflow parity (large streams)
The Tier‑1 streaming decompressor accumulates a byte counter; for compressed
streams well over 4 GiB (a 12 GB package) a `u32` total-in counter can overflow
in debug builds and panic, while release wraps silently. Debug and release
behavior must match (integer overflow is not normally trapped in this codebase's
intent), so the dev profile's overflow-checks are aligned with release for the
portable streaming path. Re-verify on the target base.

### 8.10 WinFSP bindings strategy
The WinFSP API is a C ABI (exposed via `winfsp-x64.lib` import libraries and
`winfsp.h` headers). Rust needs a FFI binding layer. Three approaches are
available:

| Criterion | **A — Vendor in-repo** (prototype) | **B — Use `winfsp-sys` from crates.io** | **C — Generate at build time (bindgen)** |
|---|---|---|---|
| Build prerequisite | None (`.lib` + `bindings.rs` in repo) | None *if* the crate ships the `.lib` files; otherwise WinFSP SDK on PATH | WinFSP SDK headers + libclang on every build machine and CI image |
| Update path | Re-run bindgen locally, commit updated `bindings.rs` + `.lib` | Bump crate version in `Cargo.toml` | Automatic — re-generates each build against installed SDK |
| Staleness risk | Bindings frozen until a maintainer re-generates | Crate may lag WinFSP releases; if abandoned, same as A | Always tracks installed SDK version |
| Repo impact | ~7 500 lines of generated Rust + ~110 KB binary `.lib` files | Minimal (crate reference only) | Minimal (only a `build.rs` + `bindgen` dep) |
| Reproducibility | Fully pinned in the repo | Pinned via `Cargo.lock` | Depends on SDK version present on the build machine |
| CI simplicity | No extra install step | No extra install step (if crate ships libs) | Must install WinFSP SDK in the CI image before `cargo build` |
| Review burden | Large diff when WinFSP API changes | Small diff (version bump) | None visible in the repo |

**Decision: A — vendor in-repo.**

Rationale:
- WinFSP's C ABI is **stable and rare to change** (it has not changed in
  several years; the `winfsp` crate version we target pins a specific WinFSP
  release). The staleness risk is low in practice.
- It keeps Windows CI and local builds **hermetic**: no SDK install step, no
  libclang dependency, no bindgen at build time. This was a hard requirement
  for the "minimize extra installs" constraint.
- The `.lib` import libraries are **binary artifacts that must exist at link
  time** regardless; if we don't ship them in the repo, we must either rely on
  the crates.io crate to ship them (which the current `winfsp-sys` crate does
  not — it expects them from the system) or generate them (which requires the
  SDK). Vendoring is the simplest self-contained path.
- The generated `bindings.rs` is mechanical and reviewable as a one-time
  artifact; future updates are a diff of the generated file, which is easy to
  sanity-check against the WinFSP release notes.

**Update procedure:** When the `winfsp` crate is bumped to a newer version
that targets a newer WinFSP release, re-run `bindgen` against the new headers,
replace `third-party/winfsp-sys/src/bindings.rs` and the `.lib` files, and
commit. The `winfsp` safe-wrapper crate's version and the vendored
`winfsp-sys` must stay in lockstep.

**Licensing:** WinFSP is MIT-licensed. The vendored `bindings.rs` is generated
from the MIT-licensed `winfsp.h`; the `.lib` files are binary artifacts of the
same MIT-licensed project. A `LICENSE` file in `third-party/winfsp-sys/`
carrying the MIT text and an attribution note ("Generated from WinFSP
[vX.Y.Z](https://github.com/billziss-gh/winfsp), MIT © Bill Zissimopoulos")
satisfies the MIT attribution requirement for redistribution.

### 8.11 Overlay materialisation on Windows: copies, not links

The three candidates were Windows symlinks, hard links, and file copies. Hard
links were rejected outright: two names for the same bytes cannot express "my
version shadows the package version" — editing through the Custom Scenery name
edits the shared package data, which inverts the override model. That left:

| | Windows symlinks | File copies |
|---|---|---|
| Privilege | Developer Mode or admin, per machine | none |
| Override model | delete link, drop your file | edit the file directly |
| Rebuild/update semantics | wipes user files (existing behaviour) | identical |
| `xworld_pro` / `disable_overlays` | unchanged | unchanged |
| Disk cost | zero | overlay data size (≈ MB per region) |
| Code | `symlink_file` + capability check + error UX | `fs::copy` |

Copies win on every axis except disk, and the disk cost is measured in
megabytes against a product whose ortho packages run to gigabytes. Unix keeps
symlinks; the mechanism is an implementation detail below the folder-level
contract that every consumer (X-Plane, the preflight checks, the package
commands) already treats as opaque.

Note for the documentation pass (Phase 4 item 4): `docs/x-world-pro.md`
step 4 references `install_XWP_Linux_Mac.sh`, a Unix-only script run by the
user under X-World Pro's own instructions; the Windows guide must point
Windows users at X-World Pro's Windows instructions instead. XEarthLayer
itself never runs it.

---

## 9. Assumptions to re-validate on the target base

These risk-bearing assumptions were **validated against a real Windows box** in
the reference implementation; treat them as starting points to re-check on the
base you land on (they may shift if the base's FUSE backend, the generation
budget, or the WinFSP crate version change):

- **Read-transaction ceiling vs generation budget** — the core's
  `generation_timeout_secs` bound + placeholder fallback is sufficient and
  `IrpTimeout` does not abort in-flight reads (§8.4). Re-check against the
  target's `winfsp` crate version and default `IrpTimeout`.
- **Directory-enumeration semantics** — FUSE `off_t`/`.`-`..` mapping to
  WinFSP batched/offset enumeration + file-context `.`/`..` is an isolated,
  property-testable helper (§6.2).
- **Identity re-mapping** — the `FsNode` seam is sufficient for both backends
  and keeps the core free of provider concepts (§8.1).
- **The `open` sync/async seam** — `Handle::block_on` supplies the `tokio::fs`
  context without deadlock (§8.3).
- **Case insensitivity** — enumerate-then-read-back stays transparent (§8.6).
- **ACL expectations** — read-only, default ACL is enough (§8.7).
- **ISPC kernels on Windows** — `intel_tex_2` prebuilt x86_64 kernels load and
  produce correct BC1/BC3 on the target CPU (a pure-Rust software fallback
  exists).
- **`winfsp` crate maturity** — the 0.13.x `!Send`/`Drop` subtleties are
  handled by following the crate's recommended service architecture, not the
  raw sys bindings.

---

## 10. Reference implementation (oracle)

A working reference implementation of this design exists on a fork branch
(`karlrado/xearthlayer`, `feature/winfsp-backend`; its Phase‑1 checkpoint is
`feature/windows-backend-split`) and was exercised end-to-end in X-Plane 12 on
Windows — package install, a live WinFSP mount, and scenery rendering. It is
referenced **by existence only** as evidence that the design is feasible and as
an **oracle** to diff against while regenerating each phase on this base. It is
**not** a merge source: each phase here is regenerated against the base using
this document as the spec, so the result is clean and independent of the
reference's history.

---

## 11. Open questions / decisions to confirm

1. **Release target** — 0.6.0? When is it cut, and is this in scope for it?
2. **Shape of the backend split** — the one part that reorganizes shared code.
   Align on the seam (neutral core in `support/`; `fuse3` as a thin adapter)
   before it is written, especially if union-FS changes are already planned.
3. **WinFSP bindings + licensing** — resolved: vendor in-repo (§8.10).
4. **PR shape** — one PR with phase-ordered commits, or stacked PRs (each phase
   merged green before the next builds on it)?
5. **Windows CI** — a Windows build job (build + test, no mount) from the start
   (Phase 2), or a lighter gate initially?

---

## 12. Out of scope

- Bundling, signing, or re-signing the WinFSP driver (§4.4).
- Any non-Windows behavior change; the FUSE backend's public API and behavior
  are preserved.
- The two tree kinds beyond what the core already models (ortho + night-lit).
- Re-architecting prefetch, cache, or the job executor (already cross-platform
  and reused as-is, §5.2).
