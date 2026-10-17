//! Finds, loads and caches night recipes for 1 degree tiles.
//!
//! The union index hides a package's `night/` folder from X-Plane, so the
//! store reads recipes through each ortho source's own path, in the union's
//! priority order (patches before regional packages).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashSet;
use moka::sync::Cache;
use tracing::warn;

use super::recipe::{parse_recipe, Recipe};
use crate::ortho_union::OrthoUnionIndex;

/// Byte budget for decoded recipes (about 1.5 MB each).
const RECIPE_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Larger files are rejected unread. Real recipes are tens of kilobytes.
const MAX_RECIPE_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub use xearthlayer_package::night::{block_name, tile_name};

/// Where a source keeps a tile's recipe: `<root>/night/<block>/<tile>.oxn`.
pub fn recipe_path(root: &Path, lat: i32, lon: i32) -> PathBuf {
    root.join(xearthlayer_package::night::PACKAGE_DIR)
        .join(xearthlayer_package::night::recipe_relative_path(lat, lon))
}

/// Recipes per tile, loaded lazily and evicted by size. Absence is cached.
pub struct RecipeStore {
    roots: Vec<PathBuf>,
    cache: Cache<(i32, i32), Option<Arc<Recipe>>>,
    warned: DashSet<PathBuf>,
}

impl RecipeStore {
    /// `roots` in priority order: the first source with a recipe wins.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let cache = Cache::builder()
            .max_capacity(RECIPE_CACHE_BYTES)
            .weigher(|_, v: &Option<Arc<Recipe>>| match v {
                Some(r) => (r.class.cells.len() + r.brightness.cells.len())
                    .try_into()
                    .unwrap_or(u32::MAX),
                None => 1,
            })
            .build();
        Self {
            roots,
            cache,
            warned: DashSet::new(),
        }
    }

    /// Every source of the union index, in its priority order.
    pub fn from_index(index: &OrthoUnionIndex) -> Self {
        Self::new(
            index
                .sources()
                .iter()
                .map(|s| s.source_path.clone())
                .collect(),
        )
    }

    /// The recipe for the tile whose south-west corner is `(lat, lon)`.
    ///
    /// Blocking file I/O on a cache miss: call from a blocking context.
    /// A transient I/O failure is not cached, so the next call retries.
    pub fn get(&self, lat: i32, lon: i32) -> Option<Arc<Recipe>> {
        self.cache
            .try_get_with((lat, lon), || self.load(lat, lon))
            .unwrap_or(None)
    }

    /// `Ok(None)` (cached) for an absent or rejected recipe; `Err` (not
    /// cached) when the file could not be read.
    fn load(&self, lat: i32, lon: i32) -> Result<Option<Arc<Recipe>>, std::io::Error> {
        let Some(path) = self
            .roots
            .iter()
            .map(|root| recipe_path(root, lat, lon))
            .find(|p| p.is_file())
        else {
            return Ok(None);
        };
        let bytes = match read_bounded(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.warn_once(&path, "file exceeds the recipe size ceiling");
                return Ok(None);
            }
            Err(e) => {
                self.warn_once(&path, &e.to_string());
                return Err(e);
            }
        };
        match parse_recipe(&bytes) {
            Ok(recipe) => Ok(Some(Arc::new(recipe))),
            Err(e) => {
                self.warn_once(&path, &e.to_string());
                Ok(None)
            }
        }
    }

    fn warn_once(&self, path: &Path, reason: &str) {
        if self.warned.insert(path.to_path_buf()) {
            warn!(path = %path.display(), %reason, "Night recipe unusable; tile stays dark");
        }
    }
}

/// The file's bytes, or `None` when it is larger than the ceiling. Reads at
/// most one byte past the ceiling, whatever the file claims.
fn read_bounded(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_RECIPE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_RECIPE_FILE_BYTES).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const URBAN: &[u8] = include_bytes!("../../tests/fixtures/night/urban.oxn");
    const EMPTY: &[u8] = include_bytes!("../../tests/fixtures/night/empty.oxn");

    fn put(root: &Path, lat: i32, lon: i32, bytes: &[u8]) {
        let path = recipe_path(root, lat, lon);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn names_follow_ortho4xp() {
        assert_eq!(tile_name(37, -123), "+37-123");
        assert_eq!(block_name(37, -123), "+30-130");
        assert_eq!(tile_name(-12, 45), "-12+045");
        assert_eq!(block_name(-12, 45), "-20+040");
        assert_eq!(
            recipe_path(Path::new("/pkg"), 37, -123),
            PathBuf::from("/pkg/night/+30-130/+37-123.oxn")
        );
    }

    #[test]
    fn finds_a_recipe_in_a_source() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert_eq!(store.get(37, -123).unwrap().tile, "+37-123");
    }

    #[test]
    fn a_tile_without_a_recipe_is_none() {
        let temp = TempDir::new().unwrap();
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[test]
    fn the_first_source_in_priority_order_wins() {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        put(a.path(), 37, -123, EMPTY);
        put(b.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![a.path().to_path_buf(), b.path().to_path_buf()]);
        assert_eq!(
            store.get(37, -123).unwrap().palette.len(),
            1,
            "empty.oxn from the first source"
        );
    }

    #[test]
    fn a_rejected_recipe_is_a_dark_tile() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, b"not a recipe");
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[test]
    fn a_file_over_the_ceiling_is_rejected_unread() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(recipe_path(temp.path(), 37, -123))
            .unwrap();
        file.set_len(MAX_RECIPE_FILE_BYTES + 1).unwrap(); // sparse
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn an_io_error_is_not_cached() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let path = recipe_path(temp.path(), 37, -123);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_none(), "unreadable now");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.get(37, -123).is_some(), "the failure was not cached");
    }

    #[test]
    fn results_are_cached_including_absence() {
        let temp = TempDir::new().unwrap();
        put(temp.path(), 37, -123, URBAN);
        let store = RecipeStore::new(vec![temp.path().to_path_buf()]);
        assert!(store.get(37, -123).is_some());
        assert!(store.get(38, -123).is_none());
        std::fs::remove_file(recipe_path(temp.path(), 37, -123)).unwrap();
        put(temp.path(), 38, -123, URBAN);
        assert!(store.get(37, -123).is_some(), "served from cache");
        assert!(store.get(38, -123).is_none(), "absence is cached too");
    }

    /// R2: the union index never shows `night/` to X-Plane, yet the store,
    /// reading through the source path, still finds the recipe.
    #[test]
    fn the_union_index_hides_night_but_from_index_finds_it() {
        use crate::ortho_union::OrthoUnionIndexBuilder;
        use semver::Version;
        use xearthlayer_package::{InstalledPackage, Package, PackageType};

        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+30-130")).unwrap();
        put(&pkg_dir, 37, -123, URBAN);
        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        assert!(!index.is_directory(Path::new("night")));
        assert!(index
            .resolve(Path::new("night/+30-130/+37-123.oxn"))
            .is_none());
        assert_eq!(
            RecipeStore::from_index(&index).get(37, -123).unwrap().tile,
            "+37-123"
        );
    }
}
