//! Palette names to the user's Laminar lit terrain.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tracing::{debug, warn};

use super::library::LibraryIndex;
use super::ter::{parse_lit_terrain, LitTerrain};

/// The lit art of one X-Plane installation (`1000 world terrain`).
pub struct LaminarArt {
    terrain_root: PathBuf,
    library: OnceLock<Option<LibraryIndex>>,
    resolved: DashMap<String, Option<Arc<LitTerrain>>>,
}

impl LaminarArt {
    /// `terrain_root` is `<install>/Resources/default scenery/1000 world terrain`.
    pub fn new(terrain_root: PathBuf) -> Self {
        Self {
            terrain_root,
            library: OnceLock::new(),
            resolved: DashMap::new(),
        }
    }

    /// The lit terrain a recipe palette name stands for, or `None` (dark).
    ///
    /// Blocking file I/O on first use of a name: call from a blocking context.
    pub fn lit_terrain(&self, palette_name: &str) -> Option<Arc<LitTerrain>> {
        if let Some(hit) = self.resolved.get(palette_name) {
            return hit.clone();
        }
        let result = self.resolve(palette_name).map(Arc::new);
        self.resolved
            .insert(palette_name.to_string(), result.clone());
        result
    }

    fn library(&self) -> Option<&LibraryIndex> {
        self.library
            .get_or_init(|| {
                let path = self.terrain_root.join("library.txt");
                match std::fs::read_to_string(&path) {
                    Ok(text) => Some(LibraryIndex::parse(&text)),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e,
                              "Night lighting: Laminar library unavailable; every lit texture stays dark");
                        None
                    }
                }
            })
            .as_ref()
    }

    fn resolve(&self, palette_name: &str) -> Option<LitTerrain> {
        let file = self.library()?.resolve(palette_name)?;
        let ter_path = self.terrain_root.join(file);
        let text = std::fs::read_to_string(&ter_path).ok()?;
        let lit = parse_lit_terrain(&text, ter_path.parent()?);
        match lit {
            Some(lit) if lit.texture.is_file() => Some(lit),
            Some(lit) => {
                warn!(name = palette_name, texture = %lit.texture.display(),
                      "Night lighting: lit texture missing; this class stays dark");
                None
            }
            None => {
                debug!(
                    name = palette_name,
                    "Night lighting: terrain has no lit texture"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn install(root: &Path, with_lit_file: bool) {
        std::fs::create_dir_all(root.join("terrain12")).unwrap();
        std::fs::create_dir_all(root.join("textures12/urban_FAR")).unwrap();
        std::fs::write(
            root.join("library.txt"),
            "A\n1200\nLIBRARY\nREGION_DEFINE all\nREGION_RECT -180 -90 179 89\nREGION all\n\
             EXPORT lib/g10/terrain10/urban_tmp_sdry3.ter terrain12/urban_tmp_sdry3.ter\n\
             EXPORT lib/g10/terrain10/urban_tmp_sdry_prk.ter terrain12/park_tmp_sdry1.ter\n",
        )
        .unwrap();
        std::fs::write(
            root.join("terrain12/urban_tmp_sdry3.ter"),
            "TEXTURE_LIT ../textures12/urban_FAR/uw5_FAR_LIT.dds\nLIT_FADE_PARAMS 3072 7000 15000\n",
        )
        .unwrap();
        std::fs::write(
            root.join("terrain12/park_tmp_sdry1.ter"),
            "BASE_TEX park.dds\n",
        )
        .unwrap();
        if with_lit_file {
            std::fs::write(root.join("textures12/urban_FAR/uw5_FAR_LIT.dds"), b"DDS ").unwrap();
        }
    }

    #[test]
    fn an_urban_name_resolves_to_its_lit_terrain() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        let lit = art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .unwrap();
        assert_eq!(lit.ground_scale_m, 3072.0);
        assert!(lit.texture.is_file());
    }

    #[test]
    fn a_park_and_an_unknown_name_are_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), true);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry_prk.ter")
            .is_none());
        assert!(art.lit_terrain("lib/g10/terrain10/nope.ter").is_none());
    }

    #[test]
    fn a_missing_lit_file_is_dark() {
        let temp = TempDir::new().unwrap();
        install(temp.path(), false);
        let art = LaminarArt::new(temp.path().to_path_buf());
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
    }

    #[test]
    fn no_installation_makes_everything_dark() {
        let art = LaminarArt::new(PathBuf::from("/nonexistent/1000 world terrain"));
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .is_none());
    }

    /// Against the developer's real installation, skipped when absent: every
    /// urban and industrial export resolves; parks and golf stay dark.
    #[test]
    fn the_reference_installation_resolves_its_urban_terrain() {
        let root = PathBuf::from(
            "/media/FlightSim/X-Plane 12-beta/Resources/default scenery/1000 world terrain",
        );
        if !root.join("library.txt").is_file() {
            return;
        }
        let art = LaminarArt::new(root);
        let lit = art
            .lit_terrain("lib/g10/terrain10/urban_tmp_sdry3.ter")
            .unwrap();
        assert!(lit
            .texture
            .ends_with("textures12/urban_FAR/uw5_FAR_LIT.dds"));
        assert_eq!(lit.ground_scale_m, 4096.0);
        assert!(art
            .lit_terrain("lib/g10/terrain10/urban_cld_dry_prk.ter")
            .is_none());
    }
}
