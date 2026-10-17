//! Night lighting: lit textures generated from OXO night recipes.
//!
//! See `docs/dev/night-lighting-design.md`. Phase 1 reads recipes and
//! serves a minimal black texture for every lit name; phase 2 generates
//! light from the recipes and the user's Laminar lit art.

pub mod laminar;
pub mod layout;
pub mod prefs;
pub mod recipe;
pub mod service;
pub mod store;

pub use laminar::{LaminarArt, LitTerrain};
pub use layout::minimal_black_dds;
pub use prefs::{
    FixedTextureSetting, StartupTextureSetting, TextureSettingSource, FULL_TEXTURE_SETTING,
};
pub use recipe::{parse_recipe, BrightnessScale, Grid, Recipe, RecipeError, SUPPORTED_MAJOR};
pub use service::{BlackLitSource, LitTextureSource};
pub use store::{block_name, recipe_path, tile_name, RecipeStore};
