use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::time::sleep;

use super::discovery::discover;
use super::http::LcuClient;
use super::process::find_install_directory;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const LOCKFILE_RETRY_ATTEMPTS: u8 = 20;
const LOCKFILE_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const OUTER_LOOP_INTERVAL: Duration = Duration::from_secs(2);

/// Runs the outer/inner reconnect loop. Any HTTP failure in the inner loop
/// breaks back out and re-discovers the client from scratch — the client
/// can be closed or restarted at any time and we should recover transparently.
pub async fn run(app: AppHandle) {
    eprintln!("[Talon] Starting LCU poller");

    'outer: loop {
        let install_dir = match find_install_directory() {
            Ok(dir) => dir,
            Err(_) => {
                sleep(OUTER_LOOP_INTERVAL).await;
                continue 'outer;
            }
        };
        eprintln!("[LCU] Found League Client at {}", install_dir.display());

        let info = {
            let mut result = None;
            for _ in 0..LOCKFILE_RETRY_ATTEMPTS {
                match discover(&install_dir) {
                    Ok(info) => {
                        result = Some(info);
                        break;
                    }
                    Err(_) => sleep(LOCKFILE_RETRY_INTERVAL).await,
                }
            }
            match result {
                Some(info) => info,
                None => {
                    eprintln!("[LCU] Lockfile did not appear after 10s");
                    continue 'outer;
                }
            }
        };

        eprintln!("[LCU] Connected on port {}", info.port);

        let client = match LcuClient::new(info) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[LCU] Failed to create HTTP client: {}", e);
                sleep(Duration::from_secs(1)).await;
                continue 'outer;
            }
        };

        let mut last_phase: Option<String> = None;
        loop {
            match client.get("/lol-gameflow/v1/gameflow-phase").await {
                Ok(body) => {
                    if let Some(phase) = extract_phase(&body) {
                        if last_phase.as_deref() != Some(phase) {
                            eprintln!("[Phase] {}", phase);
                            let _ = app.emit("lcu:phase-changed", phase);
                            last_phase = Some(phase.to_string());
                        }
                    }
                }
                Err(_) => {
                    eprintln!("[LCU] Poll failed, reconnecting");
                    continue 'outer;
                }
            }
            sleep(POLL_INTERVAL).await;
        }
    }
}

/// Response body looks like `"Lobby"` — a JSON string. Strip the surrounding quotes.
fn extract_phase(body: &str) -> Option<&str> {
    let trimmed = body.trim();
    if trimmed.len() < 2 {
        return None;
    }
    if !trimmed.starts_with('"') || !trimmed.ends_with('"') {
        return None;
    }
    Some(&trimmed[1..trimmed.len() - 1])
}
