use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::game_paths::GamePathIndex;
use super::hash::xxh64_lower;
use super::wad::{Index, Mounted};

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
            "no game WADs found under {}/DATA/FINAL — not a valid game folder",
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
/// WADs and allocates an `EntryData` per entry — too slow to redo on
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
        eprintln!(
            "[Inject] mkoverlay: loading {}",
            fantome_path.display()
        );
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
                "[Inject] mkoverlay: '{}' became empty after blocklist — skipped",
                mod_index.name
            );
            continue;
        }

        for (mount_name, mounted) in &mod_index.mounts {
            eprintln!(
                "[Inject] mkoverlay: '{}' mount '{}' → {} entr(ies)",
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
            "[Inject] mkoverlay: overlay → {} ({} entries) → {}",
            mount_name,
            mounted.archive.entries.len(),
            mounted.relpath.display()
        );
    }

    eprintln!(
        "[Inject] mkoverlay: writing to {}",
        overlay_path.display()
    );
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
/// skin mod. If it doesn't match, the mount is skipped with a warning
/// — the overlap-fallback path from cslol is intentionally dropped for
/// this hot path.
pub fn build_overlay_fast(
    game_paths: &GamePathIndex,
    fantome_path: &Path,
    overlay_path: &Path,
    ignore_conflict: bool,
) -> Result<Vec<String>> {
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
    mod_idx
        .mounts
        .retain(|_, m| !m.archive.entries.is_empty());
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
                "[Inject] fast-mkoverlay: no game WAD named '{}' — skipping mount",
                mount_name
            );
            continue;
        };
        eprintln!(
            "[Inject] fast-mkoverlay: loading base {} → {}",
            mount_name,
            game_wad_path.display()
        );
        let mut base = Mounted::from_game_file(game_wad_path, &game_paths.game_path)
            .with_context(|| format!("reading {}", game_wad_path.display()))?;
        let base_entry_count = base.archive.entries.len();
        base.archive.merge_in(&mod_mounted.archive);
        eprintln!(
            "[Inject] fast-mkoverlay: merged {} mod entr(ies) into base ({} → {})",
            mod_mounted.archive.entries.len(),
            base_entry_count,
            base.archive.entries.len()
        );

        let out_path = overlay_path.join(&base.relpath);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        eprintln!(
            "[Inject] fast-mkoverlay: writing {}",
            out_path.display()
        );
        base.archive
            .write_to_file(&out_path)
            .with_context(|| format!("writing {}", out_path.display()))?;
        written.push(mount_name.clone());
    }

    if written.is_empty() {
        return Err(anyhow!(
            "no fantome mounts resolved to a game WAD — is the fantome well-formed?"
        ));
    }

    // Scrub any `.wad.client` files in the overlay dir left over from
    // a previous build for a different skin — the overlay should only
    // reflect the current hover.
    let keep: HashSet<String> = written.iter().cloned().collect();
    cleanup_overlay_except(overlay_path, &keep)?;

    eprintln!(
        "[Inject] fast-mkoverlay: done ({} mount(s) written)",
        written.len()
    );
    Ok(written)
}

fn cleanup_overlay_except(dir: &Path, keep: &HashSet<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    cleanup_recursive(dir, keep)
}

fn cleanup_recursive(dir: &Path, keep: &HashSet<String>) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            cleanup_recursive(&path, keep)?;
        } else if file_type.is_file() {
            let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !filename.ends_with(".wad.client") {
                continue;
            }
            let mount = Mounted::make_name(filename);
            if !keep.contains(&mount) {
                eprintln!("[Inject] fast-mkoverlay: cleanup removing {}", path.display());
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}
