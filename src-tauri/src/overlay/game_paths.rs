use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::wad::Mounted;

/// Cheap filename-only index of the game's WAD tree. Walks
/// `<game_path>/DATA/FINAL` recursively and records the absolute path
/// of every `*.wad.client` keyed by its mount name (lowercased
/// filename with `.wad.client` stripped).
///
/// Crucially this does NOT read any WAD contents — just `stat`s the
/// tree. Building it is ~200 filesystem ops, which finishes well under
/// a second on any modern disk; compare to `Index::from_game_folder`
/// which reads and parses every WAD and takes tens of seconds.
///
/// Used by the hover runtime to look up just the game WAD(s) a given
/// fantome actually targets, so the full game-index walk can be
/// avoided on the hot path.
pub struct GamePathIndex {
    pub game_path: PathBuf,
    mounts: HashMap<String, PathBuf>,
}

impl GamePathIndex {
    pub fn build(game_path: &Path) -> Result<Self> {
        let final_dir = game_path.join("DATA").join("FINAL");
        if !final_dir.is_dir() {
            return Err(anyhow!(
                "expected {} to exist — wrong game path?",
                final_dir.display()
            ));
        }
        let mut mounts = HashMap::new();
        walk(&final_dir, &mut mounts).with_context(|| {
            format!("walking {}", final_dir.display())
        })?;
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
}

fn walk(dir: &Path, mounts: &mut HashMap<String, PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if file_type.is_dir() {
            // Directory-form WADs exist in mod trees but the game
            // itself only loads file-backed ones; skip descending into
            // those to avoid accidentally mounting them.
            if filename.ends_with(".wad") || filename.ends_with(".wad.client") {
                continue;
            }
            walk(&path, mounts)?;
        } else if file_type.is_file() && filename.ends_with(".wad.client") {
            let mount_name = Mounted::make_name(&filename);
            mounts.insert(mount_name, path);
        }
    }
    Ok(())
}
