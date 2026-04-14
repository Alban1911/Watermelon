use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::archive::Archive;
use super::entry::EntryData;
use super::mounted::Mounted;
use crate::overlay::hash::xxh64_from_path;

/// A collection of WADs keyed by mount name (lowercased stripped filename).
/// `Index::from_game_folder` builds one for an install; `Index::from_fantome`
/// builds one for a mod; the overlay itself is also an Index that
/// `add_overlay_mod` merges mods into.
#[derive(Default)]
pub struct Index {
    pub name: String,
    pub mounts: BTreeMap<String, Mounted>,
}

impl Index {
    pub fn from_game_folder(game_path: &Path) -> Result<Self> {
        let name = game_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let mut index = Index {
            name,
            mounts: BTreeMap::new(),
        };
        let final_path = game_path.join("DATA").join("FINAL");
        if final_path.exists() {
            add_from_game_folder_recursive(&final_path, game_path, &mut index.mounts)?;
        }
        Ok(index)
    }

    /// Opens a `.fantome` zip, collects `WAD/*.wad.client` entries (both
    /// packed-as-file and unpacked-as-directory forms), and also collects
    /// any `RAW/*` tree into a synthetic `_RAW.wad.client` mount — matches
    /// `from_mod_folder` in cslol.
    pub fn from_fantome(fantome_path: &Path) -> Result<Self> {
        let name = fantome_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let mut index = Index {
            name,
            mounts: BTreeMap::new(),
        };

        let file = fs::File::open(fantome_path)
            .with_context(|| format!("opening {}", fantome_path.display()))?;
        let mut zip = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {} as a zip archive", fantome_path.display()))?;

        // First pass: classify every zip entry. We can't keep multiple
        // `ZipFile` handles live at once (they borrow the archive mutably),
        // so we record the name + shape now and re-open by name later.
        let mut packed_wads: Vec<String> = Vec::new();
        let mut unpacked_wads: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut raw_files: Vec<String> = Vec::new();

        for i in 0..zip.len() {
            let entry = zip
                .by_index(i)
                .with_context(|| format!("reading zip entry at index {}", i))?;
            let entry_name = entry.name().to_string();
            let is_dir = entry.is_dir();
            drop(entry);

            if is_dir {
                continue;
            }

            if let Some(stripped) = entry_name.strip_prefix("WAD/") {
                if stripped.is_empty() {
                    continue;
                }
                if let Some(slash_pos) = stripped.find('/') {
                    let wad_root = &stripped[..slash_pos];
                    let inner = &stripped[slash_pos + 1..];
                    if wad_root.is_empty() || inner.is_empty() {
                        continue;
                    }
                    unpacked_wads
                        .entry(wad_root.to_string())
                        .or_default()
                        .push(entry_name.clone());
                } else {
                    packed_wads.push(entry_name.clone());
                }
            } else if let Some(stripped) = entry_name.strip_prefix("RAW/") {
                if !stripped.is_empty() {
                    raw_files.push(entry_name.clone());
                }
            }
        }

        // Packed WADs: the zip entry body IS a full WAD binary.
        for packed_name in &packed_wads {
            let wad_filename = packed_name.strip_prefix("WAD/").unwrap_or(packed_name);
            let bytes = read_zip_entry(&mut zip, packed_name)?;
            let archive = Archive::read_from_bytes(&bytes)
                .with_context(|| format!("parsing WAD payload {}", packed_name))?;
            let mounted = Mounted {
                relpath: PathBuf::from("WAD").join(wad_filename),
                archive,
            };
            let mount_name = mounted.name();
            index.mounts.insert(mount_name, mounted);
        }

        // Unpacked WADs: each zip entry under `WAD/X.wad.client/...` is a
        // loose file inside the virtual WAD, keyed by its sub-path hash.
        for (wad_root, inner_names) in &unpacked_wads {
            let mut archive = Archive::default();
            let prefix = format!("WAD/{}/", wad_root);
            for inner_name in inner_names {
                let Some(inner_path) = inner_name.strip_prefix(&prefix) else {
                    continue;
                };
                if inner_path.is_empty() {
                    continue;
                }
                let bytes = read_zip_entry(&mut zip, inner_name)?;
                let hash = xxh64_from_path(inner_path);
                archive
                    .entries
                    .insert(hash, EntryData::from_raw(bytes, 0));
            }
            if archive.entries.is_empty() {
                continue;
            }
            let mounted = Mounted {
                relpath: PathBuf::from("WAD").join(wad_root),
                archive,
            };
            let mount_name = mounted.name();
            index.mounts.insert(mount_name, mounted);
        }

        // `RAW/` tree: synthetic mount — matches cslol's behaviour of
        // packing loose raw assets into a pseudo WAD called `_RAW.wad.client`
        // so later overlap discovery can route them to the right base WAD.
        if !raw_files.is_empty() {
            let mut archive = Archive::default();
            for raw_name in &raw_files {
                let Some(inner_path) = raw_name.strip_prefix("RAW/") else {
                    continue;
                };
                if inner_path.is_empty() {
                    continue;
                }
                let bytes = read_zip_entry(&mut zip, raw_name)?;
                let hash = xxh64_from_path(inner_path);
                archive
                    .entries
                    .insert(hash, EntryData::from_raw(bytes, 0));
            }
            if !archive.entries.is_empty() {
                let mounted = Mounted {
                    relpath: PathBuf::from("WAD").join("_RAW.wad.client"),
                    archive,
                };
                let mount_name = mounted.name();
                index.mounts.insert(mount_name, mounted);
            }
        }

        Ok(index)
    }

    pub fn find_by_mount_name(&self, name: &str) -> Option<&Mounted> {
        self.mounts.get(name)
    }

    pub fn find_by_overlap(&self, archive: &Archive) -> Option<&Mounted> {
        let mut best_count = 0usize;
        let mut best: Option<&Mounted> = None;
        for mounted in self.mounts.values() {
            let mut count = 0usize;
            mounted
                .archive
                .for_each_overlap(archive, |_, _, _| count += 1);
            if count > best_count {
                best_count = count;
                best = Some(mounted);
            }
        }
        best
    }

    pub fn find_by_mount_name_or_overlap(
        &self,
        name: &str,
        archive: &Archive,
    ) -> Option<&Mounted> {
        self.find_by_mount_name(name)
            .or_else(|| self.find_by_overlap(archive))
    }

    pub fn remove_filter<F>(&mut self, mut pred: F)
    where
        F: FnMut(&str, &Mounted) -> bool,
    {
        self.mounts.retain(|k, v| !pred(k.as_str(), v));
    }

    /// Merges `mod_idx` into `self`, which starts empty and grows a mirror
    /// of the game tree with mod patches applied. For each mod WAD:
    ///
    ///   1. Find its base game WAD (by mount-name match, falling back to
    ///      overlap scoring).
    ///   2. If `self` doesn't yet have that base under the base's name,
    ///      clone the base game WAD into it.
    ///   3. Merge the mod's entries into the base copy.
    ///   4. For every OTHER game WAD that shares any entries with the mod,
    ///      also clone that WAD into `self` (if needed) and apply the
    ///      overlapping entries — catches mods that accidentally target the
    ///      wrong base WAD but still need to patch shared resources.
    pub fn add_overlay_mod(&mut self, game: &Index, mod_idx: &Index) -> Result<()> {
        for (mod_mount_name, mod_mounted) in &mod_idx.mounts {
            let base_mounted = game
                .find_by_mount_name_or_overlap(mod_mount_name, &mod_mounted.archive)
                .ok_or_else(|| {
                    anyhow!("failed to find base WAD for mod mount {}", mod_mount_name)
                })?;
            let base_name = base_mounted.name();

            let combined = self
                .mounts
                .entry(base_name.clone())
                .or_insert_with(|| base_mounted.clone());
            combined.archive.merge_in(&mod_mounted.archive);

            for (extra_name, extra_mounted) in &game.mounts {
                if extra_name == &base_name {
                    continue;
                }
                let overlap = extra_mounted.archive.overlapping(&mod_mounted.archive);
                if overlap.entries.is_empty() {
                    continue;
                }
                let combined = self
                    .mounts
                    .entry(extra_name.clone())
                    .or_insert_with(|| extra_mounted.clone());
                combined.archive.merge_in(&overlap);
            }
        }
        Ok(())
    }

    /// Cross-Index conflict resolution. Each mount in `self` is checked
    /// against every mount in `other`. Safe against borrow rules because
    /// `self` and `other` are distinct instances.
    pub fn resolve_conflicts_with(&mut self, other: &Index, ignore: bool) -> Result<()> {
        for mounted in self.mounts.values_mut() {
            for other_mounted in other.mounts.values() {
                mounted.resolve_conflicts(other_mounted, ignore)?;
            }
        }
        Ok(())
    }

    /// Self-conflict resolution — for a single mod that ships multiple WADs
    /// whose contents may collide. The "take out / compare against rest /
    /// put back" dance is needed because Rust's borrow checker won't let us
    /// hold a mutable reference to one map entry while reading others.
    pub fn resolve_conflicts_within(&mut self, ignore: bool) -> Result<()> {
        let keys: Vec<String> = self.mounts.keys().cloned().collect();
        for i in 0..keys.len() {
            let key_i = keys[i].clone();
            let Some(mut mounted_i) = self.mounts.remove(&key_i) else {
                continue;
            };
            for (j, key_j) in keys.iter().enumerate() {
                if i == j {
                    continue;
                }
                if let Some(mounted_j) = self.mounts.get(key_j) {
                    mounted_i.resolve_conflicts(mounted_j, ignore)?;
                }
            }
            self.mounts.insert(key_i, mounted_i);
        }
        Ok(())
    }

    pub fn write_to_directory(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        for mounted in self.mounts.values() {
            let out_path = dir.join(&mounted.relpath);
            mounted.archive.write_to_file(&out_path)?;
        }
        Ok(())
    }

    /// Deletes any `.wad.client` file in `dir` whose mount name is no
    /// longer tracked in `self`. Recursive: the overlay tree mirrors the
    /// game layout, so WADs live under subdirectories.
    pub fn cleanup_in_directory(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        self.cleanup_recursive(dir)
    }

    fn cleanup_recursive(&self, dir: &Path) -> Result<()> {
        for entry in fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                self.cleanup_recursive(&path)?;
            } else if file_type.is_file() {
                let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !filename.ends_with(".wad.client") {
                    continue;
                }
                let mount = Mounted::make_name(filename);
                if !self.mounts.contains_key(&mount) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        Ok(())
    }
}

fn add_from_game_folder_recursive(
    dir: &Path,
    game_path: &Path,
    mounts: &mut BTreeMap<String, Mounted>,
) -> Result<()> {
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
            // Directory-backed WADs exist in mod trees but the game itself
            // only loads file-backed ones; skip them.
            if filename.ends_with(".wad") || filename.ends_with(".wad.client") {
                continue;
            }
            add_from_game_folder_recursive(&path, game_path, mounts)?;
        } else if file_type.is_file() {
            if !filename.ends_with(".wad.client") {
                continue;
            }
            match Mounted::from_game_file(&path, game_path) {
                Ok(mounted) => {
                    let name = mounted.name();
                    mounts.insert(name, mounted);
                }
                Err(e) => {
                    eprintln!(
                        "[Overlay] skipping game WAD {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }
    Ok(())
}

fn read_zip_entry(zip: &mut zip::ZipArchive<fs::File>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("reading zip entry {}", name))?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut out)
        .with_context(|| format!("reading zip entry bytes {}", name))?;
    Ok(out)
}
