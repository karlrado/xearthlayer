# XEarthLayer Package Manager Design

This document describes the design of the XEarthLayer Package Manager component.

## Overview

The Package Manager is responsible for discovering, downloading, installing, updating, and removing XEarthLayer Scenery Packages on the user's system. It provides both library and CLI interfaces.

## Responsibilities

1. **Discover** available packages from remote library
2. **Download** packages with resume and parallel support
3. **Install** packages to local storage
4. **Update** packages when new versions are available
5. **Remove** packages from the system
6. **Mount** ortho packages via FUSE (coordinate with existing mount system)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Package Manager                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐   │
│  │   Library    │  │   Package    │  │      Download        │   │
│  │   Client     │  │   Scanner    │  │      Manager         │   │
│  │              │  │              │  │                      │   │
│  │ Fetch remote │  │ Scan local   │  │ Parallel downloads   │   │
│  │ library idx  │  │ packages     │  │ Resume support       │   │
│  │ Parse meta   │  │ Build state  │  │ Checksum verify      │   │
│  └──────────────┘  └──────────────┘  └──────────────────────┘   │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐   │
│  │   Installer  │  │   Mount      │  │      Update          │   │
│  │              │  │   Coordinator│  │      Checker         │   │
│  │ Extract pkg  │  │              │  │                      │   │
│  │ Create links │  │ Track mounts │  │ Compare local/remote │   │
│  │ Verify files │  │ Start/stop   │  │ Notify user          │   │
│  └──────────────┘  └──────────────┘  └──────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Configuration

Settings in `~/.xearthlayer/config.ini`:

```ini
[packages]
# URL to the package library index
library_url = https://packages.xearthlayer.org/xearthlayer_package_library.txt

# Local directory for package storage
install_location = ~/.xearthlayer/packages

# X-Plane Custom Scenery path for overlay symlinks
custom_scenery_path = /path/to/X-Plane 12/Custom Scenery

# Automatically install matching overlay when installing ortho
auto_install_overlays = true

# Temporary directory for downloads
temp_dir = /tmp/xearthlayer
```

## Local Package Discovery

The Manager discovers installed packages by scanning the `install_location` directory for `xearthlayer_scenery_package.txt` files.

### Scan Process

1. List directories in `install_location`
2. For each directory, check for `xearthlayer_scenery_package.txt`
3. Parse metadata file to extract version and package info
4. Build in-memory state of installed packages

### Package State

```rust
pub struct InstalledPackage {
    /// Region identifier (e.g., "EUROPE")
    pub region: String,
    /// Package type (Ortho or Overlay)
    pub package_type: PackageType,
    /// Installed version
    pub version: Version,
    /// Path to package directory
    pub path: PathBuf,
    /// Whether currently mounted (ortho only)
    pub mounted: bool,
}
```

No persistent index file - state is always derived from scanning. This ensures:
- Single source of truth (the actual files)
- Self-healing if user manually modifies packages
- No sync issues between index and reality

## Remote Library Fetching

### Process

1. Fetch `xearthlayer_package_library.txt` from `library_root`
2. Parse library index
3. Check the specification version: a MAJOR above `SUPPORTED_SPEC_MAJOR` (`package/spec.rs`) is refused with an instruction to upgrade XEarthLayer. The same check applies when a package's metadata file is fetched
4. Compare sequence number with cached value for quick change detection
5. Build available packages list

### Caching

- Cache library index locally with timestamp
- Re-fetch if older than configurable TTL (default: 1 hour)
- Force refresh available via CLI flag

## Update Detection

Compare installed packages against remote library:

```rust
pub struct UpdateAvailable {
    pub region: String,
    pub package_type: PackageType,
    pub installed_version: Version,
    pub available_version: Version,
}
```

User is notified of available updates on first run of XEarthLayer (not repeatedly).

## Download Manager

### Features

- **Resumable downloads**: Track progress, resume interrupted downloads via HTTP Range headers
- **Parallel parts**: Sliding-window concurrency with semaphore-gated execution
- **Configurable concurrency**: `packages.concurrent_downloads` (1-10, default 5)
- **Per-part retry**: 3 retries with exponential backoff (2s, 4s, 8s) via `RetryDownloader` decorator
- **Per-part progress**: `DownloadProgress` snapshot with per-part `PartState` (Queued, Downloading, Done, Failed, Retrying)
- **Verification**: SHA-256 checksum each part before proceeding
- **Progress UI**: `indicatif::MultiProgress` bars in CLI (library layer is UI-agnostic)

### Download State

For interrupted downloads, maintain state file:

```
~/.xearthlayer/packages/.downloads/
├── zzXEL_na-1.0.0.download_state
└── zzXEL_na-1.0.0.tar.gz.aa  (partial)
```

State file format (JSON for simplicity):
```json
{
  "package": "zzXEL_na_ortho",
  "version": "1.0.0",
  "total_parts": 5,
  "completed_parts": ["aa", "ab"],
  "current_part": "ac",
  "current_part_bytes": 524288000,
  "started_at": "2025-12-20T10:30:00Z"
}
```

### HTTP Range Requests

For resume support, use HTTP Range header:
```
Range: bytes=524288000-
```

Most CDNs support this. Fall back to full re-download if not supported.

### Sliding Window Execution

`ParallelStrategy` uses a semaphore-based sliding window instead of batch-based parallelism. All download threads are spawned immediately; each acquires a semaphore permit before downloading. When a fast part completes, the next queued part starts immediately -- no waiting for the slowest part in a batch.

### Per-Part Progress Architecture

```
ParallelStrategy (semaphore-gated threads)
  --> ProgressCounters (atomic per-part bytes + state)
      --> ProgressReporter (100ms poll interval)
          --> DownloadProgressCallback(&DownloadProgress)
              --> CLI: indicatif MultiProgress bars
```

`ProgressCounters` uses `AtomicU8` for per-part state (lock-free hot path) and `Mutex<Option<String>>` for error reasons (cold path, only written on failure). Per-part sizes are populated from HEAD requests at construction time so the UI shows accurate totals from the start.

### Retry Policy

Per-part retry via `RetryDownloader` decorator wrapping `HttpDownloader`:
- **Max retries:** 3 (hardcoded)
- **Backoff:** Exponential -- 2s, 4s, 8s
- **Resume on retry:** Existing `download_with_resume()` handles Range-based resume
- **HTTP 416:** Delete partial file before retrying to avoid infinite loop
- **Checksum mismatch:** Delete partial file and retry from scratch
- **Scope:** Per-part only. A failing part does not affect other downloads

## Installation Process

`PackageInstaller::install_all` installs a batch of packages; a single install
is a batch of one. Installing has two halves with different bottlenecks, and
the batch overlaps them (#292).

### Per package

1. **Prepare.** Validate the title and every archive and part file name (each
   must be a single plain path component, since all of them come from metadata
   fetched over HTTP and are joined onto paths). Refuse a version that is
   already installed. Create the package's temp directory under an
   `InstallTempGuard`, then size its parts with HEAD requests.
2. **Download** (network bound). Fetch every part into the temp directory,
   verifying checksums as parts arrive.
3. **Install** (disk bound). Reassemble the parts into one archive and free the
   parts (#260), extract the `tar.gz` in process with the `tar` crate over
   flate2's `zlib-rs` backend (#274), then move the extracted tree into the
   package store. Dropping the guard removes the temp directory on every exit
   path.

### Batch schedule

`manager/pipeline.rs` runs downloads on the calling thread and installs on a
single worker thread, joined by a rendezvous channel (`sync_channel(0)`). A
downloaded package is handed over only once the previous install has finished,
so at most one package installs while one downloads, and package `k` cannot
start downloading before package `k - 2` has left the disk. Packages never
install in parallel: they would only compete for the same disk.

The free space is read once, after every package is sized and before anything
downloads. Installing a package peaks at `SPACE_BUFFER_MULTIPLIER` (4) times
its download, so with download sizes `s`:

| Schedule | Peak |
|---|---|
| Overlapped | `max(4 * s[i] + s[i+1], 4 * s[last])` |
| Sequential | `max(4 * s[i])` |

`pipeline::schedule_for` overlaps when that peak fits, falls back to sequential
when only that fits, and otherwise refuses the batch before downloading. A
batch is therefore never refused where installing its packages one at a time
would have succeeded. A per-package check made when each download starts would
not work: during the overlap it would see the previous package's half-written
tree as used space.

A failure at any stage is that package's result alone; every other package in
the batch still downloads and installs.

### Progress

Progress is reported through `InstallObserver`: `download_started` returns the
per-part callback for the package downloading, and `install_stage` reports the
typed stage of the package installing. The two can arrive concurrently. The
CLI's `ConsoleInstallObserver` shows a spinner line for the install above the
part bars of the download.

### Overlays

The CLI plans the whole batch before it starts: each selected package, followed
by its region's overlay when `packages.auto_install_overlays` is on. Packages
succeed or fail independently, so an overlay installs even if its ortho package
fails. After a batch that installed any overlay, the CLI rebuilds the
consolidated `yzXEL_overlay/` folder once.

### Symlink Considerations

- Check filesystem supports symlinks (warn on FAT32/exFAT)
- Handle existing directory/symlink at target
- Provide manual copy fallback if symlinks unavailable

## Mount Coordination

For ortho packages, coordinate with XEarthLayer FUSE system.

### Multi-Mount Strategy

Each installed ortho package gets its own FUSE mount:

```
Custom Scenery/
├── zzXEL_afr_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_afr_ortho/
├── zzXEL_eur_ortho/  → FUSE mount → ~/.xearthlayer/packages/zzXEL_eur_ortho/
└── zzXEL_na_ortho/   → FUSE mount → ~/.xearthlayer/packages/zzXEL_na_ortho/
```

### Mount State Tracking

Track active mounts for clean shutdown:

```rust
pub struct MountState {
    pub package: String,
    pub source_path: PathBuf,
    pub mount_point: PathBuf,
    pub session: BackgroundSession,
}
```

### Lifecycle

1. **Start**: Iterate installed ortho packages, mount each
2. **Runtime**: Handle mount failures gracefully (log, continue with others)
3. **Shutdown**: Unmount all in reverse order, wait for clean unmount

## Package Removal

### Ortho Packages

1. Check if mounted - unmount first if so
2. Remove mount point directory from Custom Scenery
3. Delete package directory from `install_location`

### Overlay Packages

1. Remove symlink from Custom Scenery
2. Delete package directory from `install_location`

### Partial Removal

If removal fails partway:
- Log what was removed
- Attempt to restore consistent state
- Report to user what manual cleanup may be needed

## CLI Interface

### Commands

```bash
# List available and installed packages
xearthlayer packages list

# Check for updates
xearthlayer packages check

# Install a package
xearthlayer packages install <region> [--type ortho|overlay|all]

# Update packages
xearthlayer packages update [<region>] [--all]

# Remove a package
xearthlayer packages remove <region> [--type ortho|overlay|all]

# Show package details
xearthlayer packages info <region>
```

### Output Examples

```
$ xearthlayer packages list

Available Packages:
  AFRICA        ortho: 1.0.0    overlay: 1.0.0
  ANTARCTICA    ortho: 1.0.0    overlay: 1.0.0
  ASIA          ortho: 1.0.0    overlay: 1.0.0
  AUSTRALIA     ortho: 1.0.0    overlay: 1.0.0
  EUROPE        ortho: 1.2.0    overlay: 1.1.0
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0
  SOUTH AMERICA ortho: 1.0.0    overlay: 1.0.0

Installed:
  EUROPE        ortho: 1.0.0 [update available: 1.2.0]
  NORTH AMERICA ortho: 1.0.0    overlay: 1.0.0

$ xearthlayer packages install europe --type ortho

Downloading EUROPE ortho package (v1.2.0)...
  Part 1/5: zzXEL_eur-1.2.0.tar.gz.aa [################] 100% (1.0 GB)
  Part 2/5: zzXEL_eur-1.2.0.tar.gz.ab [########        ]  50% (512 MB)
  ...

Verifying checksums... OK
Extracting... OK
Registering for FUSE mount... OK

EUROPE ortho package installed successfully.
Run 'xearthlayer start' to begin serving scenery.
```

## Error Handling

### Network Errors

- Retry transient failures with exponential backoff
- Resume from last successful byte on connection drop
- Clear error message if server unreachable

### Disk Space

- Check available space before download
- Estimate required space (archive + extracted)
- Fail early with clear message if insufficient

### Checksum Failures

- Re-download failed part (up to 3 attempts)
- If persistent, report corruption and suggest re-download
- Never proceed with failed checksum

### Mount Failures

- Log detailed error
- Continue mounting other packages
- Report which packages failed at end

## Library Interface

The Package Manager exposes a library API for use by CLI and future GUI:

```rust
pub trait PackageManager {
    /// Scan local packages and return installed state
    fn scan_installed(&self) -> Result<Vec<InstalledPackage>, PackageError>;

    /// Fetch and parse remote library
    fn fetch_library(&self) -> Result<PackageLibrary, PackageError>;

    /// Check for available updates
    fn check_updates(&self) -> Result<Vec<UpdateAvailable>, PackageError>;

    /// Download and install a package
    fn install(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Update a package to latest version
    fn update(
        &self,
        region: &str,
        package_type: PackageType,
        progress: impl Fn(DownloadProgress),
    ) -> Result<(), PackageError>;

    /// Remove an installed package
    fn remove(&self, region: &str, package_type: PackageType) -> Result<(), PackageError>;
}
```

## Security Considerations

- **HTTPS only**: All downloads over TLS
- **Checksum verification**: SHA-256 for all parts
- **No arbitrary code execution**: Packages contain only data files
- **Symlink safety**: Validate symlink targets are within expected paths
