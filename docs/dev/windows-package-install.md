# XEarthLayer — Windows Package Install (Tier 1)

Date: 2026-10-28
Status: **Implemented 2026-10-28** (commits 1–2; Karl approved the design before
code). Tier 1 of unblocking the scenery-package installer on Windows.
Author: Bionic Agent, pairing with Karl
Parent: `windows-scope.md`; unblocks the Phase 3 in-scene e2e (which needs
content to mount).

## 1. Purpose

Unblock installing scenery packages on Windows, so there is **real ortho
content** to drive the Phase 3 in-scene e2e (X-Plane loading ortho off the
WinFSP mount). This is **Tier 1** of a two-tier split:

- **Tier 1 (this doc):** make the **core install path** work on Windows —
  download, extract, swap-in — plus the two OS-specific helpers it needs
  (the cross-process queue lock, and the disk-capacity device identity).
- **Tier 2 (deferred):** the **overlay** mechanism (the consolidated
  `yzXEL_overlay` folder built from symlinks). Karl does not use overlays, so
  it stays stubbed.

Scope is deliberately **ortho-only**: after Tier 1, `xearthlayer packages` can
**install an ortho package** on Windows. Installing an **overlay** package
still stops at the symlink step (§4) — acceptable here, since overlays aren't
used.

## 2. What's already portable (no work needed)

The 0.5 rework already moved the heavy lifting into portable Rust, so there is
**no process-spawning to port**:

| Step | Implementation | Portable? |
|---|---|---|
| Download parts | `manager/download/` (HTTP, checksum, retry, parallel) | ✅ cross-platform |
| Decompress | `manager/extractor.rs` — `flate2::GzDecoder` (zlib-rs, runtime SIMD) | ✅ cross-platform |
| Un-tar | `manager/extractor.rs` — the Rust `tar` crate | ✅ cross-platform |
| Concat split parts | `manager/extractor.rs` — `PartsReader` (in-process; deletes each part as read) | ✅ cross-platform |
| Swap-in | `manager/aside.rs` — rename (staging sits beside the destination, same volume) | ✅ cross-platform |

`system/filesystem.rs::fs_info` (free space) already has a real Windows impl
(`GetDiskFreeSpaceExW`). `local.rs` / `night/*` `chmod` are cfg no-ops (no exec
bits on Windows). The `windows` crate (v0.61, `Win32_Storage_FileSystem`) is
already a Windows-only direct dependency.

So the **only** two OS-specific helpers left on the ortho install path are the
**queue lock** and the **capacity device identity** — both currently
compile-stubbed to `UnsupportedOnWindows` (the message you saw when you tried
to install).

## 3. The two changes

### 3.1 The queue lock — `manager/queue/lock.rs`

**Requirement (identical on every platform):** one package operation at a time
across all XEL processes; held for the queue's life; **auto-released when the
process dies** (so a killed XEL can't leave a stale lock — this is what keeps
the startup `sweep` safe); a second process is refused immediately with
`QueueBusy`.

**Today (Linux/Unix):** `flock(2)` on a dedicated `.queue.lock` file in the
temp dir, via `nix::fcntl::Flock<File>` (which *owns* the open file).
`LOCK_EX | LOCK_NB`; `EWOULDBLOCK` → `QueueBusy`. On Windows it is a stub that
returns `UnsupportedOnWindows`.

**Target:** the same behavior, cross-platform, via **`fs2`** (the chosen
cross-platform file-lock wrapper). `fs2` maps "try exclusive lock on a file" to
`flock` on Unix and `LockFileEx` on Windows. Both syscalls tie the lock to the
open handle, so it is released when the handle closes / the process exits — the
critical "dead process can't hold the lock" guarantee is preserved.

- `QueueLock` stores whatever keeps the lock alive for the queue's life (today:
  `Flock<File>`). It stays a field of `Shared<S>` (`queue/state.rs:110`), held
  until both queue threads join — unchanged.
- `acquire(temp_dir)`: create the dir, open `.queue.lock` (`create`, `write`),
  then `fs2::FileExt::try_lock_exclusive()`. Map "already locked" → `QueueBusy`;
  any other I/O error → `WriteFailed` (same mapping as today).
- **"Already locked" detection needs no `#[cfg]` at all:**
  `fs2::lock_contended_error()` returns the platform-specific contended error
  (`EWOULDBLOCK` on Unix, `ERROR_LOCK_VIOLATION`/33 on Windows); the code
  compares the failed error's `raw_os_error()` against it and maps to
  `QueueBusy`. Everything else is shared.
- Remove the Windows stub and the
  `#[cfg_attr(target_os = "windows", allow(dead_code))]` on `LOCK_FILE_NAME`
  (now used on Windows).

**Resolved (commit 1):** `fs2` 0.4.3 has **no guard type** —
`FileExt::try_lock_exclusive()` returns `Result<()>` and the lock is tied to the
open handle. So `QueueLock` simply stores the open `File`; dropping it closes the
handle and releases the lock, and the OS releases it however the process exits,
so a stale lock can't outlive its owner. No struct/borrow gymnastics, and the
fallback described here was never needed.

**Tests:**
- `a_second_lock_on_the_same_directory_is_refused_until_the_first_is_dropped` —
  remove its `#[cfg(not(target_os = "windows"))]`; it now runs on Windows too
  (that is the point of the change).
- `acquire_is_refused_on_windows` (asserts `UnsupportedOnWindows`) — remove;
  superseded by the cross-platform test above.

### 3.2 Disk-capacity device identity — `manager/installer.rs::filesystem_of`

**Role:** `capacity()` reads the temp and packages folders and builds a
`Capacity` — `Shared` (one disk) vs `Split` (two disks) — by comparing their
**device identity** (`Capacity::from_filesystems`: `temp.0 == packages.0`). The
free-space half already works on Windows (`GetDiskFreeSpaceExW`); only the
**device identity** is missing.

**Today (Linux/Unix):** `fs::metadata(folder).dev()` (the st_dev). On Windows
it is a stub returning `UnsupportedOnWindows`.

**Target (Windows):** use the **volume serial number** from
`GetVolumeInformationW` (already-available `windows` crate, no new dependency)
as the device identity. Same volume → same serial → `Shared`; different drives →
different serials → `Split`. This matches the intent of the Unix `st_dev`
comparison (are the two folders on the same filesystem?).
- `fs_info_provider` (currently `allow(dead_code)` on Windows) becomes used →
  drop that suppression.
- The `windows`-crate `GetVolumeInformationW` is in the same
  `Win32_Storage_FileSystem` module as the already-used `GetDiskFreeSpaceExW`,
  so the feature is already enabled.

**Tests:** a Windows test that `filesystem_of` returns `Ok` for an existing
folder and reports a **stable** device id for the same path (two calls agree).
(Cross-drive comparison can't be exercised on one machine; the `Shared`/`Split`
branching is already covered by the platform-neutral `Capacity` tests in
`queue/ledger.rs`.)

## 4. What stays stubbed (Tier 2 — overlays)

`manager/symlinks.rs` (the consolidated `yzXEL_overlay` merge) stays the
`UnsupportedOnWindows` stub. This is **not** on the ortho path: the CLI rebuilds
the consolidated overlay only when an **overlay** package was installed
(`xearthlayer-cli/.../handlers.rs` `install_selection`:
`overlay_installed |= result.package_type == PackageType::Overlay`, then
`if overlay_installed { rebuild_consolidated_overlay(...) }`). An **ortho**
install never sets `overlay_installed`, so it never touches the symlink stub.

Consequence: **ortho install works; overlay install still refuses** at the
overlay-rebuild step. Since overlays aren't used here, that's acceptable. (If
overlays are ever wanted on Windows the mechanism is a separate decision —
directory symlink vs per-file hard links vs copy — out of scope now.)

## 5. Scope boundaries (NOT in Tier 1)

- Overlays / `symlinks.rs` (Tier 2, above).
- Windows signal handling (`signals.rs` `SignalWatch` is a no-op) — Ctrl+C keeps
  the console default; a follow-up, not needed to install.
- Enabling the *full* queue-driven installer test suite on Windows — many of
  those exercise overlays; only the lock + capacity tests are enabled here. A
  broader un-gating is a follow-up once overlays are decided.
- No new public API. The `UnsupportedOnWindows` error variant stays (still used
  by the overlay stub).

## 6. Verification

- `scripts/verify.ps1` on the Windows box (fmt + `clippy -D warnings` + tests) —
  must pass; the flipped lock test now runs here.
- WSL/Linux: `cargo clippy -- -D warnings` + `cargo fmt -- --check` (and the
  lock test on Linux, unchanged).
- Karl's box, manual: `xearthlayer setup`, then install a real **ortho**
  package; confirm it downloads, extracts, swaps in, and appears under
  `xearthlayer packages` → Local. This is the first time real content can be
  installed on Windows, and it unblocks the Phase 3 in-scene e2e.

## 7. Commit plan

1. **`feat(windows): queue lock via fs2 (cross-platform)`** — add `fs2`; rewrite
   `manager/queue/lock.rs` (remove the Windows stub; cross-platform `QueueLock`;
   `QueueBusy` mapping); flip the lock test to run on Windows; drop the
   `LOCK_FILE_NAME` `allow(dead_code)`. Verify green both platforms.
2. **`feat(windows): disk-capacity device identity via volume serial`** —
   implement the Windows `filesystem_of` (volume serial + existing free-space);
   drop the `fs_info_provider` `allow(dead_code)`; add the Windows capacity test.
   **Docs ride here:** `windows-scope.md` (package install unblocked for ortho;
   Tier 2 overlays deferred), `CHANGELOG.md` (Windows ortho install), and this
   design doc. Verify green both platforms.

(Karl watches CI; do not block on it.)

## 8. Risks / open items

- **`fs2` guard shape** — resolved (§3.1): no guard type, the lock is tied to
  the open handle; `QueueLock` stores the `File`. The four invariants hold.
- **`QueueBusy` mapping on Windows** — resolved: `fs2::lock_contended_error()`
  surfaces `ERROR_LOCK_VIOLATION` (33) and is compared via `raw_os_error()`,
  so no `#[cfg]` is needed.
- **`GetVolumeInformationW` signature** — resolved: the `windows` 0.61 binding
  takes 6 arguments, with the serial/max-length/flags out-params wrapped in
  `Some(&mut …)`; verified by the compiler.
- **Real-content smoke** — the actual install of a real ortho package is Karl's
  manual step (§6); the automated tests cover the lock and capacity logic but
  not a live network download.
