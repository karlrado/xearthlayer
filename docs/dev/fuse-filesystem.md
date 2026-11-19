# FUSE Virtual Filesystem Architecture

This document describes XEarthLayer's FUSE virtual filesystem implementation for serving X-Plane scenery with on-demand orthophoto generation.

## Current Implementation Status

The following features are **implemented and working**:

| Feature | Status | Description |
|---------|--------|-------------|
| Consolidated ortho mount | ✅ Complete | The single union FUSE mount (sole path since #233, which deleted PassthroughFS and UnionFS) |
| X-Plane Path Detection | ✅ Complete | Auto-detects X-Plane 12 install from reference file |
| On-demand DDS Generation | ✅ Complete | Downloads satellite imagery and encodes to BC1/BC3 DDS |
| Multi-provider Support | ✅ Complete | Bing Maps, Google GO2, Google Maps API |
| Three-tier Caching | ✅ Complete | Memory, DDS disk, and chunk disk caches for generated textures |
| Async Pipeline | ✅ Complete | Request coalescing, parallel downloads, async encoding |
| Graceful Shutdown | ✅ Complete | Auto-unmount on SIGTERM/SIGINT |
| Package Management | ✅ Complete | Install, update, remove regional packages |
| Consolidated Mounting | ✅ Complete | Single mount for all sources (v0.2.11+) |
| Tile Patches | ✅ Complete | Custom mesh/elevation from airport addons |
| Overlay Support | ✅ Complete | Consolidated overlay symlinks |
| Predictive Prefetch | ✅ Complete | Radial prefetcher with X-Plane telemetry |
| Scenery Index | ✅ Complete | Fast tile coordinate lookup across packages |

## Overview

XEarthLayer creates virtual filesystem mounts that appear as standard X-Plane scenery folders. The FUSE layer intercepts file requests and either:
1. **Passes through** real files from the scenery pack
2. **Generates on-demand** orthophoto DDS textures
3. **Returns not found** for unrecognized resources

This allows distributing compact scenery packs (DSF + terrain files) without the massive storage requirements of pre-rendered orthophotos.

## Consolidated Mounting (v0.2.11+)

Starting with v0.2.11, XEarthLayer uses **consolidated mounting** - a single FUSE mount that combines all ortho sources (patches + regional packages) into one unified view.

### Benefits

| Before (≤0.2.10) | After (≥0.2.11) |
|------------------|-----------------|
| N mounts for N packages | Single `zzXEL_ortho` mount |
| Separate patches mount | Patches integrated into main mount |
| Per-region overlays | Single `yzXEL_overlay` folder |
| N FUSE sessions | Single shared FUSE session |
| N XEarthLayerService instances | Single shared service |

### Mount Points

| Mount | Type | Description | Priority |
|-------|------|-------------|----------|
| `yzXEL_overlay` | Symlinks | Consolidated overlay DSFs | Highest (loaded first) |
| `zzXEL_ortho` | FUSE | All ortho sources (patches + packages) | Lowest (loaded last) |

### Source Priority

Within the consolidated mount, file resolution follows **alphabetical folder name** ordering:

```
Priority (highest to lowest):
1. _patches/A_KDEN_Mesh/    ('_' prefix sorts first)
2. _patches/B_KLAX_Mesh/
3. eu/                      (alphabetically: eu < na < sa)
4. na/
5. sa/
```

Patches automatically have highest priority because their internal `_patches/` prefix sorts before region codes.

## Regional Scenery Packages

XEarthLayer distributes scenery in regional packages. Each region includes both ortho and overlay components.

### Available Regions

| Region | Code | Description |
|--------|------|-------------|
| North America | `na` | Continental US, Canada, Mexico |
| Europe | `eu` | European continent |
| South America | `sa` | South American continent |

### Package Structure

Each regional package follows standard X-Plane scenery structure:

```
na/
├── ortho/
│   ├── Earth nav data/
│   │   └── +30-120/
│   │       ├── +33-119.dsf
│   │       └── ...
│   └── terrain/
│       ├── 94800_47888_BI18.ter
│       └── ...
└── overlay/
    └── Earth nav data/
        └── +30-120/
            ├── +33-119.dsf
            └── ...
```

**Included in ortho packages:**
- DSF files (mesh, airports, objects)
- Terrain files (.ter)
- Water masks and transitions (.png)

**NOT included in ortho packages:**
- Orthophoto textures (.dds) - generated on-demand

**Included in overlay packages:**
- DSF files with road networks, railways, powerlines, forest placement

## Overlays

Overlays restore roads, railways, powerlines, and forests that are excluded from orthophoto tiles.

### Why Overlays Are Needed

Orthophoto imagery shows the ground as photographed from satellites, but X-Plane needs vector data for:
- **Roads** - For AI traffic and visual accuracy
- **Railways** - Track routing and visual rendering
- **Powerlines** - Visual scenery elements
- **Forests** - 3D tree placement and autogen

Without overlays, orthophoto scenery would show roads in the imagery but lack the 3D elements X-Plane expects.

### Consolidated Overlays

All overlay packages are combined into a single `yzXEL_overlay` folder:

```
X-Plane 12/Custom Scenery/yzXEL_overlay/
└── Earth nav data/
    ├── +30-120/           <- From na/overlay
    │   ├── +33-119.dsf
    │   └── ...
    ├── +40+000/           <- From eu/overlay
    │   ├── +48+002.dsf
    │   └── ...
    └── ...
```

The `yz` prefix ensures overlays load **before** the `zz` orthophoto mount (alphabetically), which is required for proper rendering - overlay elements draw on top of the orthophoto terrain.

## Tile Patches

Tile patches allow using custom mesh/elevation data from third-party airport addons. See [patches.md](../patches.md) for full documentation.

### How Patches Integrate

Patches are stored in `~/.xearthlayer/patches/` and automatically included in the consolidated mount with highest priority:

```
zzXEL_ortho (consolidated mount)
├── _patches/A_KLAX_Mesh/   <- Highest priority
├── _patches/B_KDEN_Mesh/
├── eu/                      <- Regional packages
├── na/
└── sa/
```

## Request Routing

```
┌─────────────────────────────────────────────────────────────────┐
│                     X-Plane File Request                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  FUSE Intercept │
                    └─────────────────┘
                              │
                              ▼
                 ┌────────────────────────┐
                 │  OrthoUnionIndex       │
                 │  Resolve source        │
                 └────────────────────────┘
                              │
                              ▼
                 ┌────────────────────────┐
                 │ File exists in source? │
                 └────────────────────────┘
                    │              │
                   Yes             No
                    │              │
                    ▼              ▼
            ┌──────────────┐  ┌─────────────────────┐
            │  Passthrough │  │ DDS orthophoto      │
            │  (serve file)│  │ pattern match?      │
            └──────────────┘  └─────────────────────┘
                                  │            │
                                 Yes           No
                                  │            │
                                  ▼            ▼
                          ┌─────────────┐  ┌─────────┐
                          │  Generate   │  │ ENOENT  │
                          │  on-demand  │  │ (not    │
                          │  via async  │  │ found)  │
                          │  pipeline   │  └─────────┘
                          └─────────────┘
```

### File Type Handling

| File Type | Pattern | Handling |
|-----------|---------|----------|
| DSF | `*.dsf` | Passthrough from source |
| Terrain | `*.ter` | Passthrough from source |
| Water masks | `water_*.png` | Passthrough from source |
| Other PNG | `*.png` | Passthrough if exists, else ENOENT |
| Orthophoto DDS | `{row}_{col}_{maptype}{zoom}.dds` | Generate on-demand |
| Other DDS | `*.dds` | ENOENT (not an orthophoto) |
| Unknown | `*` | Passthrough if exists, else ENOENT |

### DDS Pattern Recognition

Orthophoto DDS files follow the Ortho4XP naming convention:

```
{row}_{col}_{maptype}{zoom}.dds
```

**Examples:**
- `100000_125184_BI18.dds` - Valid orthophoto, generate on-demand
- `airplane_texture.dds` - Not an orthophoto pattern, return ENOENT
- `normals_100000_125184.dds` - Not an orthophoto pattern, return ENOENT

The regex pattern for recognition:
```regex
(\d+)_(\d+)_([A-Za-z]+\d?)(\d{2})(_LIT)?\.dds$
```

### Lit Textures (Night Lighting)

Names ending `_LIT.dds` are lit textures for night lighting. They get their own inode (`LIT_INODE_FLAG`), never reach the DDS client, coalescer or caches, and are served by a `LitTextureSource`, which in phase 1 is the 152-byte black DDS. See [night-lighting-design.md](night-lighting-design.md).

## FUSE Operations

### Directory Operations

| Operation | Behavior |
|-----------|----------|
| `readdir` | Union of all sources, with priority ordering |
| `getattr` | Return attributes from winning source |
| `lookup` | Resolve via OrthoUnionIndex, pattern match for DDS |

### File Operations

| Operation | Behavior |
|-----------|----------|
| `open` | Virtual DDS inodes: `VIRTUAL_DDS_OPEN_FLAGS` (platform-dependent, see below) plus a file handle; Real passthrough files: default flags, no handle |
| `read` | For DDS: slice the handle's tile, producing it on first read; For others: seek and read the requested range |
| `release` | Drop the handle's tile |

### Open Flags for Virtual DDS Files

The fuse3 adapter opens virtual DDS inodes with the flags decided by
`vfs/fuse3/fuse_platform.rs` — the fuse3 backend's platform-policy module
(`virtual_dds_open_flags()`, and the one `#[cfg(target_os)]` policy site in
`vfs/`; see `docs/dev/filesystem-backend-split.md`). The value is
platform-dependent:

**Linux: `FOPEN_DIRECT_IO`** -- bypasses the kernel page cache for generated textures:

- **Exact read counters** -- every X-Plane DDS read goes through the FUSE handler, so the `FuseRead` metrics count every read
- **No stale data** -- page cache cannot serve outdated DDS data after provider changes or cache clears
- **Reduced kernel memory** -- no page cache duplication of data already in the moka memory cache
- Real passthrough files use default kernel caching (unchanged)

**macOS: `0` (page cache enabled)** -- direct I/O breaks X-Plane's `mmap`-based
texture loading under macFUSE (`EXC_BAD_ACCESS` in the sim's texture loader),
so generated textures use normal kernel caching. Consequences:

- **Read counters under-report** -- once the kernel caches a DDS file, repeat reads never reach FUSE, so the `FuseRead` metrics count each tile's *cold first read* only. Nothing functional depends on per-read visibility: the position-inference and scene-tracker consumers that once did were deleted in #204, having had no production caller on any platform
- **Prefetch is unaffected** in practice: the adaptive prefetcher is driven by Web API telemetry, and cold reads (the ones that matter for generation) always reach FUSE
- Stale-data risk is acceptable: generated DDS content for a given tile/provider is immutable within a run
### Ranged Reads and Handle Reuse (#233, #234)

A FUSE `read()` is always a *ranged* request. Linux caps every one at
`max_pages * PAGE_SIZE` -- 1 MiB -- regardless of what the application asked
for, so one X-Plane whole-file read arrives as many calls: 12-23 for an
11.17 MB DDS, up to 238 for the largest ortho DSF. Direct I/O does not change
this; it only stops the kernel answering any of them from cache.

Both read paths are built around that:

- **Passthrough files** seek to the offset and read only the requested window.
  Reading the whole file to serve one window moved it once per call (#233).
- **Virtual DDS files** get a handle at `open()`. The first read produces the
  tile, later reads slice it, and `release()` drops it. Each read used to be a
  separate executor round trip that cloned the whole tile twice (#234).

`open()` deliberately does **not** produce the tile: generation can take up to
the configured timeout, and X-Plane may open a file it never reads. Handles pin
memory, so past `MAX_PINNED_TILE_BYTES` `open()` stops handing out memoising
handles and reads fall back to resolving per call -- a throughput bound, never a
correctness one. `open_dds_handles()` and `pinned_tile_bytes()` report the live
figures.

### Synthesized DDS Attributes

For on-demand DDS files, we synthesize file attributes:

```rust
FileAttr {
    size: expected_dds_size,  // Based on encoder settings
    kind: RegularFile,
    perm: 0o444,              // Read-only
    mtime: pack_mtime,        // Use pack modification time
    ...
}
```

## Package Management

### CLI Commands

| Command | Description |
|---------|-------------|
| `xearthlayer packages list` | List available and installed packages |
| `xearthlayer packages install <region>` | Download and install a region |
| `xearthlayer packages update [region]` | Update installed packages |
| `xearthlayer packages remove <region>` | Remove a package |
| `xearthlayer packages check` | Check for available updates |

Installing a region (e.g., `na`) downloads both ortho and overlay components.

### Package Distribution

Packages are distributed as split archives:
- Format: `.tar.gz` split into parts
- Hosted on: GitHub Releases
- Versioning: Semantic versioning per region
- Discovery: Library index at configured URL

## Storage Locations

### Package Location (Configurable)

```
Default: ~/.xearthlayer/packages/
         ├── na/
         │   ├── ortho/
         │   └── overlay/
         ├── eu/
         │   ├── ortho/
         │   └── overlay/
         └── ...
```

### Patches Location

```
~/.xearthlayer/patches/
├── A_KLAX_Mesh/
│   ├── Earth nav data/
│   └── terrain/
└── B_KDEN_Mesh/
    └── ...
```

### Mount Points (X-Plane Custom Scenery)

```
X-Plane 12/Custom Scenery/
├── Global Airports/              <- Real folder (X-Plane default)
├── yzXEL_overlay/                <- Symlink folder (consolidated overlays)
└── zzXEL_ortho/                  <- FUSE mount (consolidated ortho)
```

## Mount Lifecycle

### The Mount Seam

All mounting goes through `vfs/backend.rs`. The manager builds an
`OrthoUnionCore` (`vfs/common/core.rs`), fully wired — geo index, state
observer, lit source, pinned budget, metrics, timeout — wraps it as
`Arc<dyn UnionFilesystem>`, and calls `select_backend().mount(fs,
MountRequest)`. The returned `MountSession` answers `mountpoint()` and
`is_mounted()`, unmounts on `unmount()` (resolving only once the mount is
gone from the kernel's table), and **guarantees unmount on drop** — the
manager's teardown and the panic hook's `force_unmount` both rely on that.
`LinuxFuse` and `MacFuse` (`vfs/fuse3/adapters.rs`) are the two fuse3
backends; `select_backend()` is the crate's one platform choice, and
`fuse_platform.rs` carries the mount-call/open-flag/volname policy inside
the backend. The fuse3 adapter itself has no mount path of its own.
Per-backend drop-unmount tests are `#[ignore]`d unit tests plus
`tests/live_mount.rs`, run by `make live-fuse`. The design lives in
`docs/dev/filesystem-backend-split.md`.

### Startup Sequence

```
1. Read configuration
2. Discover installed packages from packages directory
3. Discover patches from patches directory
4. Build OrthoUnionIndex (patches + packages, sorted)
5. Create shared XEarthLayerService (cache, pipeline, limiters)
6. Build OrthoUnionCore; mount it via the seam: select_backend().mount(...) at zzXEL_ortho
7. Create consolidated overlay symlinks at yzXEL_overlay
8. Start prefetch listener if telemetry enabled
9. Display dashboard and wait for Ctrl+C
```

### Shutdown Sequence

```
1. Signal shutdown (Ctrl+C received)
2. Stop prefetch listener
3. Drop the MountSession (drop-unmount is the seam's guarantee)
4. Flush pending cache writes
5. Clean up resources
6. Remove overlay symlinks (optional)
```

### Runtime Management

```bash
# Primary command - mount all installed packages
xearthlayer run

# With pre-warming around an airport
xearthlayer run --airport KJFK

# Override provider for this session
xearthlayer run --provider go2

# Advanced: mount single scenery pack (debugging)
xearthlayer start --source /path/to/scenery
```

## Implementation Modules

```
xearthlayer/src/
├── vfs/                    # Virtual filesystem domain (backend-neutral + backends)
│   ├── mod.rs              # Exports: filename, placeholder, coalesce, EXPECTED_DDS_SIZE
│   ├── backend.rs          # Mount seam: MountBackend/MountSession, select_backend()
│   ├── common/
│   │   ├── core.rs         # OrthoUnionCore — backend-neutral union filesystem
│   │   ├── union.rs        # UnionFilesystem contract (object-safe, boxed futures)
│   │   ├── node.rs         # FsNode — what a path node is
│   │   ├── error.rs        # FsError — the neutral error space
│   │   ├── attrs.rs        # Neutral FileAttributes/Entry/DirEntry/StatFsInfo
│   │   ├── requestor.rs    # DDS request pipeline (chunk coords, requestor trait)
│   │   └── conformance.rs  # check_union_filesystem — the contract suite
│   ├── filename.rs         # DDS filename parsing
│   ├── placeholder.rs      # Magenta placeholder generation
│   └── fuse3/
│       ├── ortho_union_fs.rs   # Thin fuse3 adapter (inode identity + kernel translation)
│       ├── adapters.rs         # LinuxFuse/MacFuse backends + Fuse3MountSession
│       ├── fuse_platform.rs    # Platform policy: mount call, open flags, volname, FileAttr
│       ├── inode.rs            # InodeManager (kernel identity)
│       ├── unmount.rs          # Mount-table operations (fusermount3 / umount)
│       └── types.rs            # Fuse3Error, errno_from
├── ortho_union/
│   ├── index.rs            # OrthoUnionIndex (file → source mapping)
│   └── builder.rs          # Parallel index build with terrain/textures lazy dirs
├── geo_index/              # Region-indexed geospatial reference database
├── patches/                # Patch discovery and union index
├── service/
│   ├── facade.rs           # XEarthLayerService (runtime, caches, wiring)
│   └── cache_layer.rs      # Cache lifecycle and budgets
├── manager/
│   ├── mounts.rs           # MountManager — mounts through the seam, holds sessions
│   └── symlinks.rs         # Overlay symlink management
├── executor/               # Two-lane job executor (demand + prefetch)
├── runtime/                # Orchestrator, lanes, requests
├── jobs/  tasks/           # Tile generation job/task units
└── prefetch/
    ├── adaptive/           # AdaptivePrefetchCoordinator (flight-phase aware)
    └── state_observer.rs   # FUSE-side prefetch claim demotion
```


### Key Types

| Type | Module | Purpose |
|------|--------|---------|
| `OrthoUnionCore` | `vfs/common/core.rs` | Backend-neutral union filesystem (implements `UnionFilesystem`) |
| `Fuse3OrthoUnionFS` | `vfs/fuse3/ortho_union_fs.rs` | Thin fuse3 adapter: inode identity + kernel translation only |
| `LinuxFuse` / `MacFuse` | `vfs/fuse3/adapters.rs` | The fuse3 `MountBackend`s behind the seam |
| `MountSession` | `vfs/backend.rs` | Live mount contract: `is_mounted`, `unmount`, drop-unmounts |
| `OrthoUnionIndex` | `ortho_union/index.rs` | Maps files to sources with priority |
| `MountManager` | `manager/mounts.rs` | Orchestrates consolidated mounting through the seam |
| `AdaptivePrefetchCoordinator` | `prefetch/adaptive/` | Predictive tile caching |

## Error Handling

### Runtime Errors

| Error | Handling |
|-------|----------|
| DDS generation fails | Return magenta placeholder texture |
| Network timeout | Return cached version if available, else placeholder |
| Source file missing | Check next source in priority order |
| Disk full | Log warning, evict cache entries |

### Graceful Degradation

XEarthLayer prioritizes availability:
1. Always return something for valid orthophoto requests (placeholder if needed)
2. Never crash the FUSE mount due to transient errors
3. Log errors for debugging but don't block X-Plane

## Performance Considerations

### Caching Strategy

See [cache-design.md](cache-design.md) for detailed caching architecture.

Summary:
1. Memory cache for hot tiles (2GB default)
2. Disk cache for generated DDS files (20GB default)
3. Request coalescing prevents duplicate work

### Job Executor Daemon

The DDS generation system uses a job/task framework:
- Jobs (DdsGenerateJob) submitted via DdsClient trait
- Tasks execute sequentially: Download → Assemble → Encode → Cache
- HTTP downloads via tokio + reqwest (non-blocking)
- CPU-bound work (encode) via spawn_blocking
- Resource pools limit concurrency by type (Network, CPU, DiskIO)

### Concurrency Limiting

| Resource | Limiter | Scaling |
|----------|---------|---------|
| HTTP | `ConcurrencyLimiter` | `min(cpus * 16, 256)` |
| CPU (assemble + encode) | `CPUConcurrencyLimiter` | `max(cpus * 1.25, cpus + 2)` |
| Disk I/O | `StorageConcurrencyLimiter` | Auto-detected by storage type |

## Security Considerations

### Mount Permissions

- FUSE mounts are user-space, no root required (with fuse group membership)
- Mount points are read-only to X-Plane
- Source files are read-only

### Network Access

- Orthophoto downloads only from configured providers
- HTTPS required for all downloads
- No execution of downloaded content

## Compatibility

### X-Plane Versions

- X-Plane 11: Supported
- X-Plane 12: Supported (primary target)

### Operating Systems

- Linux: Native FUSE support (✅ fully working)
- macOS: Requires [macFUSE](https://macfuse.io) (✅ working, Apple Silicon tested — see [macOS guide](../macos.md))
- Windows: Requires WinFSP (⏳ planned)

### Scenery Compatibility

XEarthLayer manages its own scenery packages created with Ortho4XP. Third-party patches (custom airport mesh) are supported via the patches system.
