use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::time::Duration;

const VERSIONS_URL: &str = "https://ddragon.leagueoflegends.com/api/versions.json";

/// Fetches the latest Data Dragon champion list and builds a map of
/// sanitized alias → numeric championId. The map is indexed by both the
/// Data Dragon `id` (e.g. "MonkeyKing") and the display `name` (e.g.
/// "Wukong") after sanitization, so Talon library entries that use
/// either naming convention resolve correctly.
pub async fn fetch_champion_map() -> Result<HashMap<String, i64>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let versions: Vec<String> = client.get(VERSIONS_URL).send().await?.json().await?;
    let version = versions
        .first()
        .ok_or_else(|| anyhow!("empty versions list"))?;

    let url = format!(
        "https://ddragon.leagueoflegends.com/cdn/{}/data/en_US/champion.json",
        version
    );
    let response: serde_json::Value = client.get(&url).send().await?.json().await?;
    let data = response
        .get("data")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("missing 'data' object in champion.json"))?;

    let mut map: HashMap<String, i64> = HashMap::new();
    for (dd_id, info) in data {
        let Some(key_str) = info.get("key").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(key) = key_str.parse::<i64>() else {
            continue;
        };
        map.insert(sanitize(dd_id), key);
        if let Some(name) = info.get("name").and_then(|v| v.as_str()) {
            map.insert(sanitize(name), key);
        }
    }
    Ok(map)
}

/// Looks up a champion id from a `.fantome`-style alias. Handles
/// casing/punctuation variations via `sanitize`.
pub fn lookup(map: &HashMap<String, i64>, alias: &str) -> Option<i64> {
    map.get(&sanitize(alias)).copied()
}

/// Lowercases and strips non-alphanumeric characters so "Miss Fortune",
/// "missfortune" and "MissFortune" all hit the same key.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}
