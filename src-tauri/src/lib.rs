mod lcu;
mod skins;

use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

use skins::library::{self, SkinLibrary};
use skins::state::SkinState;

fn resolve_paths(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let skins_dir = data_dir.join("skins");
    let state_path = data_dir.join("state.json");
    Ok((skins_dir, state_path))
}

#[tauri::command]
fn list_skins(app: AppHandle) -> Result<SkinLibrary, String> {
    let (skins_dir, state_path) = resolve_paths(&app)?;
    let state = SkinState::load(&state_path).map_err(|e| e.to_string())?;
    let skins = library::scan(&skins_dir, &state).map_err(|e| e.to_string())?;
    Ok(SkinLibrary {
        dir: skins_dir.to_string_lossy().into_owned(),
        skins,
    })
}

#[tauri::command]
fn set_skin_enabled(app: AppHandle, id: String, enabled: bool) -> Result<(), String> {
    let (_, state_path) = resolve_paths(&app)?;
    let mut state = SkinState::load(&state_path).map_err(|e| e.to_string())?;
    state.set(id, enabled);
    state.save(&state_path).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn open_skins_folder(app: AppHandle) -> Result<(), String> {
    let (skins_dir, _) = resolve_paths(&app)?;
    // Make sure the folder exists before trying to open it — on a fresh
    // install the app data dir may not yet be created.
    std::fs::create_dir_all(&skins_dir).map_err(|e| e.to_string())?;
    app.opener()
        .open_path(skins_dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
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
    let (skins_dir, _) = resolve_paths(&app)?;
    std::fs::create_dir_all(&skins_dir).map_err(|e| e.to_string())?;

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
    let dest = pick_dest(&skins_dir, file_name);

    std::fs::copy(&source_path, &dest).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn import_skin_bytes(
    app: AppHandle,
    filename: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let (skins_dir, _) = resolve_paths(&app)?;
    std::fs::create_dir_all(&skins_dir).map_err(|e| e.to_string())?;

    if !filename.to_lowercase().ends_with(".fantome") {
        return Err("file must have .fantome extension".into());
    }

    let dest = pick_dest(&skins_dir, &filename);
    std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
    Ok(())
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
            import_skin_bytes
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                lcu::run(handle).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
