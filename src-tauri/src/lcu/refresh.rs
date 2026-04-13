use anyhow::Result;

use super::discovery::discover;
use super::http::LcuClient;
use super::process::find_install_directory;

const REFRESH_ENDPOINTS: &[&str] = &[
    "/lol-champ-select/v1/session/simple-inventory",
    "/lol-champ-select/v1/retrieve-latest-game-dto",
    "/lol-lobby-team-builder/champ-select/v1/simple-inventory",
    "/lol-lobby-team-builder/champ-select/v1/retrieve-latest-game-dto",
];

/// Best-effort champ-select refresh nudge. If League isn't running or the
/// client isn't in a champ-select flow, this quietly becomes a no-op.
pub async fn refresh_champ_select() -> Result<bool> {
    let install_dir = match find_install_directory() {
        Ok(dir) => dir,
        Err(_) => return Ok(false),
    };
    let info = match discover(&install_dir) {
        Ok(info) => info,
        Err(_) => return Ok(false),
    };
    let client = LcuClient::new(info)?;

    let mut refreshed = false;
    for path in REFRESH_ENDPOINTS {
        match client.post_empty(path).await {
            Ok(status) if status.is_success() => {
                refreshed = true;
                eprintln!("[LCU] refresh request accepted via {}", path);
            }
            Ok(status) => {
                eprintln!("[LCU] refresh request {} returned {}", path, status);
            }
            Err(e) => {
                eprintln!("[LCU] refresh request {} failed: {}", path, e);
            }
        }
    }

    Ok(refreshed)
}
