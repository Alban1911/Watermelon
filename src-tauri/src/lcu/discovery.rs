use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct LcuInfo {
    pub port: u16,
    /// Full `Basic <b64>` value for the Authorization header.
    pub auth_header: String,
}

/// Reads the League Client lockfile and returns the port + pre-built auth header.
/// Lockfile format: `name:pid:port:password:protocol`
pub fn discover(league_path: &Path) -> Result<LcuInfo> {
    let lockfile_path = league_path.join("lockfile");
    let content = fs::read_to_string(&lockfile_path).context("reading lockfile")?;

    let parts: Vec<&str> = content.trim().split(':').collect();
    if parts.len() < 5 {
        return Err(anyhow!("invalid lockfile format: {} fields", parts.len()));
    }

    let port: u16 = parts[2].parse().context("invalid port in lockfile")?;
    let password = parts[3];

    let raw = format!("riot:{}", password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
    let auth_header = format!("Basic {}", encoded);

    Ok(LcuInfo { port, auth_header })
}
