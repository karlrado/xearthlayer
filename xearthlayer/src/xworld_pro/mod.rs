//! SimHeaven X-World Pro: what is installed and enabled, read from X-Plane's
//! `scenery_packs.ini` and the Custom Scenery folders (copied or symlinked
//! alike). XEarthLayer only reads; it never writes `scenery_packs.ini`,
//! X-World Pro's files or X-Plane's `Resources`.

mod detect;
mod packs;

pub use detect::{detect, Continent, Detection, XwpLayer, XwpLibrary, LAYER_PREFIX, LIBRARY_NAME};
pub use packs::{parse_scenery_packs, PackEntry, SCENERY_PACKS_FILE};
