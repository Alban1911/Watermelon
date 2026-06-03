use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::archive::Archive;
use super::toc::{Toc, LATEST_MAJOR, LATEST_MINOR};

/// A single WAD archive mounted at a relative path within a game install or
/// mod tree. The relpath is preserved so `Index::write_to_directory` can
/// reproduce the source layout.
#[derive(Clone, Default)]
pub struct Mounted {
    pub relpath: PathBuf,
    pub archive: Archive,
}

impl Mounted {
    /// Mount name = lowercased filename with the `.wad.client` suffix
    /// stripped (falling back to stripping just `.wad`). This is the key
    /// `Index::mounts` uses to match up mod WADs with their base game WADs.
    pub fn make_name(filename: &str) -> String {
        let mut s = filename.to_ascii_lowercase();
        if let Some(stripped) = s.strip_suffix(".client") {
            s = stripped.to_string();
        }
        if let Some(stripped) = s.strip_suffix(".wad") {
            s = stripped.to_string();
        }
        s
    }

    pub fn name(&self) -> String {
        let filename = self
            .relpath
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        Self::make_name(filename)
    }

    pub fn from_game_file(path: &Path, game_path: &Path) -> Result<Self> {
        let relpath = path
            .strip_prefix(game_path)
            .map_err(|_| {
                anyhow!(
                    "game WAD path {} is not under game root {}",
                    path.display(),
                    game_path.display()
                )
            })?
            .to_path_buf();

        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let toc = Toc::read(&bytes)?;
        if toc.major != LATEST_MAJOR || toc.minor != LATEST_MINOR {
            return Err(anyhow!(
                "unknown WAD version {}.{} in {} — game only accepts {}.{}",
                toc.major,
                toc.minor,
                path.display(),
                LATEST_MAJOR,
                LATEST_MINOR
            ));
        }
        let archive = if toc.entries.is_empty() {
            Archive::default()
        } else {
            let a = Archive::read_from_toc(&bytes, &toc)?;
            a.mark_optimal();
            a
        };

        Ok(Self { relpath, archive })
    }

    pub fn from_mod_file(path: &Path, mod_path: &Path) -> Result<Self> {
        let relpath = path
            .strip_prefix(mod_path)
            .map_err(|_| {
                anyhow!(
                    "mod WAD path {} is not under mod root {}",
                    path.display(),
                    mod_path.display()
                )
            })?
            .to_path_buf();
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !filename.ends_with(".wad.client") {
            return Err(anyhow!("not a .wad.client file: {}", path.display()));
        }
        let archive = Archive::read_from_file(path)?;
        Ok(Self { relpath, archive })
    }

    /// Walks overlapping entries with `other` and reconciles conflicts. On
    /// matching checksums (identical or decompressed-identical) the entry is
    /// replaced with `other`'s handle so downstream dedupe works; on genuine
    /// conflict it errors unless `ignore` is set. Caller is expected to skip
    /// the self-pair before calling (see `Index::resolve_conflicts_within`).
    pub fn resolve_conflicts(&mut self, other: &Mounted, ignore: bool) -> Result<()> {
        let mut had_conflict = 0usize;
        self.archive
            .for_each_overlap_mut(&other.archive, |_, old, new| {
                if old.checksum() == new.checksum() {
                    *old = new.clone();
                    return;
                }
                let old_dec = match old.into_decompressed() {
                    Ok(d) => d,
                    Err(_) => {
                        had_conflict += 1;
                        return;
                    }
                };
                let new_dec = match new.into_decompressed() {
                    Ok(d) => d,
                    Err(_) => {
                        had_conflict += 1;
                        return;
                    }
                };
                if old_dec.checksum() == new_dec.checksum() {
                    *old = new.clone();
                    return;
                }
                if ignore {
                    *old = new.clone();
                    return;
                }
                had_conflict += 1;
            });
        if had_conflict > 0 {
            return Err(anyhow!(
                "{} conflicting file(s) between {} and {}",
                had_conflict,
                self.relpath.display(),
                other.relpath.display()
            ));
        }
        Ok(())
    }

    /// Removes entries that are not present in `other`. Returns the number
    /// of removed entries.
    pub fn remove_unknown(&mut self, other: &Mounted) -> usize {
        let before = self.archive.entries.len();
        self.archive
            .entries
            .retain(|name, _| other.archive.entries.contains_key(name));
        before - self.archive.entries.len()
    }

    /// Removes entries whose contents match `other`'s (either same physical
    /// payload or same decompressed payload). Returns the number of removed
    /// entries.
    pub fn remove_unmodified(&mut self, other: &Mounted) -> usize {
        let mut count = 0usize;
        self.archive.erase_overlap(&other.archive, |_, old, new| {
            if old.checksum() == new.checksum() {
                count += 1;
                return true;
            }
            if let (Ok(old_dec), Ok(new_dec)) = (old.into_decompressed(), new.into_decompressed()) {
                if old_dec.checksum() == new_dec.checksum() {
                    count += 1;
                    return true;
                }
            }
            false
        });
        count
    }
}
