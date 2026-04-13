mod lcu;
mod pengu;
mod skins;
mod wad;

use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_opener::OpenerExt;

/// Path of the activation marker file. Stored here so the Ctrl+C handler
/// (which runs on a separate thread without an `AppHandle`) and the Tauri
/// `ExitRequested` callback can both find it at shutdown time.
static FLAG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Idempotent shutdown cleanup: deletes the IFEO key, removes the flag
/// file, and kills any running `LeagueClientUx.exe` so the parent
/// `LeagueClient.exe` respawns it cleanly without the injection hook.
fn cleanup_pengu_on_exit() {
    let Some(flag) = FLAG_PATH.get() else { return };
    match pengu::deactivate(flag) {
        Ok(()) => eprintln!("[Pengu] cleaned up on exit"),
        Err(e) => eprintln!("[Pengu] cleanup on exit failed: {}", e),
    }
}

use skins::library::{self, SkinLibrary};
use skins::state::SkinState;

struct AppPaths {
    skins_dir: PathBuf,
    state_path: PathBuf,
    previews_dir: PathBuf,
    champion_icons_dir: PathBuf,
}

fn resolve_paths(app: &AppHandle) -> Result<AppPaths, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(AppPaths {
        skins_dir: data_dir.join("skins"),
        state_path: data_dir.join("state.json"),
        previews_dir: data_dir.join("previews"),
        champion_icons_dir: data_dir.join("champion_icons"),
    })
}

#[tauri::command]
fn list_skins(app: AppHandle) -> Result<SkinLibrary, String> {
    let paths = resolve_paths(&app)?;
    let state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    let skins = library::scan(
        &paths.skins_dir,
        &paths.previews_dir,
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

    let mut state =
        SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    state.set(id, false);
    state
        .save(&paths.state_path)
        .map_err(|e| e.to_string())?;

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
    let dest = pick_dest(&paths.skins_dir, file_name);

    std::fs::copy(&source_path, &dest).map_err(|e| e.to_string())?;
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

    let dest = pick_dest(&paths.skins_dir, &filename);
    std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
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
    let flag = pengu::flag_path(&data_dir);
    pengu::deactivate(&flag).map_err(|e| e.to_string())
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
            if let (Ok(data_dir), Ok(resource_dir)) =
                (app.path().app_data_dir(), app.path().resource_dir())
            {
                let flag = pengu::flag_path(&data_dir);
                let _ = FLAG_PATH.set(flag.clone());
                pengu::cleanup_if_dirty(&flag);

                let core_dll = pengu::resolve_core_dll_path(&resource_dir);
                match pengu::activate(&core_dll, &flag) {
                    Ok(()) => eprintln!("[Pengu] auto-activated on startup"),
                    Err(e) => eprintln!("[Pengu] auto-activate failed: {}", e),
                }

                // Console-side Ctrl+C: installs a Win32 console handler so
                // the dev terminal's Ctrl+C runs cleanup before exit.
                // (Window-close path is handled by RunEvent::ExitRequested
                // in the run callback below.)
                if let Err(e) = ctrlc::set_handler(|| {
                    cleanup_pengu_on_exit();
                    std::process::exit(0);
                }) {
                    eprintln!("[Pengu] could not install Ctrl+C handler: {}", e);
                }
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
                cleanup_pengu_on_exit();
            }
        });
}
