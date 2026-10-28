# WinFSP Backend — Phase 2 Design

> **Status:** Proposed — awaiting Karl's approval before any code.
> **Parent:** [windows-scope.md](windows-scope.md) (Path A scoping; §4.7 spike, §4.11 gate list)
> **Predecessor:** [windows-backend-split.md](windows-backend-split.md) (Phase 1 — complete; §4.6 previews this phase)
> **Branch:** `feature/winfsp-backend` (cut from Phase 1 HEAD `c9b4ac0`)
> **Date:** 2026-10-27

## 0. Decisions recorded (Karl, 2026-10-27)

1. **Scope:** WinFSP backend + dependency target-gating + gate-list plumbing for a
   **buildable, mountable `xearthlayer.exe`**. The **scenery-package installer path
   is deferred** (Karl can copy an installed scenery package from another machine
   for testing if needed). Installer-adjacent machinery (`flock` queue, overlay
   symlinks) is therefore **compile-stubbed, not implemented**.
2. **Design-doc-first**, this doc, approved before code.
3. **Fresh branch** `feature/winfsp-backend`.
4. Same process: push per commit, CI green at every commit, **plus a new
   `windows-latest` (MSVC) `verify` job** in fork CI so every commit also
   compiles/tests on Windows.
5. **Config format (2026-10-27):** `config.ini` keeps rust-ini's
   **escape-enabled** format (parser default; `enabled_escape: true`). The
   writer's historical double-escape was fixed in commit 2 — `path_to_string`
   is a plain conversion and the `toml_escape` calls at the INI boundary are
   the sole escapers, so values round-trip exactly (regression tests in
   `config/writer.rs`). Accepted consequence: a *hand-written* Windows path
   in `config.ini` must use doubled backslashes (`C:\\Users\\...`) or
   forward slashes; a raw `C:\Users\...` is mangled on read (`\U` becomes
   `U`). Flipping to a no-escape format (`ParseOption::enabled_escape =
   false`, writer stops escaping) remains a small, well-contained option —
   deferred.

## 1. Purpose and scope

Phase 1 extracted the backend-neutral core (`OrthoUnionCore` in
`fuse/support/core.rs`) so a second filesystem backend can drive the same
tile-generation engine. Phase 2 delivers that backend for Windows:

**In scope**
- `winfsp`-based filesystem backend serving the ortho union (virtual DDS
  generation, real passthrough files, lit textures) — read-only.
- A `windows-latest` CI job so the workspace builds and unit-tests on Windows.
- Target-gating of Unix-only dependencies (`fuse3`, `nix`, `libc`, `rlimit`)
  and the Unix-only code sites (the "gate list", §6) — this is the step that
  "adds Windows" to the dependency/build matrix.
- Mount/unmount lifecycle from `manager/mounts.rs` on Windows
  (directory mount inside `X-Plane 12\Custom Scenery\zzXEL_ortho`).
- The read-transaction **timeout spike** (parent doc §4.7 step 1) as the first
  Windows code, to de-risk the top unknown (§4.4).

**Out of scope (deferred per decision 1)**
- Scenery package install/upgrade on Windows; the `flock` queue lock and the
  overlay symlink machinery ship as explicit "not yet supported on Windows"
  stubs so the workspace builds.
- Windows installer/packaging (NSIS/etc.), code signing, service registration.
- Write support of any kind (the volume is read-only on every platform).

## 2. Crate choice: `winfsp` 0.13.2+winfsp-2.1

The official Rust bindings (`winfsp`, by the WinFSP project; source audited
from the 0.13.2 registry crate during this design):

- Built against **WinFSP 2.1** (this machine has WinFSP 2.1.25156 — match).
- Safe, sealed trait API over `winfsp-sys` (raw bindings).
- **`winfsp-sys` bundles its own `winfsp/lib` import libraries and `winfsp/inc`
  headers** (default "local" mode; the `system` feature that reads the
  installed WinFSP directory is opt-in). Consequences:
  - **Building does not require WinFSP installed** → the `windows-latest` CI
    job needs no install step.
  - **WinFSP is a runtime dependency** (delay-loaded DLL), present on Karl's
    box, absent on CI runners — CI must never *mount*, only build/test.
- Feature surface: `default = ["full", "handle-util"]`; `full` = `notify` +
  `delayload` + `async-io`. We will use
  `default-features = false, features = ["async-io", "handle-util"]`:
  - `async-io` — async `read`/`write`/`read_directory` with a user-supplied
    executor (`spawn_task`), §3.3.
  - `notify` — not needed (we push no change notifications; the driver does
    not modify its own tree).
  - `delayload` — not needed for the first cut; the bundled import library
    links directly. Can be enabled later if we want the exe to start without
    WinFSP present.
- **License: GPL-3.0** (same as the WinFSP runtime). The repository is MIT.
  For this solo fork used privately that is acceptable; if upstream ever
  distributes a Windows build they must address GPL themselves (relinkable
  runtime component, standard WinFSP integration story). Recorded, not a
  blocker.

**Rejected alternative:** `winfsp-sys` directly (raw FSP vtable). The safe
trait layer is well-documented, handles the operation-context/TLS machinery
and the async bridging (request snapshotting, in-flight barrier, `Drop`
ordering) that would be hundreds of lines of `unsafe` to write ourselves.

## 3. API model (what the crate actually looks like)

### 3.1 Core traits

```text
trait FileSystemContext: Sized {
    type FileContext: Sized;

    // required (no default):
    fn get_security_by_name(...) -> Result<FileSecurity>;
    fn open(&self, file_name, create_options, granted_access, file_info) -> Result<Self::FileContext>;
    fn close(&self, context: Self::FileContext);

    // defaulted (return STATUS_INVALID_DEVICE_REQUEST unless overridden):
    fn cleanup(&self, context, file_name, flags);
    fn get_file_info(&self, context, file_info);
    fn read(&self, context, buffer, offset) -> Result<u32>;
    fn read_directory(&self, context, pattern, marker, buffer) -> Result<u32>;
    fn get_volume_info(&self, out_volume_info);
    fn dispatcher_stopped(&self, normally);
    // ... write/create/rename/... (unused: read-only volume)
}

trait AsyncFileSystemContext: FileSystemContext + Sync {
    fn read_async(...)      -> impl Future<Output = Result<u32>> + Send;
    fn read_directory_async(...) -> impl Future<...> + Send;
    fn spawn_task(&self, future: impl Future<Output = ()> + Send + 'static);
}
```

Notes from the source:

- The driver invokes callbacks from **multiple dispatcher threads** under the
  default **`FineGuard`** strategy (exclusive-shared lock per op class, no lock
  on `read`). Both `T` and `T::FileContext` must then be `Sync`. Our core is
  already internally synchronized (`Arc` + `DashMap` + atomics) and the FUSE
  adapter already serves concurrent ops — `FineGuard` fits with no changes.
  (`CoarseGuard` would serialize everything — rejected.)
- `FileContext` is **only ever seen through `&`** and may be dropped by the
  driver at any time; mutation must go through interior mutability. We store
  immutable data only (`FsNode`, `fh`) → `Arc<FileEntry>` is `Send + Sync`.
- `open` receives the **full relative path** (e.g. `NA\us\ca\SF\...\.dds`),
  not a parent handle + name like FUSE lookup. Path walk is per §3.4.
- `get_security_by_name` is **required** even for read-only, non-ACL
  filesystems; the documented opt-out is returning
  `sz_security_descriptor == 0` (driver applies its default ACL) — exactly
  our case (parent user opens the files; no ACL policy needed).
- Directory enumeration is **marker-based** (`DirMarker` = name of the last
  entry already returned, or `.`/`..`), not an `off_t` offset, and entries
  are written into a driver-managed `DirBuffer` via `DirBufferLock`.
- `VolumeParams` exposes the **`IrpTimeout`** knob (see §4.4): pending-IRP
  timeout, valid range 60 000–600 000 ms — WinFSP's read-transaction ceiling.

### 3.2 Host and service

```text
FileSystemHost<T>           // owns the FSP_FILE_SYSTEM
  new_async(params, ctx)    // select the async interface
  mount(path | "X:")        // directory path or drive letter — both supported
  unmount() / stop()        // unmount is safe when not mounted
FileSystemService<T>        // FspServiceCreate/Loop/Stop wrapper
  Builder.with_start(|| -> T).with_stop(|t| -> ()).build(name, init)
  start()                   // worker thread: FspServiceAllowConsoleMode + FspServiceLoop
  stop() / join()           // Drop joins the worker, then deletes the service
```

The crate's recommended architecture is the service wrapper even for a
console app: `start()` runs `FspServiceAllowConsoleMode` + `FspServiceLoop`
on a worker thread; `Drop` stops, joins, and deletes in a safe order.
**Decision: use `FileSystemService` (console mode)** rather than driving
`FileSystemHost` directly — it is the crate-blessed lifecycle, and our
`xearthlayer.exe` is a foreground process, not an SCM service.

### 3.3 Async reads on our tokio runtime

`AsyncFileSystemContext::spawn_task` is where the future goes; the crate is
executor-agnostic. **Decision: `spawn_task` = `RuntimeHandle::spawn`** on the
runtime `manager/mounts.rs` already owns (`service.runtime_handle()`).
`RuntimeHandle::spawn` is callable from any thread, which is exactly the
situation (the WinFSP dispatcher thread is not a tokio worker).

This matters for concurrency: with async reads the dispatcher thread returns
`STATUS_PENDING` immediately and **never blocks**, so N dispatcher threads
can have N in-flight tile generations, each executing on the tokio runtime —
the same model FUSE/tokio has today. The sync `read` path would instead park
a dispatcher thread for up to the generation budget per read.

### 3.4 The one sync-async seam: `open`

`open` has no async variant. It must resolve the full path through the core
(`lookup` is `async` because it may stat real source files). **Decision:
`pollster::block_on` inside `open`** (pollster is already a workspace
dependency and is the codebase's established pattern for blocking on a
tokio future from a non-runtime thread). This is safe — we are on a WinFSP
dispatcher thread, not a tokio worker — and it mirrors what the FUSE adapter
does implicitly (fuse3 runs the async ops on its own executor).

## 4. Architecture

### 4.1 Module layout

```text
xearthlayer/src/
  fuse/
    support/               # Phase 1 neutral core — unchanged
    fuse3/                 # FUSE adapter — unchanged
    winfsp/                # NEW (all behind #[cfg(windows)] in mod tree)
      mod.rs               # WinFspOrthoUnionFS builder + mount/unmount + WinFspSession
      context.rs           # impl FileSystemContext + AsyncFileSystemContext
      error.rs             # FsError → NTSTATUS mapping (pure, unit-tested)
      resolve.rs           # path-walk over core.lookup (pure fn where possible)
  manager/
    mounts.rs              # cfg(windows) branch in the consolidated-mount flow
  system/
    unmount.rs             # Windows impls (session-based unmount, is_path_mounted)
    hardware.rs            # GlobalMemoryStatusEx impl of try_detect_total_memory
    filesystem.rs          # GetDiskFreeSpaceExW impl of available_bytes
  metrics/memory_probe.rs  # Windows RSS probe (or documented fallback)
```

Mirrors the Phase 1 layout philosophy: neutral core, one thin adapter per OS,
small platform seams in `system/` (the same shape as `system/unmount.rs`
already has for Linux/macOS).

### 4.2 `WinFspOrthoUnionFS` (the builder)

A builder **mirroring `Fuse3OrthoUnionFS`'s** surface, so the call site in
`manager/mounts.rs` changes by as little as possible:

```rust
WinFspOrthoUnionFS::new(index, dds_client, expected_dds_size)
    .with_geo_index(geo_index)
    .with_state_observer(observer)
    .with_pinned_budget(budget)
    .with_lit_source(source)          // night
    .with_metrics(metrics)
    .with_timeout(generation_timeout) // core's per-tile budget
    // .with_fuse_limits(max_bg, congestion) — DROPPED: no WinFSP equivalent;
    // WinFSP bounds its own queue via IrpCapacity. (Phase 1 doc §4.6.)
```

Internally it owns the `Arc<OrthoUnionCore>` (constructed exactly like the
FUSE adapter does today) plus the `RuntimeHandle`. It exposes:

```rust
fn mount(&mut self, mountpoint: &str) -> Result<WinFspSession, MountError>;
impl WinFspSession {
    fn unmount(&mut self);        // host.unmount() + service stop/join
}
```

### 4.3 File context and op mapping

```rust
type FileContext = Arc<FileEntry>;

struct FileEntry {
    node: FsNode,              // from core.lookup walk
    fh: u64,                   // from core.open; 0 = stateless (real files, dirs)
}
```

| WinFSP op (crate) | Implementation |
|---|---|
| `get_security_by_name` | `Ok(FileSecurity { reparse: false, sz_security_descriptor: 0, attributes: READONLY })` — no ACL policy |
| `open(path)` | walk components: `pollster::block_on(core.lookup(parent, name))` per component → `FsNode`; then `core.open(&node)` → `fh`; `Arc::new(FileEntry)` |
| `cleanup(ctx)` | if `fh != 0` → `core.release(fh)` (interior mutability via `AtomicU64` or `Mutex`), idempotent |
| `close(ctx)` | drop `Arc` (no core work; release already happened in `cleanup`) |
| `get_file_info(ctx)` | `pollster::block_on(core.attrs(&node))` → `FileInfo` (size, times, attrs) |
| `read_async(ctx, buf, off)` | `core.read(&node, fh, off, len).await` → `Bytes` → copy into `buf`; return len |
| `read_directory_async(ctx, pattern, marker, buf)` | `core.list_directory(&node)` → `Vec<DirEntry>`; resume after `marker` (pure helper `entries_after(marker)`); fill via `DirBuffer::acquire` + `DirInfo` writes; handle `.`/`..` |
| `get_volume_info` | `core.statfs()` + `VolumeParams` (name "XEarthLayer", 512-byte sectors, serial, read-only flag) |
| `dispatcher_stopped` | log + (later) notify diagnostics |
| errors | `error.rs`: `NotFound → STATUS_OBJECT_NAME_NOT_FOUND`, `NotADirectory → STATUS_NOT_A_DIRECTORY`, `Io →` NTSTATUS mapping (pure fns, unit-tested) |

Directory enumeration note: the core's `list_directory` is **one-shot**
(`Vec<DirEntry>`), which is simpler than FUSE's `off_t` streaming. The only
WinFSP-specific logic is resuming after the driver's marker when the buffer
fills mid-listing — a pure, testable function over `Vec<DirEntry>`.

`fs_node → attrs` mapping for `DirInfo`/`FileInfo`: the neutral
`FileAttributes` (Phase 1) carries `size/blocks/times/kind/perm/nlink/
uid/gid/rdev/blksize`. On Windows `uid/gid/rdev` are unused by the driver;
`perm` maps to `FILE_ATTRIBUTE_READONLY` (files) — the volume is read-only.

### 4.4 The top risk: read-transaction timeout vs the 10 s generation budget

Parent doc §4.4 #2: X-Plane does plain synchronous `ReadFile` (no mmap —
Karl-confirmed 2026-09-28). If a read blocks in tile generation past
WinFSP's pending-IRP ceiling, the IRP errors → black/magenta texture.

Research findings (winfsp 0.13.2 source, this design):

1. The ceiling is `VolumeParams::IrpTimeout`, **configurable 60 000–
   600 000 ms** (1–10 min). The WinFSP default is 60 s.
2. Our per-tile generation budget is `generation_timeout_secs` (10 s) — the
   core *already* times out generation and falls back to the magenta
   placeholder. So nominal generation (10 s) sits inside the default
   ceiling (60 s) with 6× margin; only pathological stalls (dead DDS
   upstream, disk hang) could approach it.
3. The async read path (§3.3) means the dispatcher never blocks; the IRP
   simply stays pending until the tokio task completes or the core's own
   timeout fires first.

**Mitigation plan (in order):**
- Keep `generation_timeout_secs` (core timeout → placeholder) strictly below
  the IRP ceiling.
- Set `IrpTimeout` explicitly to 300 000 ms (5 min) so the margin is
  contractual, not default-dependent — *if* the spike shows the default is
  already ≥ 60 s, setting it explicitly is still correct.
- **Spike first** (commit 3, parent doc §4.7 step 1): a minimal WinFSP mount
  (kept in-repo as `examples/winfsp_spike.rs`, `#[cfg(windows)]`, run
  locally only) whose read callback sleeps for a configurable duration;
  observe `ReadFile` success vs error at 5 s / 10 s / 70 s / 310 s. Record
  findings as an addendum to this doc. This is cheap, decisive, and removes
  the one unknown that could force an architecture change.

### 4.5 Mount model and lifecycle

- **Mount point:** directory mount at
  `<X-Plane 12 install>\Custom Scenery\zzXEL_ortho` (parent doc §4.3A:
  X-Plane only scans its own tree, so a drive letter buys nothing and costs
  a letter). WinFSP supports directory mounts on NTFS; `mount()` takes a
  plain path.
- **Lifecycle:** `FileSystemServiceBuilder::with_start` (build host with
  `new_async`, `start()`, `mount(path)`) / `with_stop` (`unmount()`,
  `stop()`) → `build` → `start()` (console mode). The session handle keeps
  the service alive; `Drop` order is handled by the crate (join worker,
  delete service).
- **`manager/mounts.rs`:** the sole production caller (line ~573–597) gets a
  `#[cfg(target_os = "windows")]` branch that builds `WinFspOrthoUnionFS`
  from the same service/index/dds inputs and stores the `WinFspSession`.
  The `consolidated_session` field and the panic-handler registration are
  cfg-gated to the platform handle type. **Decision: cfg-gate rather than a
  `MountBackend` trait** — a single binary targets exactly one OS, the two
  handle types have different lifetimes, and a trait would force type
  erasure (the codebase's Phase 1 precedent is "neutral core + thin
  per-OS adapter + small platform seams", not a cross-OS trait).
- **`system/unmount.rs`:** Windows impls —
  - `unmount_fuse` → escalate path calls the registered WinFSP session's
    `unmount()` (the panic handler already registers the mountpoint; on
    Windows it also carries the session handle).
  - `is_path_mounted` → in-process mount registry first (authoritative for
    our own mounts), with a best-effort `GetVolumeInformation` volume-name
    check for "is something mounted here at all" (package status display).
  - `is_stale_fuse_mount` → a mount whose service worker is dead (probe via
    the session's `join`-state / `GetVolumeInformation` failing).
- **Preflight/CLI:** `FdLimit` (rlimit raise) becomes a satisfied no-op on
  Windows (documented: Windows handles are per-process and there is no
  soft-limit raise to do).

### 4.6 Dependency target-gating ("adds Windows")

`xearthlayer/Cargo.toml`:

```toml
[target.'cfg(not(target_os = "windows"))'.dependencies]
fuse3 = { git = "...", ... }     # stays a git fork for macOS — untouched
nix   = "0.31"                   # flock + statvfs only
libc  = "0.2"                    # used only by the fuse3 adapter (errno, uid)

[target.'cfg(windows)'.dependencies]
winfsp = { version = "0.13", default-features = false,
             features = ["async-io", "handle-util"] }
windows = { version = "0.61", features = ["Win32_System_SystemInformation",
                                          "Win32_Storage_FileSystem"] }
```

(`windows` for `GlobalMemoryStatusEx` / `GetDiskFreeSpaceExW` /
`GetVolumeInformation` — the same crate `winfsp` itself depends on, so no
new major version.)

`xearthlayer-cli/Cargo.toml`: `rlimit` → `cfg(not(windows))`.

`xearthlayer/build.rs`: unchanged (no `delayload` feature → no
`winfsp_link_delayload` needed). The `fuse3`/macOS `stdc++` lines remain
harmless on Windows (already `cfg`'d).

Workspace crates `xearthlayer-package`, `xearthlayer-publish`,
`xearthlayer-publisher`: no Unix-only deps found (verified by grep during
this design) — they should build as-is; the windows CI job will confirm.

## 5. Gate list (every Unix-only site → Windows plan)

Compiled from a full-workspace grep (`std::os::unix`, `nix::`, `libc::`,
`rlimit`, `/proc/`) during this design. "Stub" = compile on Windows, return
an explicit `UnsupportedOnWindows` error; exercised only by the deferred
installer path.

| Site | Unix mechanism | Windows plan |
|---|---|---|
| `system/unmount.rs` | `fusermount`/`umount`, `/proc/mounts` | §4.5: session unmount + volume-info probe |
| `system/hardware.rs` (`try_detect_total_memory`) | `/proc/meminfo` / `sysctl` | `GlobalMemoryStatusEx` (falls back to 8 GB like today if it fails) |
| `diagnostics/report.rs:~228` | `/proc/meminfo` | reuse `hardware.rs` detection (DRY) |
| `system/filesystem.rs` (`available_bytes`) | `nix::statvfs` | `GetDiskFreeSpaceExW` |
| `metrics/memory_probe.rs` | `/proc/self/status` `RssAnon` | `PROCESS_MEMORY_COUNTERS` (`GetProcessMemoryInfo`); if the crate proves flaky, documented `None` fallback (probe is best-effort) |
| `fuse/support/attrs.rs:99` | `MetadataExt` (dev/rdev/blksize/nlink) | map from `std::fs::Metadata`: `dev=rdev=0`, `blksize=4096`, `nlink=1`; symlink detection already goes through portable `file_type()` |
| `manager/local.rs:692` | `PermissionsExt` (chmod) | cfg no-op (read-only sources; no exec bits on Windows) |
| `night/service.rs:709`, `night/store.rs:328,402,500` | `PermissionsExt` | cfg no-op (same reason) |
| `system/storage_detect.rs:39` | `MetadataExt::dev()` (device identity) | `GetVolumeInformation` volume serial; `0` + path-compare fallback |
| `manager/queue/lock.rs` | `nix::fcntl::Flock` | **STUB (deferred with installer)** — `acquire` returns `UnsupportedOnWindows` |
| `manager/installer.rs:192` (`filesystem_of`) | `MetadataExt::dev()` | **STUB (deferred)** — covered by the same `UnsupportedOnWindows` surface |
| `manager/symlinks.rs` | `std::os::unix::fs::symlink` | **STUB (deferred)** — create/remove return `UnsupportedOnWindows` |
| `xworld_pro/detect.rs` (tests) | `std::os::unix::fs::symlink` in tests | `#[cfg(not(target_os = "windows"))]` the affected tests |
| `xearthlayer-cli` tests (`handlers.rs:832`, `effects.rs:1994`) | unix symlink in tests | same: gate the affected tests |
| `xearthlayer-cli/src/preflight/system.rs` (`FdLimit`) | `rlimit` raise | cfg satisfied no-op (§4.5) |

Also on the verify list from Phase 1 §5 (first real Windows build will tell):
`memory-stats` crate on MSVC, `crossterm` `use-dev-tty` feature on Windows
(console allocation differences).

## 6. Commit plan (TDD; every commit green on Linux + macOS + Windows)

1. **CI: `verify-windows` job.** `windows-latest`, MSVC, `dtolnay/rust-toolchain@stable`
   + `rust-cache`, then `make verify` (format-check, clippy `-D warnings`,
   `test-strict`). No dependency-install step (winfsp-sys bundles its link
   inputs; no `libfuse3-dev` equivalent needed). No WinFSP install, no mount
   tests — CI builds and unit-tests only. *Green = the job itself runs on
   today's tree or the gating in commit 2 lands in the same push sequence.*
2. **Clean Windows build (the "adds Windows" commit).** Dependency
   target-gating (§4.6) + every §5 gate-list site cfg-stubbed/implemented
   (platform impls that are pure are TDD'd with unit tests; stubs get a
   test asserting the `UnsupportedOnWindows` error). Exit: `make verify`
   green on all three CI runners; `cargo build` produces `xearthlayer.exe`.
3. **Read-timeout spike.** `examples/winfsp_spike.rs` (Windows-gated):
   minimal `winfsp` mount, read callback sleeps 5 s / 10 s / 70 s / 310 s
   (`IrpTimeout` variations), local `ReadFile` client observes
   success-vs-error. Run on Karl's box; findings appended to §4.4. If the
   results force changes, §4.4 is updated *before* commit 4.
4. **`fuse/winfsp/error.rs` + `resolve.rs` + `context.rs` core.** Pure
   mapping/resolution functions first (TDD: FsError→NTSTATUS table tests,
   `entries_after(marker)` property tests, path-walk unit tests over a fake
   core), then the trait impls. Unit-testable without a live mount because
   every WinFSP-specific shape is isolated in pure functions.
5. **Mount lifecycle.** `WinFspOrthoUnionFS` builder, `WinFspSession`,
   `system/unmount.rs` Windows impls, panic-handler cfg wiring.
   Local smoke: mount a temp folder, list/read a real file via Explorer,
   unmount cleanly (manual checklist — CI cannot mount).
6. **`manager/mounts.rs` Windows branch** — the consolidated mount flow
   builds the WinFSP backend from the same service inputs;
   `with_fuse_limits` dropped on this path.
7. **Polish + docs:** `windows-scope.md` §4.9 phase table updated
   (Phase 2 status), README/`docs/dev` README index, CHANGELOG entry.

Local end-to-end (not CI, Karl's box, two X-Plane 12 installs available):
mount into `Custom Scenery\zzXEL_ortho`, X-Plane lists the regions, loads
ortho textures in-scene without sustained magenta, clean unmount with
X-Plane still running (busy-handle behaviour observed and recorded).

## 7. Verification and exit criteria (Phase 2)

1. `make verify` green at every commit on **ubuntu, macOS, and
   windows-latest**.
2. `xearthlayer.exe` builds from a clean Windows checkout **without WinFSP
   installed** (link-time independence), and mounts when WinFSP is present.
3. The spike findings are recorded in §4.4 and the chosen `IrpTimeout` /
   timeout-margin policy is in code.
4. X-Plane 12 on Karl's box browses the mounted `zzXEL_ortho` directory and
   loads generated ortho (local e2e, recorded).
5. Linux + macOS behaviour unchanged: FUSE path byte-identical in
   `manager/mounts.rs` (only an added cfg branch), all existing tests green.
6. Coverage threshold (tarpaulin, Linux-measured) unchanged — the
   Windows-gated module is outside the Linux build graph by design; the
   pure helper functions it extracts are tested where they live.

## 8. Risks and mitigations

| Risk | Mitigation |
|---|---|
| IRP timeout ceiling vs generation stall (top risk) | §4.4: core timeout (10 s) ≪ explicit `IrpTimeout` (5 min); spike proves behaviour before backend work |
| `get_security_by_name`/ACL expectations of X-Plane | read-only volume, `sz_security_descriptor = 0` (driver default ACL); same-user access; e2e in §7.4 |
| Directory-enumeration marker semantics differ from FUSE `off_t` | isolated pure `entries_after` helper, property-tested; one-shot core listing simplifies state |
| `open` sync/async seam blocking a dispatcher thread | bounded by `core.lookup` (metadata reads only, no generation); pollster pattern already in use; generation never happens in `open` |
| GPL-3.0 (`winfsp`/WinFSP) in an MIT repo | private fork, no distribution; recorded for upstream's future decision |
| `intel_tex_2` ISPC kernels on Windows (scope doc §4.5) | prebuilt x86_64 kernels ship in the crate; verified during first Windows build + spike (pure-Rust `SoftwareCompressor` fallback exists) |
| Case-insensitive Windows paths vs case-sensitive index keys | X-Plane reads back the exact names we enumerate; `case_preserved_names` on; lookup mismatch surfaces in e2e, fallback = case-insensitive compare in `resolve.rs` |
| `winfsp` crate is young (0.13.x, `!Send`/`Drop` subtleties) | we follow its recommended architecture (service + `new_async` + `FineGuard`), not the raw sys; Drop-order hazards are handled inside the crate |
| Windows CI flake (MSVC + tarpaulin not on Windows) | CI runs `make verify` (no tarpaulin) on Windows; coverage stays Linux-measured |

## 9. References

- [windows-scope.md](windows-scope.md) — Path A scoping, §4.3A mount model,
  §4.4 unknowns, §4.7 spike, §4.11 gate list
- [windows-backend-split.md](windows-backend-split.md) — Phase 1 design,
  §4.4 seam API, §4.6 Phase 2 preview, §5 deferrals
- `xearthlayer/src/fuse/support/core.rs` — `OrthoUnionCore`
  (`lookup`/`attrs`/`read`/`list_directory` async; `open`/`release`/`statfs`
  sync)
- `winfsp` 0.13.2+winfsp-2.1 source (registry crate, audited 2026-10-27):
  `src/filesystem/context.rs` (traits), `src/host/fshost.rs`
  (`FileSystemHost`, guard strategies), `src/host/volumeparams.rs`
  (`IrpTimeout`), `src/host/interface_async.rs` (async bridging),
  `src/service.rs` (service loop), `winfsp-sys` `build.rs` (bundled
  link inputs)
- WinFSP 2.1.25156 installed locally (`C:\Program Files (x86)\WinFsp\bin`)
