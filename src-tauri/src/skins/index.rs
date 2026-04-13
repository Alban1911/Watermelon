use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use super::library::SkinEntry;
use super::state::SkinState;
use crate::data_dragon;

const INDEX_FILE_NAME: &str = "skins_index.json";

/// Custom skin id scheme (TalonPlugin convention). The 9M range is
/// reserved for non-Riot IDs. Each champion gets a 100-slot subrange
/// so up to 99 custom skins per champion fit without collision.
fn make_custom_id(champion_id: i64, within: usize) -> i64 {
    9_000_000 + champion_id * 100 + within as i64
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    id: i64,
    champion_id: i64,
    name: String,
    /// Whether Talon has a cached splash PNG for this skin and can
    /// therefore serve `https://talon/assets/splash/<fileStem>.png`.
    has_splash_asset: bool,
    /// Whether Talon has a cached composed background PNG for this skin and
    /// can therefore serve `https://talon/assets/background/<fileStem>.png`.
    has_background_asset: bool,
    /// Whether Talon has a cached HUD/icon PNG for this skin and can
    /// therefore serve `https://talon/assets/tile/<fileStem>.png`.
    has_tile_asset: bool,
    /// File stem of the backing `.fantome`. Reserved for later click
    /// handling that needs to look the real skin file up again.
    file_stem: String,
}

/// Builds `skins_index.json` from the current library + state + champion
/// map and writes it to `<app_data_dir>/skins_index.json`. The file is a
/// `{championId: [entries...]}` map so `core.dll`'s talon scheme handler
/// can stream it directly, and `preload.js` filters client-side.
///
/// Enabled skins whose champion can't be resolved through the Data
/// Dragon map are silently skipped — they'll still appear in Talon's
/// library UI, they just don't get an in-game carousel entry until
/// someone figures out the right alias.
pub fn regenerate(
    app_data_dir: &Path,
    skins: &[SkinEntry],
    state: &SkinState,
    champion_map: &HashMap<String, i64>,
) -> Result<()> {
    // Stable ordering: alphabetical by file stem so index-within-champion
    // ids don't shuffle on every run unless the library itself changes.
    let mut sorted: Vec<&SkinEntry> =
        skins.iter().filter(|s| state.is_enabled(&s.id)).collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut by_champion: BTreeMap<i64, Vec<IndexEntry>> = BTreeMap::new();
    for skin in sorted {
        let Some(champion_id) = data_dragon::lookup(champion_map, &skin.champion) else {
            continue;
        };
        let entries = by_champion.entry(champion_id).or_default();
        let idx_within = entries.len();
        entries.push(IndexEntry {
            id: make_custom_id(champion_id, idx_within),
            champion_id,
            name: skin.name.clone(),
            has_splash_asset: skin.preview.is_some(),
            has_background_asset: skin.background_preview.is_some(),
            has_tile_asset: skin.tile_preview.is_some(),
            file_stem: skin.id.clone(),
        });
    }

    // BTreeMap key rendering: i64 → string. Object keys in JSON are
    // always strings, so championId lives there as `"103"`.
    let mut output: BTreeMap<String, Vec<IndexEntry>> = BTreeMap::new();
    for (champion_id, entries) in by_champion {
        output.insert(champion_id.to_string(), entries);
    }

    fs::create_dir_all(app_data_dir).context("creating app_data_dir")?;
    let out_path = app_data_dir.join(INDEX_FILE_NAME);
    let json = serde_json::to_string(&output).context("serializing skin index")?;
    fs::write(&out_path, json).context("writing skin index file")?;

    Ok(())
}
