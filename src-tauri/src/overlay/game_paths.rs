use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::wad::Mounted;

/// Cheap filename-only index of the game's WAD tree. Walks
/// `<game_path>/DATA/FINAL` iteratively and records the absolute path
/// of every `*.wad.client` keyed by its mount name (lowercased
/// filename with `.wad.client` stripped).
///
/// Crucially this does NOT read any WAD contents - just directory
/// entries and metadata. Building it is quick enough for the hover
/// runtime while avoiding the full WAD parse of `Index::from_game_folder`.
pub struct GamePathIndex {
    pub game_path: PathBuf,
    mounts: HashMap<String, PathBuf>,
}

impl GamePathIndex {
    pub fn build(game_path: &Path) -> Result<Self> {
        let final_dir = game_path.join("DATA").join("FINAL");
        if !final_dir.is_dir() {
            return Err(anyhow!(
                "expected {} to exist - wrong game path?",
                final_dir.display()
            ));
        }
        let mut mounts = HashMap::new();
        walk(&final_dir, &mut mounts)
            .with_context(|| format!("walking {}", final_dir.display()))?;
        Ok(Self {
            game_path: game_path.to_path_buf(),
            mounts,
        })
    }

    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Absolute path of the WAD with the given mount name, if any.
    pub fn find(&self, mount_name: &str) -> Option<&Path> {
        self.mounts.get(mount_name).map(|p| p.as_path())
    }

    /// Iterate `(mount_name, relpath_from_game_root)` pairs.
    pub fn iter_rel(&self) -> impl Iterator<Item = (&str, PathBuf)> + '_ {
        self.mounts.iter().map(|(name, abs)| {
            let rel = abs
                .strip_prefix(&self.game_path)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| abs.clone());
            (name.as_str(), rel)
        })
    }

    /// Iterate only `DATA/FINAL/Maps/Shipping/Map*.wad.client` files,
    /// excluding localized variants such as `Map11.fr_FR.wad.client`.
    pub fn iter_map_shipping_wads(&self) -> impl Iterator<Item = (&str, &Path)> + '_ {
        self.mounts.iter().filter_map(|(name, abs)| {
            let rel = abs.strip_prefix(&self.game_path).ok()?;
            if !is_map_shipping_wad(rel) {
                return None;
            }
            Some((name.as_str(), abs.as_path()))
        })
    }

    pub fn is_map_shipping_wad(&self, abs_path: &Path) -> bool {
        abs_path
            .strip_prefix(&self.game_path)
            .ok()
            .is_some_and(is_map_shipping_wad)
    }
}

fn walk(root: &Path, mounts: &mut HashMap<String, PathBuf>) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }

            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            if file_type.is_dir() {
                if is_reparse_point(&path) {
                    continue;
                }
                // Directory-form WADs exist in mod trees but the game
                // itself only loads file-backed ones; skip descending into
                // those to avoid accidentally mounting them.
                if filename.ends_with(".wad") || filename.ends_with(".wad.client") {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && filename.ends_with(".wad.client") {
                let mount_name = Mounted::make_name(&filename);
                mounts.insert(mount_name, path);
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

    fs::symlink_metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_reparse_point(_path: &Path) -> bool {
    false
}

fn is_map_shipping_wad(relpath: &Path) -> bool {
    let mut comps = relpath.components();
    let matches_prefix = matches!(
        (
            comps.next(),
            comps.next(),
            comps.next(),
            comps.next(),
            comps.next()
        ),
        (
            Some(std::path::Component::Normal(a)),
            Some(std::path::Component::Normal(b)),
            Some(std::path::Component::Normal(c)),
            Some(std::path::Component::Normal(d)),
            Some(std::path::Component::Normal(e))
        ) if os_eq_ignore_ascii_case(a, "DATA")
            && os_eq_ignore_ascii_case(b, "FINAL")
            && os_eq_ignore_ascii_case(c, "Maps")
            && os_eq_ignore_ascii_case(d, "Shipping")
            && is_base_map_wad_filename(e)
    );
    matches_prefix && comps.next().is_none()
}

fn os_eq_ignore_ascii_case(value: &std::ffi::OsStr, expected: &str) -> bool {
    value
        .to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(expected))
}

fn is_base_map_wad_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if !name.starts_with("Map") || !name.ends_with(".wad.client") {
        return false;
    }
    let stem = &name[..name.len() - ".wad.client".len()];
    stem[3..].bytes().all(|b| b.is_ascii_digit())
}
