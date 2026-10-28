# XEarthLayer — Windows Support (Path A: WinFSP)

Date: 2026-09-28 (updated 2026-10-02, rebaselined 2026-10-26, reframed
2026-10-27 for solo-fork implementation, Phase 2 complete 2026-10-28)
Status: Karl is implementing this on his fork (karlrado/xearthlayer), solo,
with no intention of contributing upstream. Phase 0 complete (2026-10-27);
Phase 1 (backend split) complete (2026-10-27) — design and commit ledger in
`windows-backend-split.md` (same directory); Phase 2 (`winfsp` backend)
complete (2026-10-28) — design and commit ledger in `winfsp-backend.md`
(same directory). The scenery package installer is now unblocked on Windows
(Tier 1, 2026-10-28 — `windows-package-install.md`; overlays deferred,
Tier 2). Phase 3 (X-Plane integration / in-scene e2e) is next. See
§4.11 for the Phase 0 gate list and §4.9 (reframed) for the surviving
constraints.
Author: Hermes Agent, pairing with Karl
**Base commit:** `317637d` (develop/0.5.0, 0.5.0-rc.1, 2026-10-26). All
file:line anchors in this doc are valid at this commit — re-verify with grep
after any update. (Original base `ed1439a` v0.4.8 was used for the initial
scoping on 2026-09-28; superseded by the 0.5.0 release line.)
Scope: Make the Windows build behaviourally identical to the Linux build —
on-demand DDS synthesis at read time, no user-facing "pre-warm" step.

**Picking this up? Read in this order:** §1 (goal + governing requirement)
→ §4.8 (OS dependency rule) → §4.9 (solo-fork constraints) → §4.11
(Phase 0 gate list, 2026-10-27) → §8 (handoff state — a 2026-09-28 snapshot,
partly superseded by §4.11).

**Overall goal (stated by Karl, 2026-10-27; supersedes the 2026-09-28
upstream-PR framing):** finish Windows support on **Karl's fork**
(karlrado/xearthlayer), **solo, with no intention of contributing upstream**.
Karl expects samsoir to keep working and intends to keep using the upstream
finished project, so the fork must stay **mergeable with upstream**: the
Windows work is an **additive refactor** (extract a backend-neutral core,
keep the `fuse3` backend as a thin Linux adapter) rather than a rewrite, so
samsoir's ongoing changes to the shared core keep merging cleanly into the
fork. The upstream-PR constraints in §4.9 no longer apply as-is; what
survives is (a) **Linux stays green** and (b) **the diff stays clean and
additive** — now for mergeability, not for upstream review.

**Governing requirement (added 2026-09-28):**
The codebase must support **both** Windows and Linux from a single source
tree. OS handling has two tiers:
- **Runtime branching is allowed** for pure-Rust behaviour where neither
  branch names an OS-only symbol — e.g. `std::env::consts::OS == "windows"`
  to choose a path, a default, or a message. This is fine and preferred
  where it's simpler.
- **Conditional compilation is required** wherever compilation or linking
  would fail on the other target — i.e. any code that references
  OS-specific symbols (FUSE on Linux, WinFSP on Windows, `std::os::unix`
  syscalls, `nix`, `rlimit`). Those go behind `#[cfg(target_os = "...")]`
  or in separate compilation units, and their crates are declared under
  `[target.'cfg(...)'.dependencies]` so the other OS never compiles/links
  them.
The shared core (tile generation, cache, prefetch, job executor, DDS
encoding, path resolution, `DdsRequestor`) is OS-agnostic; OS-specific code
is isolated in platform backend modules.

---

## 1. Background

XEarthLayer creates a passthrough FUSE virtual filesystem that overlays real
X-Plane scenery files while generating DDS textures **on demand** when
X-Plane requests them. On Linux this is the `Fuse3OrthoUnionFS` FUSE3
filesystem (`xearthlayer/src/fuse/fuse3/`). X-Plane calls
`open(".../textures/N_N_BI16.dds")`; the kernel hands the read to FUSE;
FUSE calls back into XEarthLayer, which downloads 256 chunks, assembles the
4096×4096 image, encodes DDS, and streams the bytes back through the open
call. The file was a *promise*, not a file — scene load is ~30s, not the
4–5 min of pre-downloading.

The goal of the Windows port is to preserve that exact model.

---

## 2. The two candidate paths (recap)

**Path A — FUSE-equivalent via WinFSP.** Intercept file access at the
filesystem layer and synthesise DDS on read, exactly like Linux. Requires a
Windows filesystem proxy (WinFSP or Dokan). This is the mandated path.

**Path B — drop virtual FS, generate for real.** No interception layer.
Pre-generate the "immediately needed" tiles into a real `zzXEL_ortho`
directory before launch; rely on adaptive prefetch during flight.

**DISCARDED (2026-09-28, Karl's test).** X-Plane reports a missing DDS
file as a **fatal error** — an error popup, and the sim will not start.
The magenta placeholder xearthlayer serves is a *valid* DDS it synthesises
when it can't produce a real tile (timeout / network / decode failure); it
is not what X-Plane does with a truly absent file. Without read
interception there is no way to guarantee the file exists when X-Plane
opens it, so the penalty (sim shutdown) is too large. **Path B was
considered and dropped; Path A (WinFSP) is the only path.**

---

## 3. Why Path B was discarded

Path B's workflow:
1. **Before launch** — the user tells XEarthLayer where they're flying and
   waits for tiles to be generated as real files. This step does not exist
   on Linux.
2. **During flight** — adaptive prefetch (`prefetch/`) generates upcoming
   regions in the background as real files.

The structural limitation is decisive: without an interception layer, a DDS
read for a file not on disk returns `ERROR_FILE_NOT_FOUND` synchronously —
there is no callback to "wait, let me generate it." **Karl's test
(2026-09-28) confirmed the penalty:** X-Plane reports a missing ortho DDS
in an error popup and **will not start the sim**. The magenta placeholder
xearthlayer serves is a *valid DDS it synthesises* on generation failure
(timeout, network, decode) — not X-Plane's response to an absent file; the
two only look similar. So Path B can never guarantee the file exists at
read time, and any outrun-prefetch moment (in-session flight restart at a
new location, the `teleport` command at up to 16× ground speed, a very fast
aircraft at cruise) is a hard stop, not a visual degradation.

**Therefore Path B was considered and discarded — the missing-DDS penalty
(X-Plane shutdown) is too large.** The FUSE/WinFSP interception layer is
what makes the app work, not an implementation detail. **Path A (WinFSP) is
the only path.**

---

## 4. Path A scoping findings

### 4.1 Correction: "WinFSP supports FUSE3"

True, but it is two distinct layers, and the distinction decides the
architecture:

- **The WinFSP "FUSE compat layer"** (`fuse.h`) is a **C-level** FUSE 3.2
  shim. Correct for porting C FUSE filesystems; the *wrong* layer here
  (we'd be doing C FFI against a C API).
- **The idiomatic `winfsp` Rust crate** (over `winfsp-sys`) exposes a
  **Rust trait** analogous to the fuse3 `Filesystem` trait we already
  implement. Its `FileSystemHost` supports **`async` implementations of
  `read`, `write`, and `read_directory`.**

So the async model the codebase is built on maps onto WinFSP in Rust. This
is a re-expression, not a rewrite.

Prior art: the `zero-sfs-mount` crate does exactly the split recommended
here — a `FsAdapter` core with a `fuse` feature (libfuse) and a `winfsp`
feature (WinFSP).

### 4.2 What makes this tractable

1. **The FUSE surface here is read-only.** `impl Filesystem for
   Fuse3OrthoUnionFS` (`ortho_union_fs.rs:657`) implements: `init`,
   `destroy`, `lookup`, `getattr`, `read`, `readdir`, `readdirplus`,
   `open`, `release`, `opendir`, `access`, `flush`, `fsync`, `statfs`.
   **No** `create`/`mknod`/`mkdir`/`unlink`/`rmdir`/`rename`/`symlink`/
   `setxattr`. That removes the entire Windows mutation / ACL /
   security-descriptor / reparse-point complexity. It is a virtual *read*
   filesystem — the easy half of WinFSP.

2. **The on-demand generate path is already portable.** `read`
   (`ortho_union_fs.rs:835`) is the block-and-generate model: per-open
   handle memoisation, `await resolve_tile(...)`, zero-copy window slice.
   The `await` is the whole point and survives the port directly because
   WinFSP async `read` exists.

3. **Licensing is workable.** This repo is **MIT** (`LICENSE` — "MIT
   License, Copyright (c) 2025 XEarthLayer Contributors"). WinFSP is
   **GPLv3 with a FLOSS exception** plus a $2400/3yr commercial option. The
   FLOSS exception permits free use/distribution *by a free-software
   project*; an MIT project qualifies. WinFSP ships as a **signed kernel
   driver the user installs separately**; we do not bundle or re-sign it.
   No licensing blocker. (Commercial license only if the project goes
   proprietary or we bundle the driver.)

4. **Everything downstream is already cross-platform.** Download
   orchestrator, three-tier cache, prefetch, job executor, ISPC/wgpu
   compressor, `tasks/` — none are FUSE-bound. The port is concentrated in
   the FUSE layer + a handful of Unix primitives.

### 4.3 What to actually touch

**A. Backend trait split — the core move.** Extract the union-FS *logic*
(path resolution, `DdsRequestor`, tile resolution, real-file passthrough,
placeholder generation) into a backend-neutral core that both a `fuse3`
backend and a `winfsp` backend call into. `DdsRequestor`/`FileAttrBuilder`
are already abstract (SOLID), so most of the core is reusable.
FUSE-specific and must be re-expressed per backend:
- The `impl Filesystem` ops above (14 of them).
- **The inode model:** `InodeManager` (`inode.rs`) uses numeric FUSE ino
  numbers. WinFSP identifies files by a **file-context object**, not a
  number. This is the biggest conceptual lift — the inode model must be
  re-mapped onto WinFSP's context model, and the `is_virtual_inode` /
  `get_virtual_dds` / `get_path` lookups re-keyed.
- **`readdir`/`readdirplus`:** WinFSP's `ReadDirectory` is a batched,
  offset-based model (`FSP_FSCTL_DIR_INFO`), semantically different from
  FUSE's `readdir`+`readdirplus`. Needs real rework, not a copy.
- **Error/attr types:** `Errno::from(libc::ENOENT)` → `winfsp::FspError`;
  `std::os::unix::fs::MetadataExt`, `libc::getuid()/getgid()`
  (`shared.rs:169,170,190,191`) → WinFSP equivalents or dropped (the compat
  layer's `-o uid=-1` analog presents everything as the launching user —
  all X-Plane needs).

**B. Windows plumbing (mostly cfg-gated removals, small additions).**
- `xearthlayer-cli/src/preflight/system.rs:128` `FdLimit::remediate()` (rlimit
  `NOFILE.set(hard, hard)`; part of the 0.5.0 preflight framework, #265 —
  no longer in `run.rs`) → cfg out the `FdLimit` check on Windows.
- `manager/local.rs:267` `/proc/mounts` detection → query WinFSP / the
  volume instead.
- `manager/mounts.rs`: `max_background` / `congestion_threshold` threading
  (Linux-FUSE throttling) → drop; WinFSP has its own queue. `unprivileged
  mount` / `spawn_mount` → replace with a WinFSP mount (drive letter or a
  folder under `Custom Scenery`).
- `manager/symlinks.rs:17` `std::os::unix::fs::symlink` → Windows reparse
  points / `symlink_dir` (Developer Mode or the service context). This is
  the *overlay* static-package path, separate from the ortho mount.
- `xplane/paths.rs` already has a Windows branch — no work.
- `metrics/memory_probe.rs` glibc code already cfg-gated to `linux+gnu` —
  no work.

**C. Process model + packaging.**
- **Resolved:** run as a **foreground user-mode process**, same as today's
  `xearthlayer run`. The WinFSP mount is registered by the process and
  released on exit — the `manager/mounts.rs` lifecycle maps 1:1, only the
  mount target changes to a WinFSP volume. No Windows Service, no SCM, no
  console-less logging (a later "nice to have" if ever wanted).
- **Installer:** WinFSP is a signed driver the user must have installed (a
  prereq, as with FUSE on Linux). We need a bootstrap that detects it and
  prompts to install if missing (or an MSI that bundles the *installer*, not
  the binary). Main new UX surface vs. Linux FUSE.

### 4.4 Unknowns (updated)

**Resolved by the user (2026-09-28):**
- **Unknown 1 — X-Plane DDS read model:** *X-Plane does a simple
  `ReadFile`; it does NOT use mmap.* Consequences:
  - The FUSE-compat-layer risk is gone (mmap was its main weakness).
  - The idiomatic `winfsp` Rust path with `async read` is fully sufficient;
    no need to drop to the native `FSP_FILE_SYSTEM_INTERFACE` C API.
  - The read path is the well-supported half of WinFSP. `ortho_union_fs.rs:835`'s
    block-and-generate `await resolve_tile(...)` ports almost verbatim.

**Still open:**
- **#2 — transaction timeout during first-scene generation.** With a
  synchronous `ReadFile`, the kernel is sitting in a `Read` transaction
  while we generate. On Linux the app itself caps the wait: each `read`
  blocks **at most ~10s** (the per-tile generation budget,
  `config/settings.rs:103-106` — "If exceeded, returns a magenta
  placeholder") and then serves a **valid magenta placeholder DDS** — a
  slow-but-answered read, never an unbounded hang, and the exact pattern
  X-Plane already tolerates (standard "missing scenery" look). On WinFSP,
  the question is whether WinFSP's own read-transaction ceiling sits **above
  or below that 10s**: if below, `ReadFile` could error *before* our
  placeholder is served, so we'd need to either shorten the per-tile budget
  or raise WinFSP's timeout. To verify: WinFSP's read-transaction timeout
  value + a blocked-read observation (see §4.7 step 1).
- **#3 — `readdir` batching** vs. X-Plane's `textures/` scan. Rework, not
  risk.
- **#4 — the overlay/symlink path** (`yzXEL_overlay`) on Windows. Unchanged.

### 4.5 Risk profile

The "can we even do this on WinFSP" question is now a **yes**. Remaining
questions are "will it behave well under the real timing and enumeration
X-Plane imposes" — testable, not architectural.

Top risk: **#2 read-transaction timeout vs. the 10s per-tile budget** —
mostly a config question (WinFSP's ceiling), with a fallback of serving the
magenta placeholder earlier.
Second risk: the inode→context re-mapping (§4.3A) — the one non-mechanical
lift.

**ISPC compressor on Windows (tracked).** `intel_tex_2` v0.5 ships prebuilt
Windows x86_64 kernels — no ISPC compiler needed at build time
(`compressor.rs:151-152`, `build.rs` has no Windows-specific linkage).
`SoftwareCompressor` (pure Rust) is the fallback if the ISPC path fails.
Verify on the Windows box: does `intel_tex_2` load and produce correct BC1/BC3
output on the target CPU? (One `cargo build` + a quick encode test will
confirm.)

### 4.6 Effort & risk estimate

Assuming a **Windows box with X-Plane 12 is available for testing**
(untestable on the Linux host):
- Backend split + winfsp backend (A): **~2–3 weeks** — core exists; cost is
  inode→context re-mapping + `readdir` rework.
- Windows plumbing (B): **~1 week.**
- Service model + installer/packaging (C): **~1 week.**
- Windows X-Plane integration testing (unknowns 2–4): **~1 week.**
- **Total: ~5–8 weeks to a first working Windows build.** Dominant risk:
  the WinFSP read-transaction ceiling vs. our 10s per-tile budget and the
  inode→context model.

### 4.7 Recommended next steps

1. **1–2 day spike (cheap, decisive):** a minimal `winfsp` Rust mount
   serving a file; in the `Read` callback, block for a configurable time
   (e.g. 5s, then 10s — our per-tile budget) and observe whether
   `ReadFile` succeeds slowly or returns an error. Purpose: establish
   WinFSP's read-transaction ceiling and confirm the per-tile 10s +
   magenta-placeholder pattern (§4.4 #2) works as on Linux.
2. **Backend-split design doc:** go through `ortho_union_fs.rs` (2933
   lines) and `inode.rs`; tag every method as *portable core* /
   *fuse3-bound* / *needs WinFSP re-expression*. Grounds the estimate in
   the actual code and isolates the inode→context re-mapping work.

### 4.8 OS dependency handling (governing requirement)

The dual-platform requirement (§1) means the codebase compiles to two
distinct binaries from one source tree. OS handling splits on the
**compile/link boundary**:

1. **OS-specific symbols → conditional compilation (hard rule).** Any code
   that references an OS-only symbol must be gated so it only compiles on
   the matching target: FUSE calls → Linux; WinFSP calls → Windows;
   `std::os::unix` / `nix` / `rlimit` → Linux. Use
   `#[cfg(target_os = "...")]` or **separate compilation units** (a `fuse3/`
   module vs a `winfsp/` module). The compiler cannot link a symbol that
   doesn't exist on the target, so this is not optional.
2. **Pure-Rust behaviour → runtime branching is allowed.** Where neither
   branch names an OS-only symbol, a runtime check is fine and preferred
   for readability — e.g. `std::env::consts::OS == "windows"` to pick a
   default path, a config key, or a log message. Don't force these through
   compile-time gates when a runtime branch is simpler.
   - **Foot-gun:** `cfg!(windows)` inside a runtime `if` still *compiles
     both* branches (it const-folds the condition, not the code). So an
     OS-only symbol must never appear in a `cfg!`/runtime branch expecting
     it to be stripped — it must be behind `#[cfg]` or in a separate unit.
3. **Cargo target-specific dependencies.** Crates that pull in OS-specific
   symbols (`fuse3`, `winfsp`, `nix`, `rlimit`) are declared under
   `[target.'cfg(...)'.dependencies]` so the other OS never compiles or
   links them. Pure-Rust crates stay in the main `[dependencies]`.
4. **One source tree, no forks.** Both platforms build from the same
   repository; the difference is the target triple
   (`x86_64-pc-windows-msvc` vs. `x86_64-unknown-linux-gnu`) plus the
   cfg-gated backend.

#### 4.8.1 Dependency matrix

Grounded in the actual manifests (`xearthlayer/Cargo.toml`,
`xearthlayer-cli/Cargo.toml`). "Move to target-specific" means the crate
stays in use on one OS but must be declared under a
`[target.'cfg(...)'.dependencies]` section so the other OS's build does not
compile it.

| Crate / module | Linux | Windows | Notes |
|---|---|---|---|
| `fuse3` (lib) | ✅ required | ❌ absent | Fork of fuse3 (`samsoir/fuse3`, `configurable-background` branch, `tokio-runtime` + `unprivileged` features). FUSE kernel module; Linux-only. Move to `[target.'cfg(target_os = "linux")'.dependencies]`. |
| `winfsp` + `winfsp-sys` | ❌ absent | ✅ required | New dependency. WinFSP user-mode API + Rust bindings. `[target.'cfg(target_os = "windows")'.dependencies]`. |
| `rlimit` (CLI) | ✅ required | ❌ absent | `FdLimit::remediate` at `xearthlayer-cli/src/preflight/system.rs:128` (`rlimit::Resource::NOFILE.set`), part of the 0.5.0 preflight framework (#265) — no longer in `run.rs`. Cfg out the `FdLimit` check and the `rlimit` crate on Windows — WinFSP has its own connection model. |
| `nix` (lib, `fs` feature) | ✅ used | ❌ absent | `system/filesystem.rs:21` (`nix::sys::statvfs::statvfs`) for `statfs`-style volume queries. Windows: `GetDiskFreeSpaceEx` / WinFSP volume query. Move to Linux target. |
| `libc` (lib) | ✅ used | partially | `getuid`/`getgid`/`MetadataExt` in `fuse/fuse3/shared.rs:16` → cfg out on Windows; WinFSP presents files as the launching user. `libc` itself is cross-platform (MSVC target exists) so the crate can stay; only the Unix syscalls it wraps are gated. |
| `intel_tex_2` (ISPC) | ✅ | ✅ | v0.5 ships prebuilt x86_64 Windows kernels — no ISPC compiler needed at build time on either platform. `compressor.rs:151-152`. Verify the Windows DLL loads on the target CPU. |
| `wgpu` + `block_compression` | ✅ | ✅ | Already cross-platform (Vulkan / DX12 backends). |
| `moka` (memory cache) | ✅ | ✅ | Pure Rust, no OS dependency. |
| `reqwest` (HTTP, rustls) | ✅ | ✅ | Cross-platform. |
| `tokio`, `tokio-util`, `tokio-tungstenite` | ✅ | ✅ | Cross-platform. |
| `image`, `bytes`, `tracing`, `dashmap`, `rayon`, `parking_lot`, `futures`, `serde`, `bincode`, `flate2`, `glob`, `sha2`, `chrono`, `dirs`, `regex`, `thiserror`, `rust-ini`, `semver`, `staticmap`, `tiny-skia`, `pollster` | ✅ | ✅ | All pure Rust / cross-platform. No action. |
| glibc `mallopt` calls | ✅ (`linux-gnu`) | ❌ | Already cfg-gated in `metrics/memory_probe.rs` and the allocator config (`configure_allocator`). |
| `/proc/mounts` parsing | ✅ | ❌ | `manager/local.rs:267`. Windows: query the WinFSP volume / `GetVolumePathName`. |
| `std::os::unix::fs::symlink` | ✅ | ❌ | `manager/symlinks.rs:17`. Windows: reparse points or `symlink_dir` (Developer Mode). |
| `std::os::unix::fs::MetadataExt` | ✅ | ❌ | `fuse/fuse3/shared.rs:16`, `system/storage_detect.rs:33`. Cfg out on Windows. |
| `[fuse]` config section | ✅ | ❌ | `max_background` / `congestion_threshold` are Linux FUSE kernel params. Cfg-gate the config parser to ignore or reject these keys on Windows. |

#### 4.8.2 Code-level cfg structure (target architecture)

```
xearthlayer/src/
  fuse/
    mod.rs            # re-exports (shared)
    shared.rs         # DdsRequestor, FileAttrBuilder, placeholder — OS-agnostic
    placeholder.rs    # magenta placeholder generation — OS-agnostic
    filename.rs       # DDS filename parsing — OS-agnostic
    inode.rs          # InodeManager — needs trait split (§4.3A)
    fuse3/            # Linux backend — #[cfg(target_os = "linux")]
      ortho_union_fs.rs
      shared.rs       (FUSE-specific impls of the shared traits)
    winfsp/           # Windows backend — #[cfg(target_os = "windows")]
      ortho_union_fs.rs   (WinFSP Filesystem trait impl)
      shared.rs       (WinFSP-specific impls of the shared traits)
  manager/
    local.rs          # /proc/mounts (linux) / volume query (windows) — cfg-split
    mounts.rs         # FUSE mount params (linux) / WinFSP mount (windows) — cfg-split
    symlinks.rs       # unix symlink (linux) / reparse point (windows) — cfg-split
  metrics/
    memory_probe.rs   # glibc arena telemetry — already cfg-gated to linux-gnu
```

The trait split in §4.3A (backend-neutral core + per-OS backend) is the
mechanism that makes this work: `DdsRequestor`, `FileAttrBuilder`, and the
tile-resolution logic live in the OS-agnostic core; the `fuse3` and `winfsp`
backends each provide their own implementation of the 14 FUSE ops (Linux)
or the equivalent WinFSP `Filesystem` trait (Windows) against that core.

#### 4.8.3 What this does and does NOT require

**Required (the compile/link boundary):**
- OS-specific *symbols* (FUSE, WinFSP, `nix`, `rlimit`, `std::os::unix`)
  are gated with `#[cfg(target_os)]` or split into separate compilation
  units, and their crates are target-gated in Cargo.toml. The compiler
  cannot link a symbol that doesn't exist on the target — this is the hard
  rule.

**Not required:**
- **Runtime branching is fine** for pure-Rust behaviour — `if os ==
  "windows"` choosing a path/default/message is allowed and preferred where
  simpler than a compile-time gate.
- **No cross-compilation from Linux to Windows** for the initial build —
  a Windows dev box building natively is the expected workflow.
- **No MSYS2 / MinGW** — `winfsp` targets MSVC (`x86_64-pc-windows-msvc`).
- **No bundling the WinFSP driver** — it is a signed driver the user
  installs separately (same model as FUSE on Linux). We only link against
  its user-mode API.

### 4.9 Solo-fork constraints (overall goal; supersedes the upstream-PR framing)

Reframed 2026-10-27: this is now work on Karl's fork, not a PR against
samsoir. Of the five PR-ability constraints below, three drop away and two
survive, repurposed.

**Dropped (no longer a PR, so no upstream gate):**
- **Windows CI for upstream.** Karl builds and tests on his own Windows box
  (see §4.11 environment). A reproducible local build/test recipe is still
  worth having, but there is no upstream CI job to satisfy.
- **WinFSP licensing sign-off by a maintainer.** Karl decides this himself.
  The "MIT project qualifies for the FLOSS exception" argument (§4.2) still
  holds and should be noted in the fork's docs; the `winfsp` dep stays
  target-gated so a Linux build never pulls it.
- **"Clean diff against `main`" as a merge requirement.** There is no merge
  to upstream. (See the surviving constraint below for why a clean diff is
  *still* wanted.)

**Surviving (repurposed):**
1. **Linux stays green.** Non-negotiable: the existing Linux suite
   (`make pre-commit`: fmt + clippy + tests, ≥80% coverage) must still pass.
   Target-gated deps and `#[cfg]` splits keep the Windows path out of the
   Linux build entirely — a Windows addition must be zero-cost to Linux.
2. **Keep the diff clean and additive — now for mergeability, not review.**
   Karl intends to keep using samsoir's finished project and to pull his
   ongoing work into the fork. A clean, additive diff (extract a neutral
   core; keep `fuse3` a thin adapter; don't rewrite shared code) is what
   keeps upstream changes mergeable into the fork. This replaces the old
   "no fork divergence" constraint — we *want* a fork, but we don't want it
   to fork *away* from upstream.

**Phase structure (replaces the 3-PR plan).** Karl controls the release; the
work decomposes into independently-shippable phases, each leaving a working
state:
- **Phase 0 — environment + gate list.** Done 2026-10-27 (§4.11).
- **Phase 1 — backend trait split.** Extract the backend-neutral core from
  `ortho_union_fs.rs` + `inode.rs`; the `fuse3` backend now calls into it.
  Linux-only, no behaviour change, no new deps. De-risks Phase 2 and keeps
  the fork mergeable. **Done 2026-10-27:** `OrthoUnionCore` + neutral types
  in `fuse/support/`, `Fuse3OrthoUnionFS` reduced to a thin adapter, and
  the behavioural op tests ported to the core level; commit ledger in
  `windows-backend-split.md` §6.
- **Phase 2 — `winfsp` backend.** Add the Windows backend against the core
  with `winfsp`/`winfsp-sys` under `[target.'cfg(target_os = "windows")']`;
  move `fuse3`/`nix`/`rlimit` under the Linux target. This is the phase that
  "adds Windows." Exit: a `xearthlayer.exe` that mounts a WinFSP volume and
  serves ortho + night-lit on demand. **Done 2026-10-28:** the WinFSP backend
  (`fuse/winfsp/`) is built and wired into the production mount flow
  (`manager/mounts.rs`), green on Linux + macOS + Windows, and spike-verified
  on the Windows box; commit ledger in `winfsp-backend.md` §6. The scenery
  package installer is now unblocked on Windows too (2026-10-28, Tier 1 —
  `windows-package-install.md`): ortho packages download, extract and install
  on the same queue as Linux and macOS, so there is real content to mount.
  Overlay packages are deferred (Tier 2 — the symlink path). Note: the
  workspace `[profile.dev]` now sets `overflow-checks = false` (2026-10-28)
  because zlib's 32-bit `total_in`/`total_out` counters wrap by design and a
  real ~12 GB package's compressed stream passes the 4 GiB wrap point, so
  default debug traps panicked a debug `packages install` mid-extract while
  release completed the same package (see the note in the root `Cargo.toml`).
  The in-scene e2e
  (X-Plane loading ortho off the mount) is the remaining Phase 3 item.
- **Phase 3 — X-Plane integration.** Wire into X-Plane 12 on the Windows box;
  confirm the 10s per-tile budget + magenta placeholder under WinFSP; verify
  ISPC/wgpu under real load, dashboard/TUI, prefetch, providers, package
  manager.
- **Phase 4 — packaging/polish (optional).** WinFSP detect-and-prompt
  bootstrap, a reproducible Windows build recipe, Windows section in docs.

This ordering gets a working on-demand ortho mount in Karl's hands by the
end of Phase 2, front-loading the two real risks (WinFSP read-transaction
ceiling, inode→context re-mapping).

### 4.10 Decision inputs (added 2026-10-02)

Karl is deciding whether to proceed at all. Recorded here so the tradeoffs
are on the record, independent of the port mechanics.

**The ZL17 gap is real, verified, and the port does NOT close it.**
AutoOrtho generates ZL17 detail even when the `.ter` file asks for ZL16: it
downloads ZL17 satellite chunks and assembles a DDS *named* as ZL16 whose
base layer is 8192×8192 — the same geographic area that takes 16×16 ZL16
chunks (4096×4096) instead takes 32×32 ZL17 chunks (4× the count, each 4×
smaller). X-Plane uses the extra layer if present, even though the DSF/`.ter`
references ZL16. xearthlayer does **not** do this:

- **Code (2026-10-02, verified):** the FUSE read path is purely
  filename-driven — zoom passes through from the requested `.dds` name
  (`fuse/filename.rs`, `fuse/fuse3/shared.rs:249`
  `chunk_to_tile_coords`, `tasks/download_chunks.rs:323`) with no zoom
  selection logic and no config hook.
- **Config (2026-10-02, verified):** no zoom key exists anywhere in
  `ConfigKey` (`config/keys.rs`) nor in the live `~/.xearthlayer/config.ini`.
  User-facing docs mention zoom only in the *publisher* context (deduping
  ZL16/ZL18 overlaps in Ortho4XP output, `docs/content-publishing.md`).
  Contrast: AO exposes user controls for ZL, including ZL17 for
  non-airport areas.
- **Cache forensics (2026-10-02, Karl's real flight, CO region):**
  `$HOME/.xearthlayer/cache` contains 886,272 chunk JPEGs —
  **822,528 `chunk_12_*` (ZL16) + 63,744 `chunk_14_*` (ZL18)** — and
  3,210 DDS tiles (2,978 `tile_12_*` ZL16 + 232 `tile_14_*` ZL18),
  **every one** 4096×4096 with 13 mips (parsed from the actual DDS
  headers; in xearthlayer's model the tile pixel size is fixed at
  4096×4096 = 16×16 chunks — zoom only changes the geographic area
  covered). Zero `chunk_13_*`, zero `tile_13_*`, zero 8192×8192 files.
  Two facts matter: (1) the genuine ZL18 content (presumably from
  airport-area Ortho4XP/AO packages whose `.ter` files ask for ZL18)
  proves zoom is entirely pack-driven — xearthlayer serves whatever the
  `.ter` requests; (2) the absence of 8192×8192 base layers proves the
  ZL16-named files are not secretly higher-resolution, i.e. no AO-style
  ZL17 trick. (Note: chunk JPEGs are 256×256 at *every* zoom — tile
  servers always serve 256×256 — so JPEG pixel size is not a ZL
  discriminator; the signature of the AO trick is the assembled base
  layer size, which here is uniformly 4096×4096.)

**Implication for the decision:** the Windows port delivers xearthlayer's
strengths (on-demand synthesis, no pre-download, clean architecture, the
30s scene-load model) but **not** AO's ZL17 bonus. If ZL17 detail is a
reason to prefer AO on the Windows box, that gap survives the port and
would be a separate feature: 4× chunks per tile, 8192×8192 assembly and
encoding, plus a config surface that does not exist today (which zooms,
which areas, which provider). AO has other issues (Karl: "don't make it an
obvious choice"), so this is a genuine tradeoff, not a slam dunk either way.

**Effort & exit ramps (unchanged from §4.6):** ~5–8 weeks to a first
working Windows build, gated on two *testable* risks — the WinFSP
read-transaction ceiling vs. the 10s per-tile budget (§4.4 #2) and the
inode→context re-mapping (§4.3A). The cheapest kill-switch remains the
1–2 day blocked-`Read` spike (§4.7 step 1): if WinFSP's ceiling sits below
the 10s budget and can't be raised, the core model degrades and the effort
question changes.

**Rebaselined to `317637d` (0.5.0-rc.1) on 2026-10-26 — two inputs change:**

1. **`paths/` is now the platform-dispatch layer, and it's the exact shape
   the port needs.** The 0.5.0 directory-layout work (#193) introduced a
   `BaseDirectories` trait (`xearthlayer/src/paths/base.rs`) with
   `XdgDirectories` (Linux) and `AppleDirectories` (macOS) impls selected at
   runtime in `paths/mod.rs:38` `active()`, plus a legacy-migration path
   (`paths/legacy.rs`) and a test seam (`paths/testing.rs`). Windows work
   here shrinks from "build the layer" to "add a `WindowsDirectories` impl
   + a third arm in `active()`". Every call site already goes through the
   trait. **Scope reduction vs. the original scoping.**

2. **The FUSE surface now carries a second virtual tree.** Night lighting
   (#302) serves generated `_LIT.dds` textures through the same FUSE mount
   on a separate `night/` path. The WinFSP backend therefore has to expose
   *two* virtual trees (ortho + night-lit), not one — the inode→context
   re-mapping in `ortho_union_fs.rs` (now 3,221 lines, was ~2,900) spans
   both. **Small scope increase vs. the original scoping.**

   The net effect on the 5–8 week estimate is neutral. The top risks
   (WinFSP read-transaction ceiling, inode→context re-mapping) and the
   delivery plan are unchanged (the 3-PR shape named here was reframed to
   the phase structure in §4.9 on 2026-10-27). The `rlimit` plumbing anchor moved from
   `run.rs:387` to `preflight/system.rs:128` as part of the 0.5.0
   preflight framework (#265); the doc's other file:line anchors remain
   valid at `317637d`.

**Status (2026-10-28):** Karl IS implementing the port, on his fork
(karlrado/xearthlayer), solo, not contributing upstream (see reframed §1
goal and §4.9). Phase 0 is complete — environment ready and the full
OS-specific gate list established (§4.11). Phase 1 (backend trait split)
is complete (2026-10-27); Phase 2 (the `winfsp` backend) is complete
(2026-10-28) — commit ledger in `winfsp-backend.md` §6; Phase 3 (X-Plane
integration / in-scene e2e) is next. The ZL17 tradeoff, the effort estimate,
and the §4.4 risk list stand as before.

### 4.11 Phase 0 findings (2026-10-27)

**Environment (all present, verified on the Windows box):**

| Need | State |
|---|---|
| Fork (`karlrado/xearthlayer`) | ✅ `origin`; clean tree; samsoir not a remote |
| Rust 1.97.1 + VS 2022 Build Tools (MSVC linker 14.44) | ✅ present |
| X-Plane 12 (integration target) | ✅ `C:\X-Plane 12`, `C:\X-Plane 12 (12.2)` |
| WinFSP 2025 v2.1.25156 | ✅ service `WinFsp.Launcher` running; `winfsp-x64.dll` + `winfsp-x64.sys` under `Program Files (x86)\WinFsp\bin` |

**Confirmed by the compiler** (`cargo check --target x86_64-pc-windows-msvc --workspace`): `fuse3` (the `samsoir/fuse3` git fork) is **hard-Linux-only** — 14 errors (`std::os::fd`, `std::os::unix`, `nix::mount`, `libc::S_IF*`). It is the single root dependency that blocks the Windows build. Cargo stops there, so the rest of the gate list below comes from a full grep of the source (not masked by the early stop).

**Gate list — dependencies (move to target-specific sections):**

| Crate (manifest) | Why | Action |
|---|---|---|
| `fuse3` (xearthlayer) | FUSE, hard-Linux (14 errors) | → `[target.'cfg(target_os = "linux")']` |
| `nix` (xearthlayer) | `statvfs`/`flock`/`errno`, Unix-only | → Linux target |
| `rlimit` (xearthlayer-cli) | `NOFILE` | → Linux target |
| `libc` | cross-platform crate | **keep**; gate only the Unix-syscall *usages* |
| `memory-stats` (xearthlayer) | process RSS (cross-platform crate) | likely OK — **verify** in Phase 1 |
| `crossterm` `use-dev-tty` (xearthlayer-cli) | Unix event source | likely no-op on Win — **verify** |

**Gate list — our production code (gate or rework):**

| Location | What | Windows action |
|---|---|---|
| `fuse/fuse3/*` | the whole FUSE backend | replaced by a `winfsp/` backend (Phase 1–2) |
| `manager/queue/lock.rs:13` | `nix::fcntl::Flock` (install-queue lock) | **NEW FINDING** — `flock` has no Win equivalent → `fs2` / `LockFileEx` |
| `system/filesystem.rs:21` | `nix::statvfs` (free-space) | `GetDiskFreeSpaceEx` / volume query |
| `manager/installer.rs:192` | `MetadataExt` device + free bytes (DiskLedger) | volume query |
| `manager/symlinks.rs:17` | `std::os::unix::fs::symlink` (overlay) | reparse point / `symlink_dir` |
| `manager/mounts.rs` + `system/unmount.rs:80` | FUSE mount mgr, `/proc/mounts` | WinFSP mount + volume query |
| `preflight/system.rs` (cli) | `rlimit` FdLimit check | cfg-out on Windows |
| `system/hardware.rs`, `diagnostics/report.rs` | `/proc/meminfo`, `/proc/cpuinfo` | `GlobalMemoryStatusEx` / registry (or fallback) |

**Already handled (cfg-gated, no work):** `system/storage_detect.rs` and `metrics/memory_probe.rs` (both `#[cfg(linux)]` + fallback); the `#[cfg(unix)]` permission/symlink tests in `night/*` and `manager/*` (skip cleanly on Windows).

**`intel_tex_2` / `wgpu`:** `wgpu` is cross-platform (DX12); `intel_tex_2` ships Windows ISPC kernels. Both confirmed compiling in Phase 1 (first real Windows build of `xearthlayer`) and functionally under X-Plane in Phase 3.

**New finding the original scoping missed:** `manager/queue/lock.rs` uses `nix::fcntl::Flock` for the install-queue's cross-process lock. It is in the **package-manager** path, not the core ortho path, so it does **not** block "on-demand ortho first" — but it is a genuine porting item (no Windows `flock`), landed in Phase 2/3.

**Phase 0 exit criteria:** met (environment ready + complete gate list). The two "verify" items (`memory-stats`, `crossterm use-dev-tty`) fold into Phase 1.

---

## 5. Path B — considered and discarded

Path B (non-FUSE mode) was considered as an opt-in alternative that would
drop the virtual-filesystem mount, pre-warm the starting area into a real
`zzXEL_ortho` directory, and rely on adaptive prefetch for everything
after. It was **discarded (2026-09-28)** because the penalty for a missing
DDS is too large.

**The finding that closed it (Karl's test, 2026-09-28):** delete one ortho
DDS from the scenery tree and launch X-Plane — **X-Plane reports it in an
error popup and will not start the sim.** A missing ortho DDS is a **fatal
error**, not a tolerated missing texture. This corrects an earlier
inference in this discussion: the magenta placeholder xearthlayer serves
(`fuse/placeholder.rs`) is a **valid DDS the app synthesises** when it
cannot produce a real tile (per-tile 10s timeout at
`config/settings.rs:103-106`, network/decode failure) — it is the app's
error path, not X-Plane's. X-Plane never sees an absent file on Linux
*because* FUSE intercepts the read and always returns bytes, real or
placeholder. The two mechanisms only look similar; the distinction is what
makes the FUSE layer mandatory.

Without read interception, any moment where prefetch loses the race
(in-session flight restart at a new location, the `teleport` command at up
to 16× ground speed, a very fast aircraft at cruise) is a hard stop — the
sim refuses to run — not a visual degradation. That penalty is too large to
accept.

**Conclusion:** Path B was considered and discarded. **Path A (WinFSP) is
the only path for the Windows port.**

---

## 6. Transcript of the discussion

Note: user messages are verbatim. Assistant responses are faithful
summaries that preserve the substantive conclusions and reasoning (the
full responses are lengthy; the summaries below keep every decision and
every cited file/line).

---

**USER:** What would it take to make this app run on Windows?

**ASSISTANT (summary):** Grounded the answer in a code investigation rather
than guessing. Findings:
- FUSE is the single point of Linux coupling. `Fuse3OrthoUnionFS`
  (`xearthlayer/src/fuse/fuse3/`) is an async multi-threaded FUSE filesystem
  that overlays real scenery and generates DDS on read.
- Linux coupling is concentrated in: the `fuse3` crate + `libfuse3-3`
  (`xearthlayer-cli/Cargo.toml:28`); `max_background`/`congestion_threshold`
  threading (`fuse/fuse3/shared.rs`, `manager/mounts.rs`); `/proc/mounts`
  (`manager/local.rs:267`); Unix primitives in the FUSE layer
  (`libc::getuid()/getgid()`, `std::os::unix::fs::MetadataExt`, unprivileged
  mount, `spawn_mount`); the ~3000-line `impl Filesystem` in
  `ortho_union_fs.rs`; `std::os::unix::fs::symlink` (`manager/symlinks.rs`);
  and `rlimit` NOFILE (`xearthlayer-cli/src/commands/run.rs:386`, as of `317637d`).
- Already portable: `xplane/paths.rs` has a Windows branch; the glibc memory
  code (`metrics/memory_probe.rs`) is cfg-gated to `linux+gnu`; the download
  orchestrator, cache tiers, prefetch, job executor, ISPC/wgpu compressor are
  cross-platform.
- Two paths: **Path A** — WinFSP/Dokan FUSE-equivalent (preserves on-demand
  30s model; multi-week; WinFSP's model is structurally different from async
  FUSE). **Path B** — drop virtual FS, pre-generate real files (few days;
  loses the generate-on-demand model; requires pre-warming).

---

**USER:** For Path B, does it require that the user pre-generate the tiles
before starting X-Plane?

**ASSISTANT (summary):** Yes for the starting scene, no for everything
after. X-Plane loads a region's scenery and its DDS tiles on entering the
area. A DDS read for a file not on disk fails immediately
(`ERROR_FILE_NOT_FOUND`); without an interception layer there is no callback
to say "wait, let me generate it." So the starting region must be pre-warmed.
Post-launch, adaptive prefetch (`prefetch/`, `PrefetchBox`, `GroundStrategy`,
`ExtentCalculator`) generates tiles in the background as real files, and
X-Plane picks them up on the next scene load. Practical shape: "pre-warm the
area you're about to fly, then let prefetch keep up." Limitation: cannot fly
to a fresh location and have it appear on demand.

---

**USER:** The requirement is to have the Windows port behave the same as the
Linux implementation. The Windows user should not have to "Pre-warm" the
area. Do you agree that Path B is off the table with this requirement?

**ASSISTANT (summary):** Agreed. The requirement isn't just a constraint Path
B happens to fail; it's something Path B is defined by lacking. The entire
Linux feature is "a file that doesn't exist on disk can still be served,
because something intercepts the read and synthesizes it" — that interception
is the feature. Path B is defined by removing that interception. A
watch-and-race approach can't work because Windows returns
`ERROR_FILE_NOT_FOUND` synchronously on open — there is no user-mode hook to
pause the open. So any Windows build matching the Linux on-demand behavior
must intercept at the filesystem layer — a kernel minifilter via WinFSP or
Dokan. That is exactly Path A. There is no third option. Flagged the
consequences: a kernel FS filter is a driver the end user must install
(driver signing, SmartScreen friction), and WinFSP/Dokan redistribution is a
licensing decision, not a free one. Recommendation: commit to Path A via
WinFSP; first step is splitting the union-FS core from the FUSE trait.

---

**USER:** I still don't totally understand the user workflow for Path B.
Please explain in detail.

**ASSISTANT (summary):** Verified the DDS bytes are produced in memory
(`tasks/build_and_cache_dds.rs`) and already written to a disk cache
(`dds_disk_cache.put(...)`, into `{cache_dir}/{provider}/dds/`). Key
asymmetry: on Linux X-Plane never reads that disk cache — FUSE's `read`
serves the in-memory `DdsResponse` bytes directly; the disk cache is only a
"don't re-encode" optimization. So on Linux the "X-Plane-visible" file is
synthesized at read time; on Windows-Path-B the only X-Plane-visible artifact
is whatever real file exists on disk. Workflow:
1. One-time setup (same as Linux).
2. Before every flight session — the step that doesn't exist on Linux — the
   user pre-warms the starting area (`xearthlayer prewarm --airport KJFK` /
   `--box ...`) and waits for tiles to be written as real files.
3. Launch X-Plane — reads the real files; fast load.
4. During flight — adaptive prefetch generates upcoming regions as real
   files, same as Linux.
5. Failure case that doesn't exist on Linux — if the user flies somewhere the
   pre-warm didn't cover and prefetch didn't catch up, `ERROR_FILE_NOT_FOUND`
   → blank/magenta; on Linux the same is a slow read (up to the 10s
   per-tile budget, then a magenta placeholder) — present, not absent.

Provided a comparison table (Linux vs. Path B) across first scene, flying to
a new region, user mental model, failure mode, and required user action.
Conclusion: the user *does* have to pre-generate the starting area every
time they fly somewhere new. Added an unverified caveat: assumed X-Plane on
Windows re-reads scenery files as you fly between regions (the DSF-region
model); worth verifying, because if it caches the scenery list at startup,
even the prefetch part of Path B breaks.

---

**USER:** I see. Path B requires the user to get the app to load all the
immediately needed textures for the initial part of the flight, and then the
app relies on adaptive prefetch for tiles as movement begins. This totally
removes the need to monitor or intercept file system calls. Do I have this
correct?

**ASSISTANT (summary):** Yes — correct. The core reframe is right: Path B has
no interception layer at all (no FUSE, no WinFSP, no kernel minifilter, no
driver install). X-Plane reads ordinary files from `zzXEL_ortho/textures/`;
XEarthLayer's job is purely to make those files exist in time. Two honest
caveats about guarantees, not mechanism:
- "Immediately needed" is a guess (a box around the starting point) — there's
  no way to know X-Plane's actual first read, which is why Linux doesn't
  guess.
- Prefetch has to win the race against network/encode throughput; if you fly
  faster than it can fill the region, that area is blank until the files land.

So correct, and the "no filesystem interception" conclusion follows exactly.
The cost of that simplicity is the two caveats, which is why it can't match
Linux's "just fly, it works" guarantee. Given the requirement, still points at
Path A (WinFSP/Dokan).

---

**USER:** Yes, please scope Path A. As I understand it, WinFSP supports
FUSE3 functionality.

**ASSISTANT (summary):** Corrected one part of the understanding first, because
it changes the architecture:
- "WinFSP supports FUSE3" is true but is two different things: (a) the
  **WinFSP "FUSE compat layer"** (`fuse.h`) — a C-level FUSE 3.2 shim, right
  for porting C FUSE filesystems but the *wrong* layer here (C FFI against a
  C API); (b) the **idiomatic `winfsp` Rust crate** (over `winfsp-sys`) — a
  Rust trait analogous to the fuse3 `Filesystem` trait, whose `FileSystemHost`
  supports **async implementations of `read`, `write`, and `read_directory`.**
  So the async model maps onto WinFSP in Rust — a re-expression, not a rewrite.
  Prior art: the `zero-sfs-mount` crate (a `FsAdapter` core with a `fuse`
  feature for libfuse and a `winfsp` feature for WinFSP).

What makes this tractable (all verified against the code):
- The FUSE surface is **read-only**: `impl Filesystem` (`ortho_union_fs.rs:657`)
  implements `init`, `destroy`, `lookup`, `getattr`, `read`, `readdir`,
  `readdirplus`, `open`, `release`, `opendir`, `access`, `flush`, `fsync`,
  `statfs` — and **no** `create`/`mknod`/`mkdir`/`unlink`/`rmdir`/`rename`/
  `symlink`/`setxattr`. That removes the entire Windows mutation/ACL/
  reparse-point complexity. It is a virtual read filesystem — the easy half.
- The on-demand generate path (`ortho_union_fs.rs:835`) is already clean and
  portable: per-open handle memoisation, `await resolve_tile(...)`, zero-copy
  window slice.
- Licensing is workable: repo is **MIT**; WinFSP is **GPLv3 + FLOSS exception**
  (plus $2400/3yr commercial). FLOSS exception permits free use by a
  free-software project (MIT qualifies); WinFSP ships as a signed driver the
  user installs separately (not bundled/re-signed). No blocker.
- Everything downstream is already cross-platform.

What to touch:
- **A. Backend trait split** (core move) — extract union-FS logic into a
  backend-neutral core; re-express per backend: the 14 FUSE ops; the inode
  model (`InodeManager` uses numeric FUSE ino numbers, but WinFSP identifies
  files by a **file-context object** — the biggest conceptual lift);
  `readdir`/`readdirplus` (WinFSP `ReadDirectory` is batched/offset-based,
  `FSP_FSCTL_DIR_INFO`); error/attr types (`Errno` → `winfsp::FspError`;
  drop `MetadataExt`/`getuid`/`getgid` or use the `-o uid=-1` analog).
- **B. Windows plumbing** — `preflight/system.rs:128` `FdLimit::remediate`
  cfg out (0.5.0 relocated this out of `run.rs` into the preflight framework);
  `local.rs:267` `/proc/mounts` → query WinFSP/volume; `mounts.rs`
  `max_background`/`congestion_threshold` drop, `unprivileged mount`/
  `spawn_mount` → WinFSP mount; `symlinks.rs:17` → reparse points /
  `symlink_dir`; `paths.rs` already Windows-ready; `memory_probe.rs`
  already cfg-gated.
- **C. Process model + packaging** — service vs. foreground decision;
  installer/bootstrap to detect and install the WinFSP driver.

Unknowns (at that point): #1 X-Plane read model (plain read vs. mmap —
highest risk); #2 read-transaction timeout during first-scene generation;
#3 `readdir` batching; #4 overlay/symlink path.

Effort: ~5–8 weeks to a first working Windows build, assuming a Windows box
with X-Plane 12 for testing; dominant risks the inode→context re-mapping and
the X-Plane read/mmap behavior. Recommended: (1) a 1–2 day spike proving the
read model + timeout tolerance; (2) a per-method split analysis of
`ortho_union_fs.rs` + `inode.rs`.

---

**USER:** For unknown 1, I can confirm that X-Plane does a simple read file;
does not use mmap.

**ASSISTANT (summary):** This removes the single biggest unknown and the one
that would have forced a different API choice. Consequences:
- The FUSE-compat-layer risk is gone (mmap was its main weakness).
- The idiomatic `winfsp` Rust path with `async read` is fully sufficient; no
  need to drop to the native C API for mmap.
- The read path is the easy, well-supported half of WinFSP;
  `ortho_union_fs.rs:835`'s block-and-generate ports almost verbatim.

What's still open:
- **#2 read-transaction timeout (now TOP RISK, elevated).** With synchronous
  `ReadFile`, the kernel is in a `Read` transaction while we generate. On
  Linux FUSE `read` blocks ~30s and X-Plane tolerates it; on WinFSP, if the
  transaction exceeds the timeout, `ReadFile` returns an I/O error →
  black/magenta texture. Must test: does a WinFSP `Read` blocking ~30s
  succeed or time out? What is the timeout value?
- **#3 `readdir` batching** — rework, not risk.
- **#4 overlay/symlink path** — unchanged.

Risk profile shifted: "can we even do this on WinFSP" is now a **yes**;
remaining questions are "will it behave well under real X-Plane timing/enumeration"
— testable, not architectural. Top risk is the read-transaction timeout;
second is the inode→context re-mapping. Recommended next steps unchanged:
(1) a 1–2 day spike with a configurable blocking `Read` callback to measure
timeout behavior (decisive for #2); (2) the per-method backend-split analysis
of `ortho_union_fs.rs` + `inode.rs`.

---

**USER:** I'd like to revisit Path B and keep it on the table because I might
want to consider it as an option, meaning implementing both A and B. The
default would be FUSE mode where it takes Path A. But if the user selects
"non-FUSE" mode via a command line or GUI option, the app would take Path B
and obtain a starting position from the user.

**ASSISTANT (summary):** Good call — and it actually makes the backend split
*more* necessary, not less, because Path A and Path B share the exact same
generation pipeline. Two key findings that make Path B nearly free to add:

1. The "generate tiles around a starting position" machinery **already exists
   and is cross-platform**: `service/prewarm.rs` has
   `PrewarmOrchestrator::start_prewarm(airport)` and
   `prefetch/prewarm/grid.rs:81` has
   `generate_dsf_grid(lat, lon, grid_rows, grid_cols)`. That is exactly the
   "obtain a starting position, generate the tiles around it" step.

2. The DDS bytes are already produced as a `DdsResponse` in
   `tasks/build_and_cache_dds.rs` and served through the FUSE `read` path
   (`ortho_union_fs.rs:835`). On Linux that disk write is just an
   optimization; for Path B, that same write *is* the deliverable. So the
   pipeline is shared — Path B is a different **sink** on the same pipeline.

That's what lets "both A and B" work cleanly: a shared backend-neutral core
(path resolution, `DdsRequestor`, tile generation) with two sinks — a
WinFSP/FUSE mount (A) and a materialize-to-disk writer (B). The mode flag
just selects the sink at startup. Path B delta: (1) real-file sink writing
DDS to `zzXEL_ortho/textures/`, (2) starting-position input (ICAO or
lat/lon+box), (3) gate the mount off, (4) prefetch writes to the scenery
dir. No driver, no signing, no licensing exposure. ~1–2 weeks on top of the
shared core.

---

**USER:** 1: Should allow ICAO airport codes or a single lat/lon point.

**ASSISTANT:** Locked in — starting position is ICAO code or single lat/lon;
box extent comes from the existing `grid_rows`/`grid_cols` config centered on
the point (no box size for the user to supply). Doc updated.

---

**USER:** 2: --non-fuse

**ASSISTANT:** Locked in — CLI flag is `--non-fuse`. Doc updated.

---

**USER:** 1: Yes, I will fork the repo and clone it onto a Windows machine,
installing the Rust toolchain, etc.

**ASSISTANT:** Good — that unblocks the spike and all Windows-side testing.
The timeout spike (the top-risk unknown for Path A) can now be run on a real
box instead of being deferred. Doc updated.

---

**USER:** 4: So this is just a way to determine X-Plane's behavior when a
read is delayed? Also, for the Path B we need to know what happens if the
DDS file is simply not there. (I'll explain why in a bit). From my
experience with AutoOrtho, it seems that X-Plane can tolerate a long delay
on a read, which ends up with poor FPS and unusable stuttering. I don't
think the reads ever time out (not 100% certain). So, I think we can skip
this. BTW, where did you get the 30 sec figure?

**ASSISTANT:** Acknowledged the 30s error. Traced it in code: the per-tile
generation budget is **10s** (`config/settings.rs:103-106` — "If exceeded,
returns a magenta placeholder"), and at the end of it the app serves a
**valid magenta placeholder DDS** (`fuse/placeholder.rs:71`), not a hang.
The "30s" I kept citing was the *total scene-load* time from the project
overview, conflated with the per-tile budget. Corrected the doc: the risk
is no longer "X-Plane might time out" (your AutoOrtho experience says it
tolerates delays) but "does WinFSP's read-transaction ceiling sit above or
below our 10s per-tile budget?" — a config question with a fallback
(serve the placeholder earlier). Spike downgraded from "decisive first
step" to "cheap, optional verification".

Open question noted: **Path B — what does X-Plane do when a DDS file is
simply not there?** (Karl to explain why this matters.)

---

**USER:** For Path B, the X-Plane user has the ability to stop a flight and
start a new one at any location while remaining inside of the X-Plane app.
So, there is no opportunity for the xearthlayer to block-prompt for a new
location to use for prefight prefetch. This will result in no tiles being
available at the new flight's starting location. A less-binary method of
getting into trouble with missing tiles is if the user uses the "teleport"
command in X-Plane which makes the plane's ground speed increase by as much
as 16X. This could easily outrun the adaptive prefetch. Also, a very fast
plane could outrun the adaptive prefetch. Do we know what happens if a DDS
file simply doesn't exist, which would be the case in these scenarios?

**ASSISTANT:** Recorded all three scenarios and the core question. At that
point the working assumption (later corrected by Karl's test — see §5) was
that a missing DDS would be a tolerated missing texture, on the strength of
the app's own magenta-placeholder failure mode (`fuse/placeholder.rs`).
- **The three scenarios** (in-session flight restart at new location,
  `teleport` at up to 16× ground speed, very fast plane at cruise) are all
  cases where X-Plane reads a DDS path that isn't on disk. In Path A the
  same situations never produce a missing file (the read is intercepted and
  becomes an on-demand generation). In Path B they produce a hard
  `ERROR_FILE_NOT_FOUND` with no interception and no in-session recovery.
- **Conclusion at the time:** these are exactly the scenarios that
  distinguish the two modes and reinforce Path A as the default.

---

## 7. Open questions for Karl

1. **Windows test box:** *Resolved 2026-09-28* — Karl will fork the repo,
   clone it onto a Windows machine, and set up the Rust toolchain there.
   Spike + integration testing can proceed on that box.
2. **Service vs. foreground** process model for the ortho mount?
   *Resolved 2026-09-28: Option 1 — foreground user-mode process, same as
   today's `xearthlayer run`. The mount target changes to a WinFSP volume;
   no Windows service, no SCM, no GUI.*
3. **Doc location:** *Resolved 2026-09-28* — not committing to source
   control here (no write access). Karl will manually carry
   `CLAUDE.md` + `windows-port-scope.md` into the fork clone on the Windows
   dev machine; whether they land in the new repo is undecided.
4. **Spike:** *Karl 2026-09-28* — X-Plane tolerates long read delays (AutoOrtho
   experience); reads probably don't time out (not 100% certain); the spike is
   skipped for now. Note: the "30s" in the doc was conflated — the per-tile
   budget is **10s** (`config/settings.rs:104-106`) after which a magenta
   placeholder is served; the "30s" is the total *scene-load* figure. The
   spike remains a cheap, optional way to learn WinFSP's read-transaction
   ceiling (see §4.7 step 1) before the full backend split.
5. **Path B starting-position UX:** *Resolved 2026-09-28* — ICAO airport
   code or a single lat/lon point (not a user-supplied box); box extent comes
   from the existing `grid_rows`/`grid_cols` config.
6. **Path B — missing-DDS behavior in X-Plane (Windows):** *Confirmed 2026-09-28
   (Karl's test — delete one ortho DDS, launch).* **X-Plane reports the
   missing file in an error popup and will not start the sim** — a fatal
   error, not a tolerated missing texture. This corrects the earlier
   inference: the magenta placeholder (`fuse/placeholder.rs`) is a valid
   DDS the app synthesises on generation failure (10s per-tile timeout,
   `config/settings.rs:103-106`); X-Plane never sees an absent file on
   Linux because FUSE intercepts the read and always returns bytes.
   - **Karl's three scenarios** (in-session flight restart at a new
     location, `teleport` at up to 16× ground speed, very fast plane at
     cruise) all produce a missing file in Path B — now known to be a hard
     stop, not a visual degradation. In Path A the read is intercepted and
     becomes an on-demand generation (slow, magenta-flicker, then present).
   - **Conclusion (decisive):** Path B is **infeasible as a production
     mode** — there is no way to keep the sim from stopping on a missing
     DDS without read interception. The FUSE/WinFSP layer is what makes the
     app work. **Path B discarded; Path A (WinFSP) is the only path** (2026-09-28).
7. **Path B naming:** *Resolved 2026-09-28* — `--non-fuse` CLI flag.

---

## 8. Session handoff — state as of 2026-09-28

For whoever (agent or human) picks this up on the Windows machine: the
decisions are all made and recorded in this doc; no open design questions
remain.

**Superseded for current state (2026-10-27):** this is a 2026-09-28
snapshot. The base-commit pin (`ed1439a`), the "first actions," and the
upstream-PR framing below are stale. The current environment, gate list,
and next step are in §4.11 and §4.9 (reframed).

**Done:**
- Scoping complete; every open question in §7 resolved.
- Path B discarded (§3, §5); **Path A (WinFSP) is the only path**.
- Overall goal set (§1, §4.9): upstream PR against samsoir/xearthlayer that
  adds Windows without regressing Linux, in three independently-mergeable
  PRs.
- Dual-OS rule established (§4.8): runtime branching allowed for pure-Rust
  behavior; `#[cfg]`/separate units required only for OS-specific symbols.
- Dependency matrix verified against the actual manifests (§4.8.1) — note
  `fuse3` is a git-fork dep (`samsoir/fuse3`), `rlimit` lives in the CLI
  crate, and `nix` (not just `libc`) is the Unix-ops crate.

**Next, in order (first actions on the Windows box):**
1. **Pin the fork clone to base commit `ed1439a` (v0.4.8)** so every
   file:line anchor in this doc stays valid. Set up the Rust toolchain
   (MSVC, `x86_64-pc-windows-msvc`) and install WinFSP.
2. **Verify the cross-platform assumptions with a real build:** does
   `intel_tex_2` v0.5 load and produce correct BC1/BC3 on the target CPU?
   Does `wgpu` (DX12) + `block_compression` work? One `cargo build` plus a
   quick encode test answers both (§4.5).
3. **Optional spike (§4.7 step 1):** minimal `winfsp` mount, blocking
   `Read` callback at 5s then 10s, observe whether `ReadFile` succeeds
   slowly or errors. Purpose: pin the WinFSP read-transaction ceiling
   against the 10s per-tile budget — the top remaining risk (§4.4 #2).
4. **PR 1 work (§4.9):** the per-method backend-split analysis of
   `ortho_union_fs.rs` + `inode.rs` — tag every method *portable core* /
   *fuse3-bound* / *needs WinFSP re-expression*. This is the first
   reviewable, Linux-only refactor and de-risks PR 2.

### 8.1 Appendix — the magenta mechanism (verified in code, base commit)

Recorded here so it doesn't have to be re-derived. All magenta output is
**xearthlayer synthesising a valid DDS in response to an image-data
failure** — never X-Plane behavior (a missing DDS is fatal, §5).

- **Whole-tile solid-magenta placeholder** (`fuse/placeholder.rs`, 4096×4096
  BC1 full mipmap chain, `255,0,255`):
  - `fuse/fuse3/shared.rs:406` — coalesced request's channel failed/closed
  - `fuse/fuse3/shared.rs:476` — DDS daemon channel closed
  - `fuse/fuse3/shared.rs:496` — **generation timeout** past the 10s budget
    (`config/settings.rs:103-106`)
  - `fuse/fuse3/shared.rs:432` — `validate_dds_or_placeholder` rejects
    returned bytes (empty / wrong size / bad `DDS ` magic)
  - `fuse/fuse3/ortho_union_fs.rs:448` — `resolve_tile` fallback
- **Per-chunk magenta fill** (256×256 rectangles in an otherwise valid
  tile):
  - `tasks/build_and_cache_dds.rs:388` — chunk download failed
  - `tasks/build_and_cache_dds.rs:376` — chunk JPEG decode failed
  - such a tile is still a valid DDS, served to X-Plane, but **never
    cached** (memory or disk), gated on `is_complete()` — issue #180.
