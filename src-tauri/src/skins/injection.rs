use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::overlay::hash::xxh64_from_path;
use crate::overlay::wad::Index;

const MAX_SKIN_SLOT: u32 = 999;

/// Detects which official skin slots a Fantome mod patches by probing the
/// WAD TOC for `skinN.bin` path hashes. Packed WADs only expose hashes, so
/// callers cannot reliably recover the original filenames directly.
pub fn detect_injects_on(fantome_path: &Path, champion: &str) -> Result<Vec<u32>> {
    let index = Index::from_fantome(fantome_path)?;
    let mut champion_keys = BTreeSet::new();
    let normalized_champion = normalize_champion_key(champion);
    if !normalized_champion.is_empty() {
        champion_keys.insert(normalized_champion);
    }
    for mount_name in index.mounts.keys() {
        let key = normalize_champion_key(mount_name);
        if !key.is_empty() && key != "raw" {
            champion_keys.insert(key);
        }
    }

    let mut entry_hashes = HashSet::new();
    for mounted in index.mounts.values() {
        entry_hashes.extend(mounted.archive.entries.keys().copied());
    }

    let mut slots = Vec::new();
    for slot in 0..=MAX_SKIN_SLOT {
        if champion_keys.iter().any(|champion_key| {
            candidate_skin_bin_paths(champion_key, slot)
                .iter()
                .any(|path| entry_hashes.contains(&xxh64_from_path(path)))
        }) {
            slots.push(slot);
        }
    }

    if slots.is_empty() {
        slots.push(0);
    }
    Ok(slots)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviousIndexEntry {
    file_stem: String,
    #[serde(default)]
    injects_on: Vec<u32>,
    #[serde(default)]
    source_file_len: Option<u64>,
    #[serde(default)]
    source_file_mtime_ns: Option<u128>,
    #[serde(default)]
    source_champion: Option<String>,
}

pub struct PreviousInjectsOnIndex {
    entries: BTreeMap<String, PreviousIndexEntry>,
}

impl PreviousInjectsOnIndex {
    pub fn load(index_path: &Path) -> Self {
        let Ok(bytes) = fs::read(index_path) else {
            return Self {
                entries: BTreeMap::new(),
            };
        };
        let Ok(by_champion) =
            serde_json::from_slice::<BTreeMap<String, Vec<PreviousIndexEntry>>>(&bytes)
        else {
            return Self {
                entries: BTreeMap::new(),
            };
        };
        let entries = by_champion
            .into_values()
            .flatten()
            .filter(|entry| !entry.file_stem.is_empty() && !entry.injects_on.is_empty())
            .map(|entry| (entry.file_stem.clone(), entry))
            .collect();
        Self { entries }
    }

    pub fn get_or_detect(
        &self,
        id: &str,
        fantome_path: &Path,
        champion: &str,
    ) -> Result<Vec<u32>> {
        let fingerprint = FileFingerprint::read(fantome_path)?;
        if let Some(entry) = self.entries.get(id) {
            let metadata_matches = entry.source_file_len == Some(fingerprint.len)
                && entry.source_file_mtime_ns == Some(fingerprint.mtime_ns)
                && entry.source_champion.as_deref() == Some(champion);
            let legacy_seed = entry.source_file_len.is_none()
                && entry.source_file_mtime_ns.is_none()
                && entry.source_champion.is_none();
            if metadata_matches || legacy_seed {
                return Ok(entry.injects_on.clone());
            }
        }

        detect_injects_on(fantome_path, champion)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileFingerprint {
    pub len: u64,
    pub mtime_ns: u128,
}

impl FileFingerprint {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(Self {
            len: metadata.len(),
            mtime_ns,
        })
    }
}

fn normalize_champion_key(value: &str) -> String {
    value
        .trim()
        .trim_end_matches(".wad.client")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn candidate_skin_bin_paths(champion_key: &str, slot: u32) -> [String; 2] {
    [
        format!("data/characters/{champion_key}/skins/skin{slot}.bin"),
        format!("assets/characters/{champion_key}/skins/skin{slot}.bin"),
    ]
}
