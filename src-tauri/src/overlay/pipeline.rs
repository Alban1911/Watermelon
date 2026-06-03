use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use xxhash_rust::xxh3::Xxh3;

use super::game_paths::GamePathIndex;
use super::hash::xxh64_lower;
use super::wad::toc::{
    write_header, write_v34_entry, EntryLoc, Toc, TocEntry, ENTRY_SIZE, HEADER_SIZE, LATEST_MAJOR,
    LATEST_MINOR,
};
use super::wad::{EntryData, Index, Mounted};

#[derive(Default)]
struct MapPatchState {
    states: HashMap<PathBuf, SavedMapPatch>,
}

struct SavedMapPatch {
    original_header: [u8; HEADER_SIZE],
    original_toc: Vec<u8>,
    original_size: u64,
}

static MAP_PATCH_STATE: OnceLock<Mutex<MapPatchState>> = OnceLock::new();

fn map_patch_state() -> &'static Mutex<MapPatchState> {
    MAP_PATCH_STATE.get_or_init(|| Mutex::new(MapPatchState::default()))
}

#[derive(Debug, Clone, Default)]
pub struct BuildOverlayOptions {
    /// Strip the TFT map WADs (`map21`, `map22`) from the game index so mod
    /// lookups can't accidentally latch onto them. cslol's `--noTFT` flag.
    pub no_tft: bool,
    /// When true, entries that conflict between two mods are resolved by
    /// "last-in wins" instead of erroring out. Matches cslol's
    /// `--ignoreConflict`.
    pub ignore_conflict: bool,
}

/// Builds a WAD overlay at `overlay_path` containing all the `fantomes`
/// merged against the game install at `game_path`. Ports `mod_mkoverlay`
/// from cslol-manager's `main_mod_tools.cpp`.
///
/// Output layout mirrors the game's `DATA/FINAL/...` tree so a runtime
/// injector can point the client at `overlay_path` as a virtual root and
/// have every patched WAD sit in the exact relative location the game
/// expects.
pub fn build_overlay(
    game_path: &Path,
    fantomes: &[PathBuf],
    overlay_path: &Path,
    opts: &BuildOverlayOptions,
) -> Result<()> {
    let mut game_index = Index::from_game_folder(game_path)?;
    if game_index.mounts.is_empty() {
        return Err(anyhow!(
            "no game WADs found under {}/DATA/FINAL - not a valid game folder",
            game_path.display()
        ));
    }
    if opts.no_tft {
        game_index.remove_filter(|k, _| matches!(k, "map21" | "map22"));
    }
    build_overlay_from_index(&game_index, fantomes, overlay_path, opts.ignore_conflict)
}

/// Same pipeline as `build_overlay` but takes a pre-built (and optionally
/// pre-filtered) game index. The hover runtime caches the game index
/// across rebuilds because `Index::from_game_folder` walks hundreds of
/// WADs and allocates an `EntryData` per entry - too slow to redo on
/// every carousel hover.
pub fn build_overlay_from_index(
    game_index: &Index,
    fantomes: &[PathBuf],
    overlay_path: &Path,
    ignore_conflict: bool,
) -> Result<()> {
    eprintln!(
        "[Inject] mkoverlay: starting with {} fantome(s), {} game mount(s)",
        fantomes.len(),
        game_index.mounts.len()
    );

    // SubChunkTOC blocklist. For every game WAD, compute the hash of
    // `<relpath>.SubChunkTOC` with `.client` replaced by `.SubChunkTOC`.
    // These TOCs index subchunked entries; swapping them out from under
    // the game via a mod corrupts streaming reads, so we strip any mod
    // entry whose name matches one of these hashes.
    let mut blocked: HashSet<u64> = HashSet::new();
    for mounted in game_index.mounts.values() {
        let relpath_str = mounted.relpath.to_string_lossy().replace('\\', "/");
        let subchunk_path = std::path::Path::new(&relpath_str).with_extension("SubChunkTOC");
        let subchunk_str = subchunk_path.to_string_lossy().replace('\\', "/");
        blocked.insert(xxh64_lower(&subchunk_str));
    }
    eprintln!(
        "[Inject] mkoverlay: blocklist has {} SubChunkTOC hashes",
        blocked.len()
    );

    let mut mod_queue: Vec<Index> = Vec::new();
    for fantome_path in fantomes {
        eprintln!("[Inject] mkoverlay: loading {}", fantome_path.display());
        let mut mod_index = match Index::from_fantome(fantome_path) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!(
                    "[Inject] mkoverlay: skipping {}: {}",
                    fantome_path.display(),
                    e
                );
                continue;
            }
        };
        let entries_before: usize = mod_index
            .mounts
            .values()
            .map(|m| m.archive.entries.len())
            .sum();
        eprintln!(
            "[Inject] mkoverlay: '{}' has {} mount(s), {} entr(ies) before blocklist",
            mod_index.name,
            mod_index.mounts.len(),
            entries_before
        );

        for mounted in mod_index.mounts.values_mut() {
            mounted
                .archive
                .entries
                .retain(|name, _| !blocked.contains(name));
        }
        mod_index
            .mounts
            .retain(|_, m| !m.archive.entries.is_empty());
        let entries_after: usize = mod_index
            .mounts
            .values()
            .map(|m| m.archive.entries.len())
            .sum();
        if entries_before != entries_after {
            eprintln!(
                "[Inject] mkoverlay: '{}' stripped {} blocklisted entr(ies)",
                mod_index.name,
                entries_before - entries_after
            );
        }
        if mod_index.mounts.is_empty() {
            eprintln!(
                "[Inject] mkoverlay: '{}' became empty after blocklist - skipped",
                mod_index.name
            );
            continue;
        }

        for (mount_name, mounted) in &mod_index.mounts {
            eprintln!(
                "[Inject] mkoverlay: '{}' mount '{}' -> {} entr(ies)",
                mod_index.name,
                mount_name,
                mounted.archive.entries.len()
            );
        }

        mod_index.resolve_conflicts_within(ignore_conflict)?;
        for old in mod_queue.iter_mut() {
            old.resolve_conflicts_with(&mod_index, ignore_conflict)?;
        }
        mod_queue.push(mod_index);
    }

    eprintln!(
        "[Inject] mkoverlay: merging {} mod(s) into overlay",
        mod_queue.len()
    );
    let mut overlay_index = Index::default();
    for mod_index in &mod_queue {
        overlay_index.add_overlay_mod(game_index, mod_index)?;
    }
    eprintln!(
        "[Inject] mkoverlay: overlay has {} mount(s) to write",
        overlay_index.mounts.len()
    );
    for (mount_name, mounted) in &overlay_index.mounts {
        eprintln!(
            "[Inject] mkoverlay: overlay -> {} ({} entries) -> {}",
            mount_name,
            mounted.archive.entries.len(),
            mounted.relpath.display()
        );
    }

    eprintln!("[Inject] mkoverlay: writing to {}", overlay_path.display());
    overlay_index.write_to_directory(overlay_path)?;
    overlay_index.cleanup_in_directory(overlay_path)?;
    eprintln!("[Inject] mkoverlay: done");

    Ok(())
}

/// Fast-path mkoverlay for interactive hover. Uses a pre-computed
/// `GamePathIndex` (filenames only, no entry reads) and loads just the
/// game WAD(s) the fantome actually targets. `build_overlay_from_index`
/// by comparison pre-loads every game WAD up front, which on cold
/// cache can exceed the entire champ-select-to-game window.
///
/// Assumes name-based mount resolution: the fantome's mount name must
/// match a game WAD filename. That's true for every properly-built
/// skin mod. If it doesn't match, the mount is skipped with a warning.
///
/// For map WADs, we follow the established cache strategy more closely: keep a
/// cached original `Map*.wad.client` under the overlay tree, patch only
/// the matching TOC entries in place, and preserve those base map copies
/// across cleanup so later injections can reuse them.
pub fn build_overlay_fast(
    game_paths: &GamePathIndex,
    fantome_path: &Path,
    overlay_path: &Path,
    ignore_conflict: bool,
) -> Result<Vec<String>> {
    restore_map_cache_patches(overlay_path)?;

    eprintln!(
        "[Inject] fast-mkoverlay: starting with fantome {} against {} game mount(s)",
        fantome_path.display(),
        game_paths.len()
    );

    // SubChunkTOC blocklist is still computed over every game WAD,
    // because any mod entry matching any of these hashes must be
    // stripped regardless of which base WAD it ends up in.
    let mut blocked: HashSet<u64> = HashSet::new();
    for (_, relpath) in game_paths.iter_rel() {
        let relpath_str = relpath.to_string_lossy().replace('\\', "/");
        let subchunk_path = std::path::Path::new(&relpath_str).with_extension("SubChunkTOC");
        let subchunk_str = subchunk_path.to_string_lossy().replace('\\', "/");
        blocked.insert(xxh64_lower(&subchunk_str));
    }
    eprintln!(
        "[Inject] fast-mkoverlay: blocklist has {} SubChunkTOC hashes",
        blocked.len()
    );

    eprintln!("[Inject] fast-mkoverlay: loading fantome");
    let mut mod_idx = Index::from_fantome(fantome_path)
        .with_context(|| format!("loading {}", fantome_path.display()))?;
    let entries_before: usize = mod_idx
        .mounts
        .values()
        .map(|m| m.archive.entries.len())
        .sum();
    eprintln!(
        "[Inject] fast-mkoverlay: '{}' has {} mount(s), {} entr(ies)",
        mod_idx.name,
        mod_idx.mounts.len(),
        entries_before
    );

    for mounted in mod_idx.mounts.values_mut() {
        mounted
            .archive
            .entries
            .retain(|name, _| !blocked.contains(name));
    }
    mod_idx.mounts.retain(|_, m| !m.archive.entries.is_empty());
    let entries_after: usize = mod_idx
        .mounts
        .values()
        .map(|m| m.archive.entries.len())
        .sum();
    if entries_before != entries_after {
        eprintln!(
            "[Inject] fast-mkoverlay: stripped {} blocklisted entr(ies)",
            entries_before - entries_after
        );
    }
    if mod_idx.mounts.is_empty() {
        return Err(anyhow!("fantome became empty after SubChunkTOC blocklist"));
    }

    mod_idx.resolve_conflicts_within(ignore_conflict)?;

    let mut written: Vec<String> = Vec::new();
    for (mount_name, mod_mounted) in &mod_idx.mounts {
        let Some(game_wad_path) = game_paths.find(mount_name) else {
            eprintln!(
                "[Inject] fast-mkoverlay: no game WAD named '{}' - skipping mount",
                mount_name
            );
            continue;
        };
        eprintln!(
            "[Inject] fast-mkoverlay: loading base {} -> {}",
            mount_name,
            game_wad_path.display()
        );
        let mut base = Mounted::from_game_file(game_wad_path, &game_paths.game_path)
            .with_context(|| format!("reading {}", game_wad_path.display()))?;
        let base_entry_count = base.archive.entries.len();
        base.archive.merge_in(&mod_mounted.archive);
        eprintln!(
            "[Inject] fast-mkoverlay: merged {} mod entr(ies) into base ({} -> {})",
            mod_mounted.archive.entries.len(),
            base_entry_count,
            base.archive.entries.len()
        );

        let out_path = overlay_path.join(&base.relpath);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        eprintln!("[Inject] fast-mkoverlay: writing {}", out_path.display());
        base.archive
            .write_to_file(&out_path)
            .with_context(|| format!("writing {}", out_path.display()))?;
        written.push(mount_name.clone());
    }

    for (map_mount_name, map_path) in game_paths.iter_map_shipping_wads() {
        let Some(relpath) = map_path.strip_prefix(&game_paths.game_path).ok() else {
            continue;
        };
        let out_path = overlay_path.join(relpath);
        ensure_cached_map_wad(map_path, &out_path)?;
        let patched = patch_wad_in_place(&out_path, mod_idx.mounts.values())
            .with_context(|| format!("patching cached map overlay {}", out_path.display()))?;
        if patched == 0 {
            continue;
        }
        eprintln!(
            "[Inject] fast-mkoverlay: patched cached map {} -> {} entr(ies)",
            out_path.display(),
            patched
        );
        written.push(map_mount_name.to_string());
    }

    if written.is_empty() {
        return Err(anyhow!(
            "no fantome mounts resolved to a game WAD - is the fantome well-formed?"
        ));
    }

    // Scrub any `.wad.client` files in the overlay dir left over from
    // a previous build for a different skin. Non-active map caches are
    // restored to their original on-disk bytes instead of removed so
    // later sessions can patch them in place without a fresh full copy.
    let keep: HashSet<String> = written.iter().cloned().collect();
    cleanup_overlay_except(overlay_path, &keep, game_paths)?;

    eprintln!(
        "[Inject] fast-mkoverlay: done ({} mount(s) written)",
        written.len()
    );
    Ok(written)
}

pub fn validate_map_cache_against_game(
    game_paths: &GamePathIndex,
    overlay_path: &Path,
) -> Result<usize> {
    let mut refreshed = 0usize;
    for (_, map_path) in game_paths.iter_map_shipping_wads() {
        let Some(relpath) = map_path.strip_prefix(&game_paths.game_path).ok() else {
            continue;
        };
        let cache_path = overlay_path.join(relpath);
        if !cache_path.exists() || hash_file(map_path)? != hash_file(&cache_path)? {
            ensure_cached_map_wad(map_path, &cache_path)?;
            refreshed += 1;
        }
    }
    if refreshed > 0 {
        eprintln!(
            "[Inject] fast-mkoverlay: refreshed {} cached map WAD(s) from game files",
            refreshed
        );
    }
    Ok(refreshed)
}

pub fn restore_map_cache_patches(overlay_path: &Path) -> Result<usize> {
    let mut restored = 0usize;
    let mut first_error: Option<anyhow::Error> = None;

    let pending: Vec<PathBuf> = {
        let state = map_patch_state()
            .lock()
            .expect("map patch state lock poisoned");
        state
            .states
            .keys()
            .filter(|path| path.starts_with(overlay_path))
            .cloned()
            .collect()
    };

    for path in pending {
        match restore_one_map_patch_with_retry(&path) {
            Ok(()) => {
                let mut state = map_patch_state()
                    .lock()
                    .expect("map patch state lock poisoned");
                if state.states.remove(&path).is_some() {
                    restored += 1;
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    if restored > 0 {
        eprintln!(
            "[Inject] fast-mkoverlay: restored {} cached map WAD(s)",
            restored
        );
    }
    if let Some(err) = first_error {
        Err(err)
    } else {
        Ok(restored)
    }
}

fn ensure_cached_map_wad(src_path: &Path, dst_path: &Path) -> Result<()> {
    if dst_path.exists() {
        return Ok(());
    }
    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(src_path, dst_path).with_context(|| {
        format!(
            "copying cached map {} -> {}",
            src_path.display(),
            dst_path.display()
        )
    })?;
    eprintln!(
        "[Inject] fast-mkoverlay: refreshed map cache {}",
        dst_path.display()
    );
    Ok(())
}

fn hash_file(path: &Path) -> Result<u64> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf[..])
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.digest())
}

fn patch_wad_in_place<'a>(
    wad_path: &Path,
    mod_mounts: impl IntoIterator<Item = &'a Mounted>,
) -> Result<usize> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(wad_path)
        .with_context(|| format!("opening {}", wad_path.display()))?;

    let toc = read_toc_prefix(&mut file, wad_path)?;
    remember_original_map_state(wad_path, &toc, &mut file)?;
    let mut toc_entries = toc.entries;
    let mut toc_index: HashMap<u64, usize> = HashMap::with_capacity(toc_entries.len());
    for (idx, entry) in toc_entries.iter().enumerate() {
        toc_index.insert(entry.name, idx);
    }

    let original_size = file
        .seek(SeekFrom::End(0))
        .with_context(|| format!("seeking {}", wad_path.display()))?;
    let mut append_pos = original_size;
    let mut appended: Vec<EntryData> = Vec::new();
    let mut patched = 0usize;

    for mod_mounted in mod_mounts {
        for (name, entry) in &mod_mounted.archive.entries {
            let Some(&idx) = toc_index.get(name) else {
                continue;
            };
            let optimized = entry.into_optimal()?;
            let size = optimized.bytes_len() as u64;
            toc_entries[idx].loc = EntryLoc {
                entry_type: optimized.entry_type(),
                subchunk_count: optimized.subchunk_count(),
                subchunk_index: optimized.subchunk_index(),
                offset: append_pos,
                size,
                size_decompressed: optimized.size_decompressed(),
                checksum: optimized.checksum(),
            };
            append_pos += size;
            appended.push(optimized);
            patched += 1;
        }
    }

    if patched == 0 {
        return Ok(0);
    }

    let header_checksum = compute_header_checksum(&toc_entries);
    let mut header = [0u8; HEADER_SIZE];
    write_header(
        &mut header,
        &toc.signature,
        header_checksum,
        toc_entries.len() as u32,
    );

    let mut toc_buf = vec![0u8; toc_entries.len() * ENTRY_SIZE];
    for (i, entry) in toc_entries.iter().enumerate() {
        let off = i * ENTRY_SIZE;
        write_v34_entry(&mut toc_buf[off..off + ENTRY_SIZE], entry.name, &entry.loc);
    }

    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking {}", wad_path.display()))?;
    file.write_all(&header)
        .with_context(|| format!("writing header {}", wad_path.display()))?;
    file.write_all(&toc_buf)
        .with_context(|| format!("writing TOC {}", wad_path.display()))?;
    file.seek(SeekFrom::Start(original_size))
        .with_context(|| format!("seeking {}", wad_path.display()))?;
    for entry in appended {
        file.write_all(entry.bytes())
            .with_context(|| format!("appending {}", wad_path.display()))?;
    }
    file.set_len(append_pos)
        .with_context(|| format!("truncating {}", wad_path.display()))?;

    Ok(patched)
}

fn remember_original_map_state(wad_path: &Path, toc: &Toc, file: &mut fs::File) -> Result<()> {
    let original_size = file
        .seek(SeekFrom::End(0))
        .with_context(|| format!("seeking {}", wad_path.display()))?;
    let mut state = map_patch_state()
        .lock()
        .expect("map patch state lock poisoned");
    if state.states.contains_key(wad_path) {
        return Ok(());
    }
    let mut original_header = [0u8; HEADER_SIZE];
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking {}", wad_path.display()))?;
    file.read_exact(&mut original_header)
        .with_context(|| format!("reading header {}", wad_path.display()))?;
    let desc_count = toc.entries.len();
    let toc_len = desc_count
        .checked_mul(ENTRY_SIZE)
        .ok_or_else(|| anyhow!("TOC size overflow for {}", wad_path.display()))?;
    let mut original_toc = vec![0u8; toc_len];
    file.read_exact(&mut original_toc)
        .with_context(|| format!("reading TOC {}", wad_path.display()))?;
    state.states.insert(
        wad_path.to_path_buf(),
        SavedMapPatch {
            original_header,
            original_toc,
            original_size,
        },
    );
    Ok(())
}

fn restore_one_map_patch(path: &Path, saved: &SavedMapPatch) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking {}", path.display()))?;
    file.write_all(&saved.original_header)
        .with_context(|| format!("writing header {}", path.display()))?;
    file.write_all(&saved.original_toc)
        .with_context(|| format!("writing TOC {}", path.display()))?;
    file.set_len(saved.original_size)
        .with_context(|| format!("truncating {}", path.display()))?;
    Ok(())
}

fn restore_one_map_patch_with_retry(path: &Path) -> Result<()> {
    const ATTEMPTS: usize = 20;
    const SLEEP_MS: u64 = 250;

    for attempt in 0..ATTEMPTS {
        let saved = {
            let state = map_patch_state()
                .lock()
                .expect("map patch state lock poisoned");
            let Some(saved) = state.states.get(path) else {
                return Ok(());
            };
            SavedMapPatch {
                original_header: saved.original_header,
                original_toc: saved.original_toc.clone(),
                original_size: saved.original_size,
            }
        };

        match restore_one_map_patch(path, &saved) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < ATTEMPTS => {
                if attempt == 0 {
                    eprintln!(
                        "[Inject] fast-mkoverlay: restore retry for {} after {}",
                        path.display(),
                        e
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(SLEEP_MS));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn read_toc_prefix(file: &mut fs::File, path: &Path) -> Result<Toc> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut header = [0u8; HEADER_SIZE];
    file.read_exact(&mut header)
        .with_context(|| format!("reading header {}", path.display()))?;
    let desc_count = u32::from_le_bytes(header[268..272].try_into().unwrap()) as usize;
    let toc_len = desc_count
        .checked_mul(ENTRY_SIZE)
        .ok_or_else(|| anyhow!("TOC size overflow for {}", path.display()))?;
    let mut prefix = Vec::with_capacity(HEADER_SIZE + toc_len);
    prefix.extend_from_slice(&header);
    prefix.resize(HEADER_SIZE + toc_len, 0);
    file.read_exact(&mut prefix[HEADER_SIZE..])
        .with_context(|| format!("reading TOC {}", path.display()))?;
    Toc::read(&prefix).with_context(|| format!("parsing TOC {}", path.display()))
}

fn compute_header_checksum(entries: &[TocEntry]) -> [u8; 8] {
    let mut hasher = Xxh3::new();
    hasher.update(&[b'R', b'W', LATEST_MAJOR, LATEST_MINOR]);
    for entry in entries {
        hasher.update(&entry.name.to_le_bytes());
        hasher.update(&entry.loc.checksum.to_le_bytes());
    }
    hasher.digest().to_le_bytes()
}

fn cleanup_overlay_except(
    dir: &Path,
    keep: &HashSet<String>,
    game_paths: &GamePathIndex,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    cleanup_recursive(dir, keep, game_paths)
}

fn cleanup_recursive(dir: &Path, keep: &HashSet<String>, game_paths: &GamePathIndex) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            cleanup_recursive(&path, keep, game_paths)?;
        } else if file_type.is_file() {
            let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !filename.ends_with(".wad.client") {
                continue;
            }
            let mount = Mounted::make_name(filename);
            if keep.contains(&mount) {
                continue;
            }
            if let Some(src_path) = game_paths.find(&mount) {
                if game_paths.is_map_shipping_wad(src_path) {
                    continue;
                }
            }
            eprintln!(
                "[Inject] fast-mkoverlay: cleanup removing {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}
