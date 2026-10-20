//! Night lighting: lit textures generated from OXO night recipes.
//!
//! See `docs/dev/night-lighting-design.md`. Phase 1 reads recipes and
//! serves a minimal black texture for every lit name. Phase 2a answers lit
//! `stat`/`open` with the real file size (footprints from recipes, lit art
//! from the user's Laminar installation, texture setting from the preferences)
//! but still serves black pixels. Phase 2b generates the light, in the
//! `generator` module behind the `LitGenerator` trait.

pub mod disk;
pub mod footprint;
pub mod generator;
pub mod laminar;
pub mod layout;
pub mod prefs;
pub mod recipe;
pub mod service;
pub mod store;

pub use disk::{
    decode_entry, encode_entry, footprint_fingerprint, lit_cache_key, session_fingerprint,
    LitFingerprint,
};
pub use footprint::Footprint;
pub use generator::{
    CpuLitGenerator, LedModel, LedRegion, LitGenerator, LitRequest, LIT_GENERATOR_VERSION,
};
pub use laminar::{LaminarArt, LitPattern, LitTerrain};
pub use layout::{minimal_black_dds, LitLayout};
pub use prefs::{
    FixedTextureSetting, StartupTextureSetting, TextureSettingSource, FULL_TEXTURE_SETTING,
};
pub use recipe::{parse_recipe, BrightnessScale, Grid, Recipe, RecipeError, SUPPORTED_MAJOR};
pub use service::{BlackLitSource, LitTextureSource, NightLitSource, DEFAULT_NIGHT_BRIGHTNESS};
pub use store::{block_name, recipe_path, tile_name, FileIdentity, RecipeStore};
