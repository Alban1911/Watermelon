use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::fantome;
use super::injection;
use super::preview;
use super::state::SkinState;

#[derive(Debug, Clone, Serialize)]
pub struct SkinEntry {
    pub id: String,
    pub name: String,
    pub champion: String,
    pub author: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub preview: Option<String>,
    pub background_preview: Option<String>,
    pub tile_preview: Option<String>,
    /// True when `tile_preview` points at a user-provided override rather
    /// than an auto-extracted asset — the UI uses this to surface a reset
    /// action.
    pub tile_preview_custom: bool,
    /// Same, but for the composed background asset.
    pub background_preview_custom: bool,
    pub champion_icon: Option<String>,
    pub injects_on: Vec<u32>,
    pub source_file_len: u64,
    pub source_file_mtime_ns: u128,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkinLibrary {
    pub dir: String,
    pub skins: Vec<SkinEntry>,
}

/// Scans the skins directory for `.fantome` files and returns them as entries.
/// Real metadata comes from parsing `META/info.json` inside each archive; when
/// parsing fails or a field is missing, the filename stem is used as a fallback
/// so broken files still appear in the list and can be diagnosed.
///
/// A PNG preview is generated (or reused from cache) for each skin by
/// extracting the largest DDS entry from the inner WAD. Preview generation is
/// best-effort: any failure silently leaves `preview = None` so the list
/// still loads.
pub fn scan(
    skins_dir: &Path,
    skins_index_path: &Path,
    previews_dir: &Path,
    background_previews_dir: &Path,
    custom_background_previews_dir: &Path,
    tile_previews_dir: &Path,
    custom_tile_previews_dir: &Path,
    icons_dir: &Path,
    state: &SkinState,
) -> Result<Vec<SkinEntry>> {
    if !skins_dir.exists() {
        fs::create_dir_all(skins_dir).context("creating skins directory")?;
    }

    // Per-scan cache so we fetch each champion's icon at most once even if
    // multiple skins share the champion.
    let mut icon_cache: HashMap<String, Option<String>> = HashMap::new();
    let previous_index = injection::PreviousInjectsOnIndex::load(skins_index_path);

    let mut entries = Vec::new();
    for dir_entry in fs::read_dir(skins_dir).context("reading skins directory")? {
        let dir_entry = dir_entry.context("reading directory entry")?;
        let path = dir_entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("fantome") {
            continue;
        }

        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let fingerprint = injection::FileFingerprint::read(&path)?;

        let meta = fantome::read(&path).ok();
        let name = meta
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| stem.clone());
        let champion = meta
            .as_ref()
            .and_then(|m| m.champion.clone())
            .unwrap_or_else(|| "Unknown".into());
        let author = meta.as_ref().and_then(|m| m.author.clone());
        let version = meta.as_ref().and_then(|m| m.version.clone());
        let description = meta.as_ref().and_then(|m| m.description.clone());

        let preview = preview::cached_preview_path(&path, previews_dir, &stem)
            .map(|p| p.to_string_lossy().into_owned());

        // Custom overrides take precedence over auto-extracted assets.
        // library.rs only resolves "what should be shown" — the warmup
        // pipeline still writes auto outputs underneath, so clearing a
        // custom reveals the auto PNG without having to regenerate it.
        let custom_background = custom_background_previews_dir.join(format!("{stem}.png"));
        let (background_preview, background_preview_custom) = if custom_background.is_file() {
            (Some(custom_background.to_string_lossy().into_owned()), true)
        } else {
            (
                preview::cached_background_preview_path(&path, background_previews_dir, &stem)
                    .map(|p| p.to_string_lossy().into_owned()),
                false,
            )
        };

        let custom_tile = custom_tile_previews_dir.join(format!("{stem}.png"));
        let (tile_preview, tile_preview_custom) = if custom_tile.is_file() {
            (Some(custom_tile.to_string_lossy().into_owned()), true)
        } else {
            (
                preview::cached_tile_preview_path(&path, tile_previews_dir, &stem)
                    .map(|p| p.to_string_lossy().into_owned()),
                false,
            )
        };

        let champion_icon = icon_cache
            .entry(champion.clone())
            .or_insert_with(|| {
                preview::cached_champion_icon_path(icons_dir, &champion)
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .clone();
        let enabled = state.is_enabled(&stem);
        let injects_on = if enabled {
            previous_index
                .get_or_detect(&stem, &path, &champion)
                .unwrap_or_else(|e| {
                    eprintln!(
                        "[SkinIndex] could not detect inject targets for {}: {}; falling back to skin0",
                        path.display(),
                        e
                    );
                    vec![0]
                })
        } else {
            vec![0]
        };

        entries.push(SkinEntry {
            id: stem.clone(),
            name,
            champion,
            author,
            version,
            description,
            preview,
            background_preview,
            tile_preview,
            tile_preview_custom,
            background_preview_custom,
            champion_icon,
            injects_on,
            source_file_len: fingerprint.len,
            source_file_mtime_ns: fingerprint.mtime_ns,
            enabled,
        });
    }

    // Sort by champion (case-insensitive) primary, then by skin name so mods
    // for the same champion cluster together in alphabetical order — matches
    // the grouped view's order and the in-game champion roster.
    entries.sort_by(|a, b| {
        let champ = a
            .champion
            .to_ascii_lowercase()
            .cmp(&b.champion.to_ascii_lowercase());
        if champ != std::cmp::Ordering::Equal {
            return champ;
        }
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    Ok(entries)
}
