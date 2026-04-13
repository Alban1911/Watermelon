mod data_dragon;
mod lcu;
mod pengu;
mod skins;
mod wad;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_opener::OpenerExt;

/// Path of the activation marker file. Stored here so the Ctrl+C handler
/// (which runs on a separate thread without an `AppHandle`) and the Tauri
/// `ExitRequested` callback can both find it at shutdown time.
static FLAG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Resolved path of our `core.dll`. Needed at shutdown by `pengu::deactivate`
/// so it can verify the IFEO Debugger value still references our dll before
/// deleting the registry key.
static CORE_DLL_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Champion alias → numeric id map, fetched once from Data Dragon at
/// startup. Used by `regenerate_skin_index` to resolve `.fantome` file
/// metadata (which uses string aliases) to the numeric championIds the
/// LCU carousel speaks in.
static CHAMPION_MAP: OnceLock<HashMap<String, i64>> = OnceLock::new();

/// Idempotent shutdown cleanup. Deactivates pengu (deletes IFEO key if it's
/// still ours, removes the flag file, kills `LeagueClientUx.exe` so the
/// parent respawns it without injection) AND kills any leftover Vite dev
/// server on port 1420 — tauri dev's child cleanup doesn't always propagate
/// Ctrl+C to vite on Windows, so without this the next `pnpm tauri dev`
/// fails with "Port 1420 is already in use".
fn cleanup_on_exit() {
    if let (Some(core_dll), Some(flag)) = (CORE_DLL_PATH.get(), FLAG_PATH.get()) {
        match pengu::deactivate(core_dll, flag) {
            Ok(()) => eprintln!("[Pengu] cleaned up on exit"),
            Err(e) => eprintln!("[Pengu] cleanup on exit failed: {}", e),
        }
    }
    kill_dev_server_port();
}

/// Rebuilds `<app_data_dir>/skins_index.json` from the current library
/// scan + state + champion map. Called on every skin-mutating command so
/// `core.dll`'s talon scheme handler always serves up-to-date data.
/// Best-effort: logs errors but never fails the caller. If the champion
/// map isn't loaded yet (Data Dragon fetch still in flight), this is a
/// no-op and the file will be written later when the fetch completes.
fn regenerate_skin_index(app: &AppHandle) {
    let Ok(paths) = resolve_paths(app) else { return };
    let Ok(data_dir) = app.path().app_data_dir() else { return };
    let Some(champion_map) = CHAMPION_MAP.get() else {
        eprintln!("[SkinIndex] champion map not loaded yet, skipping regenerate");
        return;
    };
    let state = match SkinState::load(&paths.state_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[SkinIndex] load state failed: {}", e);
            return;
        }
    };
    let skins = match library::scan(
        &paths.skins_dir,
        &paths.previews_dir,
        &paths.background_previews_dir,
        &paths.tile_previews_dir,
        &paths.champion_icons_dir,
        &state,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[SkinIndex] library scan failed: {}", e);
            return;
        }
    };
    if let Err(e) = skins::index::regenerate(&data_dir, &skins, &state, champion_map) {
        eprintln!("[SkinIndex] regenerate failed: {}", e);
    }
}

fn kill_dev_server_port() {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: child gets no console, is not in our console
    // process group, and therefore won't see the Ctrl+C event the parent
    // shell broadcast. Without this, a child PowerShell spawned from
    // inside our Ctrl+C handler propagates the signal back up the
    // console group and takes the user's interactive shell down with it.
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetTCPConnection -LocalPort 1420 -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

use skins::library::{self, SkinLibrary};
use skins::state::SkinState;

struct AppPaths {
    skins_dir: PathBuf,
    state_path: PathBuf,
    previews_dir: PathBuf,
    background_previews_dir: PathBuf,
    tile_previews_dir: PathBuf,
    champion_icons_dir: PathBuf,
}

fn resolve_paths(app: &AppHandle) -> Result<AppPaths, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(AppPaths {
        skins_dir: data_dir.join("skins"),
        state_path: data_dir.join("state.json"),
        previews_dir: data_dir.join("previews"),
        background_previews_dir: data_dir.join("background_previews"),
        tile_previews_dir: data_dir.join("tile_previews"),
        champion_icons_dir: data_dir.join("champion_icons"),
    })
}

/// One-time storage migration for pre-normalization libraries. Existing
/// `.fantome` files used to keep their source filename, which leaked spaces
/// and punctuation into preview cache names and Talon asset URLs. Rename them
/// into the normalized import format, carry their preview PNG along, and
/// update enabled-state ids to match.
fn migrate_existing_skin_filenames(paths: &AppPaths) -> Result<(), String> {
    std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.background_previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.tile_previews_dir).map_err(|e| e.to_string())?;

    let mut state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    let mut changed = false;

    let entries = std::fs::read_dir(&paths.skins_dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let source_path = entry.path();
        if source_path.extension().and_then(|e| e.to_str()) != Some("fantome") {
            continue;
        }

        let Some(file_name) = source_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(old_id) = source_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let normalized_name = normalize_import_filename(file_name);
        let Some(normalized_stem) = PathBuf::from(&normalized_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string) else {
            continue;
        };

        if old_id == normalized_stem {
            continue;
        }

        let target_path = pick_dest(&paths.skins_dir, &normalized_name);
        let Some(new_id) = target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string) else {
            continue;
        };

        std::fs::rename(&source_path, &target_path)
            .map_err(|e| format!("renaming {}: {e}", source_path.display()))?;

        let old_preview = paths.previews_dir.join(format!("{old_id}.png"));
        if old_preview.exists() {
            let new_preview = paths.previews_dir.join(format!("{new_id}.png"));
            if old_preview != new_preview {
                if new_preview.exists() {
                    let _ = std::fs::remove_file(&new_preview);
                }
                std::fs::rename(&old_preview, &new_preview).map_err(|e| {
                    format!("renaming preview {}: {e}", old_preview.display())
                })?;
            }
        }

        let old_background_preview = paths.background_previews_dir.join(format!("{old_id}.png"));
        if old_background_preview.exists() {
            let new_background_preview = paths.background_previews_dir.join(format!("{new_id}.png"));
            if old_background_preview != new_background_preview {
                if new_background_preview.exists() {
                    let _ = std::fs::remove_file(&new_background_preview);
                }
                std::fs::rename(&old_background_preview, &new_background_preview).map_err(|e| {
                    format!(
                        "renaming background preview {}: {e}",
                        old_background_preview.display()
                    )
                })?;
            }
        }

        let old_tile_preview = paths.tile_previews_dir.join(format!("{old_id}.png"));
        if old_tile_preview.exists() {
            let new_tile_preview = paths.tile_previews_dir.join(format!("{new_id}.png"));
            if old_tile_preview != new_tile_preview {
                if new_tile_preview.exists() {
                    let _ = std::fs::remove_file(&new_tile_preview);
                }
                std::fs::rename(&old_tile_preview, &new_tile_preview).map_err(|e| {
                    format!("renaming tile preview {}: {e}", old_tile_preview.display())
                })?;
            }
        }

        state.rename_id(old_id, &new_id);
        changed = true;
        eprintln!("[Migration] renamed skin '{old_id}' -> '{new_id}'");
    }

    if changed {
        state.save(&paths.state_path).map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
fn list_skins(app: AppHandle) -> Result<SkinLibrary, String> {
    let paths = resolve_paths(&app)?;
    let state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    let skins = library::scan(
        &paths.skins_dir,
        &paths.previews_dir,
        &paths.background_previews_dir,
        &paths.tile_previews_dir,
        &paths.champion_icons_dir,
        &state,
    )
    .map_err(|e| e.to_string())?;
    Ok(SkinLibrary {
        dir: paths.skins_dir.to_string_lossy().into_owned(),
        skins,
    })
}

#[tauri::command]
fn set_skin_enabled(app: AppHandle, id: String, enabled: bool) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    let mut state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    state.set(id, enabled);
    state.save(&paths.state_path).map_err(|e| e.to_string())?;
    regenerate_skin_index(&app);
    Ok(())
}

#[tauri::command]
fn open_skins_folder(app: AppHandle) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    // Make sure the folder exists before trying to open it — on a fresh
    // install the app data dir may not yet be created.
    std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;
    app.opener()
        .open_path(paths.skins_dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Removes a skin from the library: deletes its `.fantome` file, any
/// cached preview PNG, and clears its entry from the enabled-state file.
/// The `id` is the file stem used by the scan (see `library::scan`).
#[tauri::command]
fn delete_skin(app: AppHandle, id: String) -> Result<(), String> {
    let paths = resolve_paths(&app)?;

    let fantome_path = paths.skins_dir.join(format!("{id}.fantome"));
    if fantome_path.exists() {
        std::fs::remove_file(&fantome_path)
            .map_err(|e| format!("removing .fantome: {e}"))?;
    }

    let preview_path = paths.previews_dir.join(format!("{id}.png"));
    if preview_path.exists() {
        // Preview is just a cache — if the remove fails, don't fail the
        // whole delete (the .fantome is already gone, the scan will no
        // longer reference this id).
        let _ = std::fs::remove_file(&preview_path);
    }

    let background_preview_path = paths.background_previews_dir.join(format!("{id}.png"));
    if background_preview_path.exists() {
        let _ = std::fs::remove_file(&background_preview_path);
    }

    let tile_preview_path = paths.tile_previews_dir.join(format!("{id}.png"));
    if tile_preview_path.exists() {
        let _ = std::fs::remove_file(&tile_preview_path);
    }

    let mut state =
        SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    state.set(id, false);
    state
        .save(&paths.state_path)
        .map_err(|e| e.to_string())?;

    regenerate_skin_index(&app);
    Ok(())
}

/// Picks a destination path in the skins directory for a given `.fantome`
/// filename, appending a numeric suffix if a file with that name already exists.
fn pick_dest(skins_dir: &std::path::Path, filename: &str) -> PathBuf {
    let mut dest = skins_dir.join(filename);
    if !dest.exists() {
        return dest;
    }
    let stem = PathBuf::from(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "skin".into());
    for i in 1.. {
        let candidate = skins_dir.join(format!("{stem} ({i}).fantome"));
        if !candidate.exists() {
            dest = candidate;
            break;
        }
    }
    dest
}

/// Converts an incoming `.fantome` filename into a stable internal storage
/// name. We keep display names from the mod metadata, so the on-disk name can
/// be aggressively normalized for filesystem and URL safety.
fn normalize_import_filename(filename: &str) -> String {
    let filename_path = PathBuf::from(filename);
    let raw_stem = filename_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("skin");

    let mut normalized = String::with_capacity(raw_stem.len());
    let mut last_was_sep = false;
    for c in raw_stem.chars() {
        if c.is_ascii_alphanumeric() {
            normalized.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            normalized.push('-');
            last_was_sep = true;
        }
    }

    let normalized = normalized.trim_matches('-');
    let stem = if normalized.is_empty() {
        "skin"
    } else {
        normalized
    };

    format!("{stem}.fantome")
}

#[tauri::command]
fn import_skin(app: AppHandle, source: String) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;

    let source_path = PathBuf::from(&source);
    if !source_path.is_file() {
        return Err("selected path is not a file".into());
    }
    if source_path.extension().and_then(|e| e.to_str()) != Some("fantome") {
        return Err("file must have .fantome extension".into());
    }

    let file_name = source_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid file name".to_string())?;
    let normalized_name = normalize_import_filename(file_name);
    let dest = pick_dest(&paths.skins_dir, &normalized_name);

    std::fs::copy(&source_path, &dest).map_err(|e| e.to_string())?;
    regenerate_skin_index(&app);
    Ok(())
}

#[tauri::command]
fn import_skin_bytes(
    app: AppHandle,
    filename: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;

    if !filename.to_lowercase().ends_with(".fantome") {
        return Err("file must have .fantome extension".into());
    }

    let normalized_name = normalize_import_filename(&filename);
    let dest = pick_dest(&paths.skins_dir, &normalized_name);
    std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
    regenerate_skin_index(&app);
    Ok(())
}

#[tauri::command]
fn activate_pengu(app: AppHandle) -> Result<(), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let core_dll = pengu::resolve_core_dll_path(&resource_dir);
    let flag = pengu::flag_path(&data_dir);
    pengu::activate(&core_dll, &flag).map_err(|e| e.to_string())
}

#[tauri::command]
fn deactivate_pengu(app: AppHandle) -> Result<(), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let core_dll = pengu::resolve_core_dll_path(&resource_dir);
    let flag = pengu::flag_path(&data_dir);
    pengu::deactivate(&core_dll, &flag).map_err(|e| e.to_string())
}

#[tauri::command]
fn pengu_status(app: AppHandle) -> Result<bool, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(pengu::flag_path(&data_dir).exists())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            list_skins,
            set_skin_enabled,
            open_skins_folder,
            import_skin,
            import_skin_bytes,
            delete_skin,
            activate_pengu,
            deactivate_pengu,
            pengu_status
        ])
        .setup(|app| {
            let setup_handle = app.handle().clone();
            if let Ok(paths) = resolve_paths(&setup_handle) {
                if let Err(e) = migrate_existing_skin_filenames(&paths) {
                    eprintln!("[Migration] filename normalize failed: {}", e);
                }
            }

            if let (Ok(data_dir), Ok(resource_dir)) =
                (app.path().app_data_dir(), app.path().resource_dir())
            {
                let flag = pengu::flag_path(&data_dir);
                let core_dll = pengu::resolve_core_dll_path(&resource_dir);
                let _ = FLAG_PATH.set(flag.clone());
                let _ = CORE_DLL_PATH.set(core_dll.clone());

                pengu::cleanup_if_dirty(&core_dll, &flag);

                match pengu::activate(&core_dll, &flag) {
                    Ok(()) => eprintln!("[Pengu] auto-activated on startup"),
                    Err(e) => eprintln!("[Pengu] auto-activate failed: {}", e),
                }

                // Console-side Ctrl+C: installs a Win32 console handler so
                // the dev terminal's Ctrl+C runs cleanup before exit.
                // (Window-close path is handled by RunEvent::ExitRequested
                // in the run callback below.)
                if let Err(e) = ctrlc::set_handler(|| {
                    cleanup_on_exit();
                    std::process::exit(0);
                }) {
                    eprintln!("[Pengu] could not install Ctrl+C handler: {}", e);
                }
            }
            // One-shot Data Dragon fetch for the champion alias→id map,
            // then generate the initial skin index. Runs async so startup
            // isn't blocked on network I/O.
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match data_dragon::fetch_champion_map().await {
                        Ok(map) => {
                            eprintln!(
                                "[DataDragon] loaded {} champion entries",
                                map.len()
                            );
                            let _ = CHAMPION_MAP.set(map);
                            regenerate_skin_index(&handle);
                        }
                        Err(e) => {
                            eprintln!("[DataDragon] fetch failed: {}", e);
                        }
                    }
                });
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                lcu::run(handle).await;
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let RunEvent::ExitRequested { .. } = event {
                cleanup_on_exit();
            }
        });
}
