# Night Lighting Design

**Status**: Phase 1 and phase 2a implemented (packaging, black serving, real lit sizes); a phase 2b flight-test generator implemented (nearest class, straight world-anchored tiling, provisional gain, `night.brightness`); stochastic tiling, soft class edges and tuning proposed
**Created**: 2026-10-17
**Branch**: `blackmarble` (local until 0.5.0 release preparation)

## Overview

X-Plane 12 lights its terrain at night through the terrain type: the urban
and industrial `.ter` files carry a lit texture, and the global scenery
assigns those terrain types to city triangles. Ortho4XP replaces that mesh
with its own and writes one `.ter` per ortho texture with no lit texture, so
ortho scenery goes dark beyond the reach of autogen lights.

OXO (the Ortho4 XEarthLayer Orchestrator) now builds night lighting into the
tiles it delivers, in two parts:

- every land `.ter` carries a lit line naming a texture that does not
  exist on disk:
  ```
  TEXTURE_LIT ../textures/<row>_<col>_<map><zoom>_LIT.dds
  LUMINANCE 150
  ```
- one **recipe** per 1° tile, `<tile>.oxn`, describing the lighting the
  tile needs: a class layer naming Laminar terrain types (3 arc-seconds) and
  a brightness layer from NASA's annual VIIRS night lights (15 arc-seconds).

XEL turns the recipe into the lit texture at run time, on the user's
machine, from the lit art already in the user's X-Plane installation. That
art cannot be redistributed, so no package carries it.

This document is XEL's side. It rests on three OXO documents, kept in the
OXO repository:

- *XEarthLayer night lighting: requirements* (R1 to R21, referenced
  throughout this document)
- *Night recipe format*, version 1: the contract between the two projects
- *Night lighting design*: OXO's side and the in-sim evidence

### Facts this design rests on

Established in the simulator (X-Plane 12 beta, 2026-10-16), not inferred:

1. A lit texture renders on Ortho4XP's terrain and uses the ortho texture's
   own coordinates: 0 to 1 across one ortho texture, north up.
2. Lit texture size is independent of the base texture's size. A 4 × 4
   black DDS is valid and draws nothing, because lit is added to the base
   colour.
3. `LIT_FADE_PARAMS` has no effect on this terrain. A distance fade works by
   making the top mip level(s) of the lit DDS black.
4. X-Plane's texture quality setting (`renopt_tex_res_04` in
   `Output/preferences/X-Plane.prf`, 4 = full down to 0) drops one top mip
   level per notch at load time. VR has its own setting.
5. Laminar's lit art is nine 1024 × 1024 DXT1 textures under
   `Resources/default scenery/1000 world terrain/textures12/urban_FAR/`,
   reached from terrain names through `library.txt`. Each lit `.ter` gives
   its pattern's ground scale in metres as the first value of
   `LIT_FADE_PARAMS` (3072, 4096 or 8192).
6. Lit ground tints the cockpit's ambient light, so brightness must stay
   restrained.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Light model | Laminar pattern × VIIRS gain | The project owner's decision. The class picks the Laminar lit texture for street-scale structure; the brightness layer scales its intensity. Nothing lights outside Laminar's urban and industrial classes |
| Where lit work runs | A separate lit path beside the ortho executor, never a job on the demand or prefetch lane | Lit generation must not delay ortho generation (R17) and must not share coalescing or cache keys with it (R8). A separate path guarantees both: lit inodes are routed to the lit source, and the ortho request path returns the black texture for a lit name as a guard |
| Texel density | 1024 px at the first visible level of a ZL16 texture (about 7.5 m per texel), scaling by zoom | The project owner's decision: about 2.8 MB of video memory per urban ZL16 texture, against about 10.7 MB at 2048 px |
| Decoding Laminar's art | `ddsfile` (header and mip layout) and `bcdec_rs` (a pure Rust port of `bcdec`) | No DDS decoder exists in XEL. Decoding happens once per art file per session, so correctness and size matter, not speed |
| Encoding lit textures | XEL's existing compressor selection: ISPC by default, the wgpu worker when `texture.compressor = gpu` | The most efficient encoders XEL has. A lit texture is under a tenth of an ortho tile's texels |
| Recipe container | `ciborium` for CBOR, `flate2` for the zlib layers | CBOR is the format's choice; `flate2` is already a dependency |
| Caching | Memory only, keyed by footprint and a settings fingerprint | Regeneration is cheap (tens of milliseconds) and deterministic. A disk tier would need invalidation whenever X-Plane, its settings or the art changed |
| Configuration | `night.enabled` and `night.brightness` | `brightness` is a visual preference, not a performance knob. It is a development setting, reviewed before release |
| Recipes in a package | `night/<10° block>/<tile>.oxn` | Hidden from X-Plane by the union index's existing root allowlist, on this and every XEL version since 0.3.0 |
| Publishing night packages | Staging library only until XEL 0.5.0 is the minimum client | Pre-0.5.0 XEL (filename regex without `(_LIT)?`) answers ENOENT for every `_LIT.dds` an OXO night `.ter` names, so a night package must not reach any library a pre-0.5.0 client reads. Whether to also bump the package spec version is open |
| Delivery | Two phases: packaging and black serving first, the generator second | Phase 1 alone makes OXO night packages publishable and safe |

## Architecture

```
 package/                          X-Plane
  night/<block>/<tile>.oxn          │ opens 25712_12960_BI16_LIT.dds
  terrain/*.ter (lit line)          ▼
  textures/                   ┌──────────────┐
                              │ FUSE (union) │── ortho .dds ──▶ executor lanes (unchanged)
                              └──────┬───────┘
                                     │ _LIT.dds
                                     ▼
                           ┌───────────────────┐  has_light  ┌──────────────┐
                           │ LitTextureService │◀───────────▶│ RecipeStore  │
                           │ coalesce, cache,  │             └──────────────┘
                           │ own CPU budget    │  patterns   ┌──────────────┐
                           └─────────┬─────────┘◀───────────▶│ LaminarArt   │
                                     │ generate              └──────────────┘
                                     ▼                       ┌──────────────┐
                            LitGenerator (trait) ◀───────────│ XPlanePrefs  │
                            CPU implementation now           └──────────────┘
```

The ortho path is untouched apart from the filename parser learning a
texture kind. Everything else is new code in `xearthlayer/src/night/`:

| File | Responsibility |
|---|---|
| `night/recipe.rs` | Read and validate one `.oxn` file |
| `night/store.rs` | `RecipeStore`: find, load, cache and evict recipes |
| `night/laminar/library.rs` | Parse `library.txt`, resolve terrain names |
| `night/laminar/ter.rs` | Read a Laminar `.ter`'s lit texture and pattern scale |
| `night/laminar/art.rs` | `LaminarArt`: name to decoded lit pattern, cached |
| `night/footprint.rs` | A lit texture's ground footprint and `has_light` |
| `night/layout.rs` | The lit DDS layout: sizes, blank levels, zero regions |
| `night/prefs.rs` | `XPlanePrefs`: the texture quality setting |
| `night/generator/mod.rs` | The `LitGenerator` trait |
| `night/generator/cpu.rs` | The CPU implementation |
| `night/generator/tiling.rs` | Hex-grid stochastic tiling |
| `night/generator/gain.rs` | The gain curve |
| `night/service.rs` | `LitTextureService`: the FUSE-facing entry point |

### Recipe reader (`night/recipe.rs`)

Implements the recipe format, version 1, and nothing more:

- The file is the four magic bytes `OXN1` followed by one CBOR map. The
  reader rejects a wrong magic, a major version other than 1, a `format`
  that differs from the magic's digit, and trailing bytes after the map.
- It accepts any valid CBOR encoding, not only the deterministic one.
- Unknown keys at every level and unknown layer names are ignored. A
  missing `class` or `brightness` layer reads as all zeros.
- A layer whose `zlib` stream does not inflate to exactly
  `width × height` bytes is rejected, and so is a class cell indexing past
  the palette. Inflation is bounded by `width × height`, so a hostile file
  cannot allocate more than it declares (the cache integrity model's
  bounded-read rule).
- Layer sizes are always taken from the layer's `width` and `height`.
- The result is a `Recipe`: tile, palette, class grid, brightness grid and
  the brightness `scale` (`r0_nw`, `rmax_nw`).

### RecipeStore (`night/store.rs`)

The union index hides `night/` from X-Plane, so the store reads recipes
through each ortho source's own path, not through the mount:

- For a 1° tile, it looks for `night/<block>/<tile>.oxn` in each ortho
  source, in the union's priority order (patches before regional
  packages), and takes the first it finds. The block is Ortho4XP's: each
  coordinate floored to a multiple of 10 (`+37-123` is in `+30-130`).
- Recipes load lazily on first use and live in a cache bounded by bytes,
  each about 1.5 MB decoded. "No recipe for this tile" is cached too, so
  countryside costs one lookup per tile per session.
- A rejected file is logged once and then treated as a dark tile.

### LaminarArt (`night/laminar/`)

Resolves a recipe palette name to a decoded lit pattern, through the user's
own installation, so it follows whatever the installed X-Plane uses (R9):

1. `library.txt` in `1000 world terrain` maps library paths such as
   `lib/g10/terrain10/urban_tmp_sdry3.ter` to files such as
   `terrain12/urban_tmp_sdry3.ter`. The parser handles the `EXPORT`
   family of directives and conditional `REGION` blocks, taking the
   unconditional (global) definition.
2. The resolved `.ter` is read for its lit texture (`TEXTURE_LIT`, or
   `LIT_TEX_NOWRAP`) and the first value of `LIT_FADE_PARAMS`, the
   pattern's ground scale in metres.
3. The lit texture is decoded with `ddsfile` and `bcdec_rs` to RGB at its
   top level and kept as a `LitPattern { texels, size, ground_scale_m }`.

A name is lit if and only if it resolves to a `.ter` with a lit texture. A
name that resolves to nothing lit is dark. The X-Plane installation comes
from the existing detection (`XPlaneEnvironment::installation_path()`).
All nine patterns together are about 36 MB decoded, loaded on first use.

### Footprint (`night/footprint.rs`)

Ortho4XP names a texture `<til_y_top>_<til_x_left>_<map><zoom>`, covering
Web Mercator tiles x = `til_x_left` to `til_x_left + 15` and y =
`til_y_top` to `til_y_top + 15` at that zoom (R5). From the name alone the
footprint gives:

- its latitude and longitude bounds, and the mapping from a texel to a
  latitude and longitude, north up, row 0 at the north edge;
- the 1° tiles it touches. A footprint can straddle tile boundaries, and
  each texel is computed from the recipe of the tile containing it;
- `has_light`: whether any class cell inside the footprint resolves to a
  lit pattern and has non-zero brightness. It reads recipes only, never
  generates pixels, and answers the file size before generation (R7).

### Lit DDS layout (`night/layout.rs`)

With the texture quality setting `t` (0 to 4):

- X-Plane drops `4 − t` top levels at load. XEL blanks
  `B = (4 − t) + 1` top levels, so the first level X-Plane uploads is
  black at any setting (R15).
- The first visible level `F` is 1024 px for a ZL16 texture, halving per
  zoom level above 16 and doubling per zoom level below it (512 px at ZL17,
  256 px at ZL18), about 7.5 m per texel throughout.
- The top level is `F × 2^B`, with a full mip chain down to 1 × 1 and
  BC1 (DXT1) blocks throughout.

| Setting `t` | Blank levels | Top level (ZL16) | File size | Uploaded by X-Plane |
|---|---|---|---|---|
| 4 | 1 | 2048 | 2.8 MB | 2.8 MB |
| 3 | 2 | 4096 | 11.2 MB | 2.8 MB |
| 2 | 3 | 8192 | 44.7 MB | 2.8 MB |

A footprint without light is a fixed **minimal black DDS**: 4 × 4, BC1,
with its full chain (4, 2 and 1 px levels), all blocks zero, 152 bytes
(R6).

The blank levels are never stored. A lit texture's bytes are
`header ‖ zero region ‖ payload`, and a read at an offset inside the zero
region is answered with zeros. Only the payload (the visible levels, about
0.7 MB for an urban ZL16 texture) is generated, encoded and cached.

**Risk.** At `t = 0` the top level would be 32768 px. Whether X-Plane
accepts a header that large, even though it only uploads from level 4
onward, is unverified. The fallback is to cap the top level at 16384 px
and accept a 512 px first visible level at the lowest setting. The tuning
flight settles this.

### XPlanePrefs (`night/prefs.rs`)

The texture setting comes from a `TextureSettingSource` trait, so where it
comes from can change without touching the layout, the service or FUSE:

```rust
pub trait TextureSettingSource: Send + Sync {
    /// X-Plane's texture quality, 0 (lowest) to 4 (full).
    fn texture_setting(&self) -> u8;
}
```

- The implementation XEL ships, `StartupTextureSetting`, reads
  `renopt_tex_res_04` from `Output/preferences/X-Plane.prf` once, when XEL
  starts, and holds it for the session.
- If the file is missing or unreadable, it assumes the full setting
  (`t = 4`) and logs once (R16).
- The setting feeds the blank-level count and the file size. Because it
  never changes within a session, a lit file's size never changes either,
  so the kernel's attribute cache can never serve a stale size, on Linux or
  on macOS (which has no direct I/O). A change to the texture setting takes
  effect the next time XEL starts.
- Following the file while running stays possible: a watching
  implementation of the same trait can replace `StartupTextureSetting`.
  It would also have to answer the stale-size question (attribute
  timeouts, macOS page cache), which is why it is not the default.
- VR's `vr_renopt_tex_res_04` is not followed. This is a documented
  limitation.

### LitGenerator (`night/generator/`)

```rust
pub trait LitGenerator: Send + Sync {
    /// Visible levels' pixels for one footprint, top first.
    fn generate(&self, request: &LitRequest<'_>) -> Result<LitImage, LitError>;
}
```

`LitRequest` carries the footprint, the recipes it touches, the resolved
patterns and the settings. The trait leaves room for a GPU generator
later (R17); the CPU implementation comes first.

For each texel of the first visible level, the CPU implementation:

1. **Places the texel in the world.** Its latitude and longitude, and from
   them its Web Mercator position.
2. **Weights the classes.** The class layer is sampled at the four nearest
   cell centres, with the sample position first warped by a smooth,
   world-anchored value noise of about half a cell (45 m). Each lit class
   gets a bilinear weight. The warp breaks the 90 m cell grid into organic
   edges (R12).
3. **Samples each weighted class's pattern** by hex-grid stochastic
   tiling (`tiling.rs`), anchored to world position at the pattern's own
   ground scale (R11):
   - A hex grid is laid over the world, with cells the size of the
     pattern's ground scale.
   - Each hex vertex hashes its integer grid coordinates to a random
     offset and a rotation in 90° steps.
   - A texel blends the pattern as transformed by its three nearest
     vertices, with barycentric weights sharpened and renormalised so
     the blend keeps the pattern's contrast. Each vertex converts world
     position to pattern coordinates with the ground scale at its own
     latitude, so the result has no global shear and is continuous
     everywhere.
   - Because the transforms depend only on world position, two textures
     compute the same light for the same ground, so edges between
     textures and 1° tiles have no seam.
4. **Applies the gain** (`gain.rs`). Brightness is interpolated
   bilinearly in its byte (log) domain and converted to radiance `R` with
   the recipe's `scale`. The gain is a linear ramp in `ln(R)` from a floor
   radiance (gain 0) to a ceiling radiance (gain 1), raised to a gamma, then
   multiplied by `night.brightness` and clamped to 0 to 1 (R10). The floor,
   ceiling and gamma are constants in code, set by the tuning flight; the
   provisional values are a floor of 1.6 (a gain of about 0.05 at 2), a ceiling of 150 nW·cm⁻²·sr⁻¹ and a
   gamma of 1.0.
5. **Outputs** the weighted sum of class patterns × gain, as RGB.

The lower levels are box-filtered from the first visible level, as the
ortho path does, and BC1-encoded through the configured compressor.

Determinism (R13): hashes use fixed seeds and integer inputs, and the
generator holds no state between calls, so the same recipe, art and
settings give the same bytes on the same build.

### LitTextureService (`night/service.rs`)

The entry point FUSE calls. Phase 2a implements the `LitTextureSource` trait
with `NightLitSource` (sizes from `has_light` and the layout, black bytes);
phase 2b's `LitTextureService` replaces it behind the same trait. Both answer
`has_light` once per footprint and remember it for the session, so `size` and
`read` agree. Phase 2b will retry a transient recipe read failure before
committing that answer. The service has two operations:

- `size(name) -> u64`: from `has_light` and the layout. It is cheap and
  never generates.
- `read(name, offset, len) -> Bytes`: serves the header, the zero region
  and the payload, generating the payload on first use.

Inside:

- **Coalescing.** Concurrent requests for one footprint share one
  generation. The key is the footprint plus the settings fingerprint, in a
  coalescer of the lit path's own (R8).
- **CPU budget.** A semaphore of a few permits derived from the core count
  bounds concurrent generations, which run on the blocking pool. The
  permits are added to `LaneSizing::blocking_threads_required()`, the
  blocking pool's cap, as every pool is (#227).
- **Cache.** Payloads live in a memory cache bounded by bytes, keyed like
  the coalescer. Nothing is written to disk.

### FUSE changes

One concept, a **texture kind** (`Ortho` or `Lit`), carried by the parsed
filename:

- The DDS filename pattern accepts an optional `_LIT` before `.dds`, and
  `DdsFilename` gains a `kind`. `Display` writes the suffix back, so names
  round-trip.
- Virtual inodes carry a kind bit, so a lit texture and its ortho texture
  never share an inode (R8).
- `lookup`, `getattr`, `open` and `read` route lit inodes to the
  `LitTextureSource` (phase 1: `BlackLitSource`; phase 2a: `NightLitSource`; phase 2b's `LitTextureService` replaces it). Ortho inodes take the existing path unchanged.
- A real `*_LIT.dds` file on disk in a package still wins: real files
  resolve before virtual ones, as today.
- Lit sizes are read once: the texture setting is read at start and held
  for the session, so a lit file's size never changes while the mount is
  up and the kernel cannot serve a stale one. A change to the setting
  takes effect the next time XEL starts.
- The single global DDS size (`EXPECTED_DDS_SIZE`) and the placeholder
  validator stay ortho-only. Lit sizes come from the service.

### Publisher (R1 to R3)

- `Ortho4XPProcessor` looks for `<source>/yOrtho4XP_Night/<block>/<tile>.oxn`
  for each tile it scans and records it on the tile (`TileInfo`).
- `process()` copies each recipe to `<package>/night/<block>/<tile>.oxn`.
  It checks the magic and the version digit only, since the publisher does
  not link the runtime, and skips a file that fails with a warning.
- A recipe with no matching tile is reported and not copied.
- A source without `yOrtho4XP_Night/` publishes exactly as today.
- `.ter` files are already copied byte for byte, and the dedupe module only
  reads them, so lit lines survive both (R3).
- Archiving already includes every folder in the package. On an XEL
  version since 0.3.0, the union index's root allowlist hides `night/`, but
  that version cannot serve the lit textures: it answers ENOENT for every
  `_LIT.dds` an OXO night `.ter` names. Night packages therefore must not
  reach a library a pre-0.5.0 client reads (see Decisions).

### Configuration (R20)

```ini
[night]
; Generate night lighting where packages carry night recipes.
enabled = true
; Overall lit brightness, 0.5 to 1.5. A development setting.
brightness = 1.0
```

With `enabled = false`, every lit texture is the minimal black DDS. The
`.ter` files' `LUMINANCE` is fixed by OXO at build time and is not XEL's
to change.

## Error Handling

**X-Plane never sees an error for a `_LIT.dds` name** (R4). Every failure
degrades to black, at the narrowest scope it affects (R19):

| Failure | Black for | Logged |
|---|---|---|
| No X-Plane installation found, or `library.txt` missing or unreadable | Every lit texture this session | Once |
| A palette name that does not resolve, a `.ter` without a lit texture, a missing or undecodable lit file | That class only | Once per name |
| A recipe rejected by the reading rules, or from a newer major version | That 1° tile | Once per file |
| A recipe that cannot be read (transient I/O) | That tile, for the session once a size was answered | Once |
| No recipe for a tile | That tile | Never (normal for countryside) |
| Generation or encoding fails | That texture, not cached | Every time, as a warning |
| `X-Plane.prf` unreadable | Nothing: the full setting is assumed | Once |

**Size invariant (R7).** `getattr` and `read` always agree. If generation
fails after a full size was reported, the read returns a full-size black
texture (zero blocks under the announced header), never the 152-byte file.

## Phases

**Phase 1: packaging and black serving.** Recipe reader, `RecipeStore`,
publisher, FUSE texture kind, and a `LitTextureService` that always serves
the minimal black DDS. This satisfies R1 to R8 and makes OXO night
packages publishable: X-Plane finds every lit texture the `.ter` files
name, and they draw nothing.

**Phase 2: the generator.** `LaminarArt`, `XPlanePrefs`, the layout's
blank levels, `LitGenerator` and its CPU implementation, the lit cache, and
the tuning flight that sets the gain constants and confirms `LUMINANCE 150`.

## Testing

- **Conformance.** OXO's three golden recipes are copied into
  `xearthlayer/tests/fixtures/night/` and decoded to exactly the cells the
  recipe format's Conformance section lists. Each reading rule has a
  negative test: bad magic, version mismatch, trailing bytes, a short zlib
  stream, an index past the palette.
- **Units, with test doubles at every boundary.** Footprint arithmetic at
  tile corners and across 1° edges; `library.txt` resolution against a
  synthetic tree with aliases and region blocks; decoding a known BC1
  block; the gain curve; the blank-level count for each setting from 0 to
  4; the layout's zero-region reads at every boundary.
- **Generator properties.** The same input gives identical bytes;
  adjacent footprints match along their shared edge at the first visible
  level; an empty footprint gives the minimal black DDS; blank levels are
  all zero and the next level is not.
- **FUSE integration.** `stat` and a full read agree on size; a lit
  texture and the ortho texture with the same row, column and zoom have
  different inodes and different bytes; `night/` does not appear in a
  listing.
- **Local tests against a real installation**, skipped when none is
  present: every urban and industrial terrain name in the stock
  `library.txt` resolves to one of the nine lit textures.
- **Acceptance in the simulator**: the scenarios below, plus one tuning
  flight.

## Acceptance Criteria

```gherkin
Feature: Night lighting on ortho scenery

  Scenario: Recipes are packaged and hidden
    Given an OXO job's output with night recipes
    When it is published and the package is installed
    Then the package contains night/<block>/<tile>.oxn for each tile
    And no recipe file is visible through the mount

  Scenario: A lit texture over a city
    Given a footprint over a city with a recipe
    And the texture quality setting is 4
    When X-Plane reads its _LIT.dds
    Then the file is a DXT1 DDS with a 2048 px top level
    And the top level is all zero
    And the next level has non-zero content
    And stat and the read agree on size

  Scenario: Countryside and missing recipes are cheap
    Given a footprint with no lit class, or no recipe
    When X-Plane reads its _LIT.dds
    Then it receives the 152-byte minimal black DDS

  Scenario: The fade follows the texture setting
    Given the texture quality setting is 2
    When X-Plane reads a city footprint's _LIT.dds
    Then three top levels are blank

  Scenario: Deterministic and seamless
    Given two adjacent city footprints
    When each is generated twice
    Then each generation gives identical bytes
    And the two join without a seam at their first visible level

  Scenario: Missing art degrades to black
    Given the installation's urban_FAR folder is renamed
    When X-Plane reads any _LIT.dds
    Then it receives a black texture
    And XEL logs the cause once

  Scenario: In the simulator
    Given a night flight over a lit city on generated textures
    Then city light appears beyond the near field without visible tiling
    And it fades in through dusk and is absent at midday
    And lowering texture quality keeps the near field dark
```

## Requirements Traceability

| Requirement | Where |
|---|---|
| R1, R3 | Publisher |
| R2 | Publisher; union index root allowlist |
| R4 | Error handling; `LitTextureService` |
| R5 | Footprint |
| R6 | Lit DDS layout (minimal black DDS) |
| R7 | Footprint `has_light`; size invariant |
| R8 | Separate lit path; texture kind; lit coalescer and cache |
| R9 | LaminarArt |
| R10 | Gain curve |
| R11, R12 | Stochastic tiling; class weighting |
| R13 | Determinism |
| R14 | Texel density decision (fixed at 1024 px, not configurable) |
| R15, R16 | Lit DDS layout; XPlanePrefs |
| R17 | `LitGenerator` trait; lit CPU budget |
| R18 | RecipeStore and LaminarArt caches |
| R19 | Error handling |
| R20 | Configuration |
| R21 | Attribution |

R14 departs from the requirements on purpose: density is fixed rather than
configurable, at 1024 px instead of 2048 px, by the project owner's
decision on video memory.

## Attribution

The brightness layer comes from NASA's VIIRS annual night lights. XEL's
user documentation credits it as **NASA Black Marble VNP46A4/VJ146A4 annual
nighttime lights** (R21). The lit patterns are Laminar Research's, read
from the user's own X-Plane installation and never redistributed.

## Open Questions

- The gain constants and `LUMINANCE 150`: set on the tuning flight.
- Whether X-Plane accepts a 32768 px header at texture setting 0 (see the
  layout's risk).
- Whether X-Plane re-reads lit textures after a mid-session change to the
  texture setting, which decides whether a watching
  `TextureSettingSource` is worth building.
- Whether `night.brightness` survives to release or becomes a constant.
- Publishing night packages: staging library only until XEL 0.5.0 is the minimum client; whether to also bump the package spec version.

## Out of Scope

- Changes to OXO or the recipe format.
- Lighting outside Laminar's urban and industrial classes from VIIRS
  alone.
- A GPU lit generator (the trait leaves room for one).
- Following VR's texture setting.
