use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Persistent set of skin IDs that should be enabled. Stored as a JSON array
/// at %APPDATA%/Watermelon/state.json.
#[derive(Debug, Default, Clone)]
pub struct SkinState {
    enabled: HashSet<String>,
}

impl SkinState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path).context("reading state file")?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let list: Vec<String> =
            serde_json::from_str(&content).context("parsing state file")?;
        Ok(Self { enabled: list.into_iter().collect() })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("creating state dir")?;
        }
        let mut list: Vec<&String> = self.enabled.iter().collect();
        list.sort();
        let content = serde_json::to_string_pretty(&list).context("serializing state")?;
        fs::write(path, content).context("writing state file")?;
        Ok(())
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    pub fn set(&mut self, id: String, enabled: bool) {
        if enabled {
            self.enabled.insert(id);
        } else {
            self.enabled.remove(&id);
        }
    }

    pub fn rename_id(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id {
            return;
        }
        if self.enabled.remove(old_id) {
            self.enabled.insert(new_id.to_string());
        }
    }
}
