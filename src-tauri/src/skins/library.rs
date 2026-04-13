use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::fantome;
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
    pub champion_icon: Option<String>,
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
    previews_dir: &Path,
    background_previews_dir: &Path,
    tile_previews_dir: &Path,
    icons_dir: &Path,
    state: &SkinState,
) -> Result<Vec<SkinEntry>> {
    if !skins_dir.exists() {
        fs::create_dir_all(skins_dir).context("creating skins directory")?;
    }

    // Per-scan cache so we fetch each champion's icon at most once even if
    // multiple skins share the champion.
    let mut icon_cache: HashMap<String, Option<String>> = HashMap::new();

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

        let preview = preview::cached_or_extract(
            &path,
            previews_dir,
            &stem,
            meta.as_ref().and_then(|m| m.champion.as_deref()),
        )
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned());

        let background_preview = preview::cached_or_extract_background(
            &path,
            background_previews_dir,
            &stem,
            meta.as_ref().and_then(|m| m.champion.as_deref()),
        )
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned());

        let tile_preview = preview::cached_or_extract_tile(
            &path,
            tile_previews_dir,
            &stem,
            meta.as_ref().and_then(|m| m.champion.as_deref()),
        )
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned());

        let champion_icon = icon_cache
            .entry(champion.clone())
            .or_insert_with(|| {
                preview::cached_champion_icon(icons_dir, &champion)
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .clone();

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
            champion_icon,
            enabled: state.is_enabled(&stem),
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
