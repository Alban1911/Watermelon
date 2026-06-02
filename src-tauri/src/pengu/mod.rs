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
    app_data_dir.join("runtime").join(FLAG_FILE_NAME)
}

/// Returns true if the given IFEO `Debugger` value references our own
/// `core.dll`. Windows paths are case-insensitive so the substring match
/// uses lowercase on both sides. Used as a guard before touching the key
/// so we don't stomp on another injector that may have set
/// its own value while Watermelon was inactive.
fn is_ours(debugger_value: &str, core_dll_path: &Path) -> bool {
    let Some(dll_str) = core_dll_path.to_str() else {
        return false;
    };
    debugger_value
        .to_ascii_lowercase()
        .contains(&dll_str.to_ascii_lowercase())
}

fn key_is_ours(core_dll_path: &Path) -> bool {
    matches!(
        ifeo::read_debugger_value(),
        Ok(Some(ref v)) if is_ours(v, core_dll_path)
    )
}

/// Returns whether Watermelon's IFEO hook is already active. If the registry key
/// still points at our `core.dll`, make sure the activation marker exists so
/// later status checks and exit cleanup stay in sync after app restarts.
pub fn resume_if_active(core_dll_path: &Path, flag_path: &Path) -> Result<bool> {
    if key_is_ours(core_dll_path) {
        if !flag_path.exists() {
            write_flag(flag_path).context("rewriting activation marker")?;
        }
        return Ok(true);
    }

    if flag_path.exists() {
        let _ = fs::remove_file(flag_path);
    }

    Ok(false)
}

/// Activates IFEO injection. Writes the registry key under HKLM (requires
/// admin), creates the activation marker, and best-effort kills any running
/// LeagueClientUx so the parent respawns it through the rundll32 hook.
///
/// Before writing, the current IFEO value is read. If it's already set by
/// another tool (a non-Watermelon `core.dll` path), activation is skipped and
/// the existing value is left alone — Watermelon will run without injection
/// this session rather than stomping on the other tool's state.
pub fn activate(core_dll_path: &Path, flag_path: &Path) -> Result<()> {
    if !core_dll_path.exists() {
        return Err(anyhow!("core.dll not found at {}", core_dll_path.display()));
    }

    if let Ok(Some(existing)) = ifeo::read_debugger_value() {
        if !is_ours(&existing, core_dll_path) {
            eprintln!(
                "[Pengu] IFEO Debugger already set by another tool: {}",
                existing.trim()
            );
            eprintln!("[Pengu] Skipping activation to avoid stomping on it");
            return Ok(());
        }
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

/// Deactivates IFEO injection. Removes the registry key and kills any
/// running LeagueClientUx **only if** the key still references our own
/// core.dll. If another tool has taken over the IFEO slot in the
/// meantime, its state is preserved — we just delete our activation
/// marker and return.
pub fn deactivate(core_dll_path: &Path, flag_path: &Path) -> Result<()> {
    if key_is_ours(core_dll_path) {
        ifeo::delete_key().context("deleting IFEO registry key")?;
        process::terminate_league_client_ux();
    } else {
        eprintln!("[Pengu] IFEO Debugger is not ours — leaving it alone");
    }

    let _ = fs::remove_file(flag_path);
    Ok(())
}

fn write_flag(flag_path: &Path) -> Result<()> {
    if let Some(parent) = flag_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(flag_path, b"")?;
    Ok(())
}
