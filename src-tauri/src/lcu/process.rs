use anyhow::{anyhow, Result};
use std::path::PathBuf;
use sysinfo::{ProcessesToUpdate, System};

/// Locates the running LeagueClient.exe and returns its install directory.
pub fn find_install_directory() -> Result<PathBuf> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    for process in sys.processes().values() {
        if process.name().to_str() == Some("LeagueClient.exe") {
            if let Some(exe_path) = process.exe() {
                if let Some(dir) = exe_path.parent() {
                    return Ok(dir.to_path_buf());
                }
            }
        }
    }

    Err(anyhow!("LeagueClient.exe not found"))
}
