use std::time::Duration;
use tauri::AppHandle;
use tokio::time::sleep;

use super::discovery::discover;
use super::events;
use super::http::LcuClient;
use super::process::find_install_directory;

const LOCKFILE_RETRY_ATTEMPTS: u8 = 20;
const LOCKFILE_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const OUTER_LOOP_INTERVAL: Duration = Duration::from_secs(2);

/// Discovers the League Client and runs an event-driven WebSocket listener.
/// Any failure returns us to discovery — the client can be closed or restarted
/// at any time and we should recover transparently.
pub async fn run(app: AppHandle) {
    eprintln!("[Talon] Starting LCU listener");

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

        let client = match LcuClient::new(info.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[LCU] Failed to create HTTP client: {}", e);
                sleep(OUTER_LOOP_INTERVAL).await;
                continue 'outer;
            }
        };

        if let Err(e) = events::run(&info, &client, &app).await {
            eprintln!("[LCU] Event loop ended: {}, reconnecting", e);
        }
        sleep(OUTER_LOOP_INTERVAL).await;
    }
}
