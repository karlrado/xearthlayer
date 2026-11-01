# Coordinate System Implementation

This document describes the coordinate conversion system implemented in XEarthLayer.

## Overview

XEarthLayer uses the **Web Mercator** (also called Slippy Map) projection system for converting between geographic coordinates (latitude/longitude) and tile coordinates used by satellite imagery providers.

The system is designed to be compatible with **AutoOrtho** and **Ortho4XP** scenery packages for X-Plane, which use a specific filename format based on Web Mercator tile indices.

## Coordinate Types

### Geographic Coordinates

Standard latitude/longitude in decimal degrees:
- **Latitude**: -85.05112878° to 85.05112878° (Web Mercator limits)
- **Longitude**: -180° to 180°

### Tile Coordinates

```rust
pub struct TileCoord {
    pub row: u32,    // Y coordinate (north-south), 0 at north
    pub col: u32,    // X coordinate (east-west), 0 at west
    pub zoom: u8,    // Zoom level (0-22)
}
```

At each zoom level:
- **Zoom 0**: 1×1 tile (entire world)
- **Zoom 1**: 2×2 tiles
- **Zoom n**: 2^n × 2^n tiles

Each tile represents a 4096×4096 pixel image composed of 16×16 chunks.

### Chunk Coordinates

```rust
pub struct ChunkCoord {
    pub tile_row: u32,
    pub tile_col: u32,
    pub chunk_row: u8,    // 0-15 within tile
    pub chunk_col: u8,    // 0-15 within tile
    pub zoom: u8,
}
```

Chunks are 256×256 pixel sub-tiles used for parallel downloading. Each tile contains 16×16 = 256 chunks.

### Zoom Level Semantics

XEarthLayer uses **chunk zoom** (like Ortho4XP) in user-facing APIs:

| Concept | Zoom Range | Description |
|---------|------------|-------------|
| **Chunk Zoom** | 12-22 | User-specified zoom level (Ortho4XP style) |
| **Tile Zoom** | 8-18 | Internal tile coordinates (chunk_zoom - 4) |

The relationship: `tile_zoom = chunk_zoom - 4` (because each tile = 16×16 chunks, and 16 = 2^4)

**Example:**
- User requests zoom 18 (chunk zoom)
- System uses zoom 14 for tile coordinates
- Downloads 256 chunks at zoom 18 to compose the tile

### Quadkeys

Quadkeys are Bing Maps' tile naming scheme using base-4 encoding:
- Each digit (0-3) represents which quadrant a tile occupies at each zoom level
- Quadkey length equals the zoom level
- Zoom 0 has an empty string quadkey

**Quadrant Encoding:**
- `0` = top-left (northwest)
- `1` = top-right (northeast)
- `2` = bottom-left (southwest)
- `3` = bottom-right (southeast)

**Example:** Tile (col=3, row=5, zoom=3) → quadkey `"213"`

## AutoOrtho/Ortho4XP Filename Format

XEarthLayer uses the same DDS filename format as AutoOrtho and Ortho4XP:

```
{row}_{col}_{maptype}{zoom}.dds
```

**Components:**
- `row`: Unsigned Web Mercator Y coordinate (increases southward)
- `col`: Unsigned Web Mercator X coordinate (increases eastward)
- `maptype`: Provider identifier (e.g., "BI" for Bing, "GO" for Google)
- `zoom`: Two-digit chunk zoom level

**Real-World Examples from AutoOrtho:**

| Region | Filename | Row | Col | Lat/Lon (center) |
|--------|----------|-----|-----|------------------|
| Portugal | `100000_125184_BI18.dds` | 100000 | 125184 | 39.19°N, 8.07°W |
| New Zealand | `169840_253472_BI18.dds` | 169840 | 253472 | 46.91°S, 168.10°E |
| Korea | `100000_222560_BI18.dds` | 100000 | 222560 | 39.19°N, 125.65°E |
| Caribbean | `116208_75824_BI18.dds` | 116208 | 75824 | 19.98°N, 75.86°W |

**Coordinate Ranges at Zoom 18:**
- Row: 0 (north pole) to 262143 (south pole)
- Col: 0 (180°W) to 262143 (180°E)
- Equator: row ≈ 131072
- Prime Meridian: col ≈ 131072

### DDS Filename Parsing

```rust
pub struct DdsFilename {
    pub row: u32,        // Web Mercator row
    pub col: u32,        // Web Mercator column
    pub zoom: u8,        // Chunk zoom level
    pub map_type: String // Provider identifier (e.g., "BI")
}

pub fn parse_dds_filename(filename: &str) -> Result<DdsFilename, ParseError>
```

**Example:**
```rust
use xearthlayer::vfs::parse_dds_filename;

let coords = parse_dds_filename("100000_125184_BI18.dds")?;
assert_eq!(coords.row, 100000);
assert_eq!(coords.col, 125184);
assert_eq!(coords.zoom, 18);
assert_eq!(coords.map_type, "BI");
```

## Conversion Functions

### Geographic to Tile Coordinates

```rust
pub fn to_tile_coords(lat: f64, lon: f64, zoom: u8) -> Result<TileCoord, CoordError>
```

**Formula (Web Mercator):**
```
n = 2^zoom
col = floor((lon + 180) / 360 * n)
lat_rad = lat * π / 180
row = floor((1 - asinh(tan(lat_rad)) / π) / 2 * n)
```

**Example:**
```rust
let tile = to_tile_coords(40.7128, -74.0060, 16)?;
// Result: TileCoord { row: 24640, col: 19295, zoom: 16 }
```

**Key Property:** Any lat/lon coordinate within a tile's boundaries will return the same tile coordinates. This ensures that regardless of where in a tile the user clicks, they get the correct tile.

### Tile to Geographic Coordinates (Northwest Corner)

```rust
pub fn tile_to_lat_lon(tile: &TileCoord) -> (f64, f64)
```

Returns the **northwest corner** of the tile.

**Formula (Inverse Web Mercator):**
```
n = 2^zoom
lon = col / n * 360 - 180
y = row / n
lat_rad = atan(sinh(π * (1 - 2*y)))
lat = lat_rad * 180 / π
```

**Example:**
```rust
let tile = TileCoord { row: 24640, col: 19295, zoom: 16 };
let (lat, lon) = tile_to_lat_lon(&tile);
// Result: approximately (40.713, -74.007)
```

### Tile to Geographic Coordinates (Center)

```rust
pub fn tile_to_lat_lon_center(tile: &TileCoord) -> (f64, f64)
```

Returns the **center point** of the tile. This is useful for displaying human-readable coordinates and matches the `LOAD_CENTER` values in AutoOrtho `.ter` files.

**Formula:**
```
n = 2^zoom
lon = (col + 0.5) / n * 360 - 180
y = (row + 0.5) / n
lat_rad = atan(sinh(π * (1 - 2*y)))
lat = lat_rad * 180 / π
```

**Example:**
```rust
use xearthlayer::coord::{TileCoord, tile_to_lat_lon_center};

// AutoOrtho Europe tile
let tile = TileCoord { row: 100000, col: 125184, zoom: 18 };
let (lat, lon) = tile_to_lat_lon_center(&tile);
// Result: approximately (39.19, -8.07) - matches LOAD_CENTER in .ter file
```

### Geographic to Chunk Coordinates

```rust
pub fn to_chunk_coords(lat: f64, lon: f64, zoom: u8) -> Result<ChunkCoord, CoordError>
```

Directly converts geographic coordinates to a specific chunk within a tile. This is useful for determining which 256×256 pixel chunk to download.

**Algorithm:**
1. First calculates the tile coordinates
2. Calculates position at chunk resolution (zoom + 4, since 2^4 = 16)
3. Extracts chunk position within tile using modulo 16

**Example:**
```rust
let chunk = to_chunk_coords(40.7128, -74.0060, 16)?;
// Result: ChunkCoord {
//   tile_row: 24640, tile_col: 19295,
//   chunk_row: 5, chunk_col: 12,
//   zoom: 16
// }
```

### Chunk to Global Coordinates

```rust
impl ChunkCoord {
    pub fn to_global_coords(&self) -> (u32, u32, u8)
}
```

Converts chunk coordinates to global tile coordinates at chunk resolution. This is used when requesting chunks from satellite imagery providers.

**Formula:**
```
global_row = tile_row * 16 + chunk_row
global_col = tile_col * 16 + chunk_col
global_zoom = tile_zoom + 4
```

**Example:**
```rust
let chunk = ChunkCoord {
    tile_row: 100, tile_col: 200,
    chunk_row: 5, chunk_col: 7,
    zoom: 10
};
let (global_row, global_col, zoom) = chunk.to_global_coords();
// Result: (1605, 3207, 14)
```

### Iterating Tile Chunks

```rust
impl TileCoord {
    pub fn chunks(&self) -> TileChunksIterator
}
```

Returns an iterator over all 256 chunks in a tile, yielded in row-major order.

**Example:**
```rust
let tile = TileCoord { row: 100, col: 200, zoom: 12 };
for chunk in tile.chunks() {
    // Process each 256×256 chunk
    let (global_row, global_col, zoom) = chunk.to_global_coords();
    download_chunk(global_row, global_col, zoom)?;
}
```

### Tile to Quadkey

```rust
pub fn tile_to_quadkey(tile: &TileCoord) -> String
```

Converts tile coordinates to a Bing Maps quadkey string.

**Algorithm:**
- For each zoom level from zoom down to 1:
  - Check if column bit is set (east/west)
  - Check if row bit is set (north/south)
  - Combine into digit 0-3
  - Append to quadkey string

**Example:**
```rust
let tile = TileCoord { row: 5, col: 3, zoom: 3 };
let quadkey = tile_to_quadkey(&tile);
// Result: "213"
```

### Quadkey to Tile

```rust
pub fn quadkey_to_tile(quadkey: &str) -> Result<TileCoord, CoordError>
```

Parses a Bing Maps quadkey string back to tile coordinates.

**Algorithm:**
- For each character in quadkey (left to right):
  - Shift row and col left by 1 bit
  - If digit & 1, set col bit (east)
  - If digit & 2, set row bit (south)
- Zoom level = quadkey length

**Example:**
```rust
let tile = quadkey_to_tile("213")?;
// Result: TileCoord { row: 5, col: 3, zoom: 3 }
```

## Error Handling

```rust
pub enum CoordError {
    InvalidLatitude(f64),   // Outside ±85.05112878°
    InvalidLongitude(f64),  // Outside ±180°
    InvalidZoom(u8),        // Outside 0-18
    InvalidQuadkey(String), // Invalid characters or too long
}
```

All conversion functions validate inputs and return descriptive errors:
- Geographic coordinates validated against Web Mercator limits
- Zoom levels validated against 0-18 range
- Quadkeys validated for:
  - Only digits 0-3
  - Maximum length of 18 (MAX_ZOOM)

## Precision and Tolerance

At different zoom levels, tiles cover different geographic areas:

| Zoom | Tile Size (degrees) | Tile Size (~km at equator) |
|------|--------------------|-----------------------------|
| 0    | 360°               | ~40,000 km                  |
| 5    | 11.25°             | ~1,250 km                   |
| 10   | 0.35°              | ~39 km                      |
| 15   | 0.011°             | ~1.2 km                     |
| 16   | 0.0055°            | ~600 m                      |
| 18   | 0.0014°            | ~150 m                      |

**Roundtrip Precision**: When converting lat/lon → tile → lat/lon, the result will be within one tile's worth of the original coordinate. This is expected because `tile_to_lat_lon` returns the northwest corner of the tile. Use `tile_to_lat_lon_center` for a more accurate center point.

## CLI Usage

The XEarthLayer CLI accepts lat/lon coordinates and automatically converts them to Web Mercator tile coordinates:

```bash
# Download a tile containing the given coordinates
xearthlayer download --lat 39.18969 --lon=-8.07495 --zoom 18 --output tile.dds

# The system:
# 1. Converts lat/lon to tile coordinates at tile_zoom (18-4=14)
# 2. Downloads 256 chunks at chunk_zoom (18)
# 3. Assembles into 4096×4096 DDS texture
```

**Any coordinate within a tile produces the same tile:**
```bash
# These all download the same tile (100000_125184 at zoom 18):
xearthlayer download --lat 39.189 --lon=-8.075 --zoom 18 --output tile.dds
xearthlayer download --lat 39.190 --lon=-8.074 --zoom 18 --output tile.dds
xearthlayer download --lat 39.188 --lon=-8.076 --zoom 18 --output tile.dds
```

## Testing

### Manual Tests (51 tests)

**Geographic/Tile Conversions:**
- Known coordinates (NYC, London)
- Edge cases (equator, prime meridian)
- Invalid input rejection
- Roundtrip conversions at multiple zoom levels

**Tile Center Conversions:**
- AutoOrtho Europe coordinates (Portugal)
- AutoOrtho Australia coordinates (New Zealand)
- AutoOrtho Asia coordinates (Korea)

**Chunk Conversions:**
- Basic chunk coordinate calculation
- Chunk to global coordinate conversion
- Chunk positions at tile boundaries (origin and max)
- Consistency between tile and chunk conversions
- Iterator count, order, and uniqueness (256 chunks per tile)

**Quadkey Conversions:**
- Zoom level 0 and 1 conversions
- Bing Maps official examples
- Invalid character/length rejection
- Roundtrip tile ↔ quadkey conversions

**DDS Filename Parsing:**
- AutoOrtho Europe/Australia/Asia/South America filenames
- Various zoom levels (12-22)
- Provider identifiers (BI, GO)
- Case normalization
- Invalid patterns and overflow handling

### Property-Based Tests (18 tests)

Using `proptest`, we verify mathematical properties with 100 random cases each:

**Tile Conversions:**
- **Roundtrip property**: lat/lon → tile → lat/lon within tile precision
- **Bounds property**: Generated tiles always in valid range
- **Monotonicity**: Increasing longitude increases column coordinate
- **Reverse bounds**: Tile conversions always return valid coordinates
- **Validation**: Invalid inputs properly rejected

**Chunk Conversions:**
- Chunk coordinates always in valid range (0-15)
- Chunk tile coordinates match direct tile conversion
- Global coordinate calculation correctness
- Iterator always yields exactly 256 chunks
- All iterator chunks have valid coordinates
- Iterator produces no duplicates

**Quadkey Conversions:**
- Roundtrip property: tile → quadkey → tile is identity
- Quadkey length always equals zoom level
- Quadkeys only contain valid digits (0-3)
- Deterministic output for same tile
- Validation of length limits
- Zoom 0 always produces empty string

**Total Test Coverage:** 51+ tests (manual + property-based)

## Implementation Details

### Why Web Mercator?

Web Mercator is the standard for web mapping because:
- Simple formulas (faster computation)
- Preserves angles (easier navigation)
- Used by all major providers (Google, Bing, OSM)
- Tiles align perfectly at all zoom levels
- Compatible with AutoOrtho/Ortho4XP scenery

### Why ±85.05112878° Latitude Limit?

Web Mercator projects the Earth as a square, which requires cutting off near the poles. The limit is chosen so that the projected map is exactly square.

### Why Unsigned Tile Coordinates?

Web Mercator tile indices are always non-negative:
- Row 0 is at the north pole, increasing southward
- Col 0 is at 180°W, increasing eastward
- This matches the AutoOrtho filename format which uses unsigned integers

### Performance Optimizations

- Functions marked `#[inline]` for better performance
- Uses `powi()` instead of `powf()` for integer exponents
- Direct float operations (no unnecessary conversions)
- Regex pattern compiled once using `OnceLock`

## Usage Examples

### Basic Tile Conversion

```rust
use xearthlayer::coord::{to_tile_coords, tile_to_lat_lon, tile_to_lat_lon_center};

// Convert user's current location to tile
let location_tile = to_tile_coords(40.7128, -74.0060, 16)?;

// Determine which tile contains a specific coordinate
if location_tile.row == some_tile.row && location_tile.col == some_tile.col {
    println!("Coordinates are in the same tile!");
}

// Get the center of a tile (for display)
let (center_lat, center_lon) = tile_to_lat_lon_center(&tile);
println!("Tile center: {}, {}", center_lat, center_lon);

// Get the bounding box of a tile
let (nw_lat, nw_lon) = tile_to_lat_lon(&tile);
let se_tile = TileCoord {
    row: tile.row + 1,
    col: tile.col + 1,
    zoom: tile.zoom
};
let (se_lat, se_lon) = tile_to_lat_lon(&se_tile);
println!("Tile bounds: ({}, {}) to ({}, {})", nw_lat, nw_lon, se_lat, se_lon);
```

### Working with AutoOrtho Filenames

```rust
use xearthlayer::coord::{TileCoord, tile_to_lat_lon_center};
use xearthlayer::vfs::parse_dds_filename;

// Parse an AutoOrtho DDS filename
let coords = parse_dds_filename("100000_125184_BI18.dds")?;

// Convert to TileCoord for further processing
let tile = TileCoord {
    row: coords.row,
    col: coords.col,
    zoom: coords.zoom,
};

// Get the geographic center (matches LOAD_CENTER in .ter file)
let (lat, lon) = tile_to_lat_lon_center(&tile);
println!("Tile center: {:.5}, {:.5}", lat, lon);
// Output: Tile center: 39.18969, -8.07495
```

### Chunk-Based Parallel Downloads

```rust
use xearthlayer::coord::{to_tile_coords, TileCoord};

// Get tile for a location
let tile = to_tile_coords(51.5074, -0.1278, 15)?;

// Download all chunks in parallel
let chunks: Vec<_> = tile.chunks().collect();
for chunk in chunks {
    let (global_row, global_col, zoom) = chunk.to_global_coords();

    // Download from provider using global coordinates
    let url = format!(
        "https://provider.com/{}/{}/{}.jpg",
        zoom, global_col, global_row  // Note: some providers use x/y order
    );
    download_chunk(&url)?;
}
```

### Bing Maps Quadkey Integration

```rust
use xearthlayer::coord::{to_tile_coords, tile_to_quadkey, quadkey_to_tile};

// Convert location to Bing Maps quadkey
let tile = to_tile_coords(47.6062, -122.3321, 18)?; // Seattle
let quadkey = tile_to_quadkey(&tile);

// Use quadkey with Bing Maps API
let url = format!(
    "https://t.ssl.ak.dynamic.tiles.virtualearth.net/comp/ch/{}?mkt=en-US&it=A",
    quadkey
);

// Parse quadkey from API response
let tile = quadkey_to_tile("0231012312")?;
println!("Tile: row={}, col={}, zoom={}", tile.row, tile.col, tile.zoom);
```

## References

- [Slippy Map Tilenames (OSM Wiki)](https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames)
- [Web Mercator Projection (Wikipedia)](https://en.wikipedia.org/wiki/Web_Mercator_projection)
- [Google Maps Tile Coordinates](https://developers.google.com/maps/documentation/javascript/coordinates)
- [Bing Maps Tile System](https://docs.microsoft.com/en-us/bingmaps/articles/bing-maps-tile-system)
- [AutoOrtho GitHub](https://github.com/kubilus1/autoortho)
- [Ortho4XP GitHub](https://github.com/oscarpilote/Ortho4XP)
