use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use zip::ZipArchive;

/// Raw `META/info.json` schema used by cslol-manager `.fantome` files.
/// Fields are PascalCase in the JSON. Everything is optional — we want
/// malformed or incomplete mods to still parse with whatever we can recover.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InfoJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    heroes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FantomeMetadata {
    pub name: Option<String>,
    pub champion: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// Opens a `.fantome` archive and extracts display metadata.
/// The champion is taken from `info.json`'s `Heroes` field when present,
/// otherwise derived from the `WAD/{Champion}.wad.client` entry name.
pub fn read(path: &Path) -> Result<FantomeMetadata> {
    let file = File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut zip = ZipArchive::new(file).context("reading zip archive")?;

    let info = read_info_json(&mut zip).unwrap_or_default();
    let champion_from_wad = find_champion_from_wad_entries(&mut zip);

    let champion = info
        .heroes
        .first()
        .cloned()
        .filter(|s| !s.is_empty())
        .or(champion_from_wad);

    Ok(FantomeMetadata {
        name: info.name.filter(|s| !s.is_empty()),
        champion,
        author: info.author.filter(|s| !s.is_empty()),
        version: info.version.filter(|s| !s.is_empty()),
        description: info.description.filter(|s| !s.is_empty()),
    })
}

fn read_info_json(zip: &mut ZipArchive<File>) -> Option<InfoJson> {
    let mut entry = zip.by_name("META/info.json").ok()?;
    let mut content = String::new();
    entry.read_to_string(&mut content).ok()?;
    serde_json::from_str::<InfoJson>(&content).ok()
}

fn find_champion_from_wad_entries(zip: &mut ZipArchive<File>) -> Option<String> {
    let len = zip.len();
    for i in 0..len {
        let name = match zip.by_index(i) {
            Ok(file) => file.name().to_string(),
            Err(_) => continue,
        };
        if let Some(stripped) = name.strip_prefix("WAD/") {
            if let Some(champ) = stripped.strip_suffix(".wad.client") {
                if !champ.is_empty() {
                    return Some(champ.to_string());
                }
            }
        }
    }
    None
}
