pub mod ifeo;
pub mod process;

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const FLAG_FILE_NAME: &str = "pengu.flag";

/// Returns the path of the activation marker file inside the app's data
/// directory. The marker exists while injection is active and lets us
/// detect crashes from a previous run.
pub fn flag_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FLAG_FILE_NAME)
}

/// Activates IFEO injection. Writes the registry key under HKLM (requires
/// admin), creates the activation marker, and best-effort kills any running
/// LeagueClientUx so the parent respawns it through the rundll32 hook.
pub fn activate(core_dll_path: &Path, flag_path: &Path) -> Result<()> {
    if !core_dll_path.exists() {
        return Err(anyhow!(
            "core.dll not found at {}",
            core_dll_path.display()
        ));
    }
    ifeo::write_key(core_dll_path).context("writing IFEO registry key")?;
    write_flag(flag_path).context("writing activation marker")?;
    process::terminate_league_client_ux();
    Ok(())
}

/// Resolves `core.dll` from the Tauri resource directory. In dev this is
/// the project's `src-tauri/resources/` (where `build.rs` writes it); in a
/// bundled install it's the resource subdir of the install location.
pub fn resolve_core_dll_path<P: AsRef<Path>>(resource_dir: P) -> PathBuf {
    resource_dir.as_ref().join("resources").join("core.dll")
}

/// Deactivates IFEO injection. Removes the registry key, deletes the
/// activation marker, and best-effort kills any running LeagueClientUx so
/// the parent respawns it cleanly without the hook.
pub fn deactivate(flag_path: &Path) -> Result<()> {
    ifeo::delete_key().context("deleting IFEO registry key")?;
    let _ = fs::remove_file(flag_path);
    process::terminate_league_client_ux();
    Ok(())
}

/// Recovers from a previous run that exited without deactivating. The IFEO
/// key is non-volatile, so a crash leaves Windows redirecting LeagueClientUx
/// launches through a rundll32 hook that may now point at a missing dll.
/// If the activation marker is present, clear the key and the marker.
pub fn cleanup_if_dirty(flag_path: &Path) {
    if !flag_path.exists() {
        return;
    }
    eprintln!("[Pengu] Detected dirty state from previous run, clearing IFEO key");
    if let Err(e) = ifeo::delete_key() {
        eprintln!("[Pengu] Cleanup failed: {} — admin required?", e);
        return;
    }
    let _ = fs::remove_file(flag_path);
}

fn write_flag(flag_path: &Path) -> Result<()> {
    if let Some(parent) = flag_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(flag_path, b"")?;
    Ok(())
}
