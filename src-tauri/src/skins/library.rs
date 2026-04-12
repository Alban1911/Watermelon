use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::Path;

use super::fantome;
use super::state::SkinState;

#[derive(Debug, Clone, Serialize)]
pub struct SkinEntry {
    pub id: String,
    pub name: String,
    pub champion: String,
    pub author: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
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
pub fn scan(skins_dir: &Path, state: &SkinState) -> Result<Vec<SkinEntry>> {
    if !skins_dir.exists() {
        fs::create_dir_all(skins_dir).context("creating skins directory")?;
    }

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

        entries.push(SkinEntry {
            id: stem.clone(),
            name,
            champion,
            author,
            version,
            description,
            enabled: state.is_enabled(&stem),
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}
