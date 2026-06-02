mod bridge;
mod data_dragon;
mod lcu;
mod overlay;
mod patcher;
mod pengu;
mod skins;
mod wad;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager, RunEvent};
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
/// Kept alive for the process lifetime so the spawned worker thread
/// and any retained Arc clones inside the bridge task never race on
/// a drop. Also gives the shutdown hooks a handle for clearing the
/// overlay directory so next cold-start doesn't reload stale WADs.
static HOVER_RUNTIME: OnceLock<Arc<overlay::HoverRuntime>> = OnceLock::new();
/// Resolved overlay directory. Separate from `HOVER_RUNTIME` so cleanup
/// works even if runtime construction failed.
static OVERLAY_DIR: OnceLock<PathBuf> = OnceLock::new();
static LEAGUE_INSTALL_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static ASSET_WARMING: AtomicBool = AtomicBool::new(false);
static INJECTION_SERVICES_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppConfig {
    league_install_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CslolDllStatus {
    path: String,
    exists: bool,
}

fn league_install_dir_cell() -> &'static Mutex<Option<PathBuf>> {
    LEAGUE_INSTALL_DIR.get_or_init(|| Mutex::new(None))
}

pub(crate) fn saved_league_install_dir() -> Option<PathBuf> {
    league_install_dir_cell()
        .lock()
        .expect("league install dir lock poisoned")
        .clone()
}

fn talon_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let app_data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let Some(parent) = app_data_dir.parent() else {
        return Err(format!(
            "cannot resolve Talon data dir parent from {}",
            app_data_dir.display()
        ));
    };
    Ok(parent.join("Talon"))
}

fn app_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let data_dir = talon_data_dir(app)?;
    Ok(data_dir.join("settings").join("config.json"))
}

fn cslol_dll_dir_path(app: &AppHandle) -> Result<PathBuf, String> {
    let data_dir = talon_data_dir(app)?;
    Ok(data_dir.join("cslol-tools"))
}

fn cslol_dll_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(cslol_dll_dir_path(app)?.join("cslol-dll.dll"))
}

fn load_app_config(app: &AppHandle) -> Result<AppConfig, String> {
    let path = app_config_path(app)?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn save_app_config(app: &AppHandle, config: &AppConfig) -> Result<(), String> {
    let path = app_config_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn normalize_league_install_dir(path: &std::path::Path) -> Result<PathBuf, String> {
    let candidate = if path.join("Game").join("DATA").join("FINAL").is_dir() {
        path.to_path_buf()
    } else if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("Game"))
        && path.join("DATA").join("FINAL").is_dir()
    {
        path.parent()
            .ok_or_else(|| format!("{} has no parent install dir", path.display()))?
            .to_path_buf()
    } else {
        return Err(format!(
            "{} does not look like a League install (missing Game/DATA/FINAL)",
            path.display()
        ));
    };
    candidate
        .canonicalize()
        .map_err(|e| format!("canonicalizing {}: {e}", candidate.display()))
}

fn path_to_user_string(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(stripped) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{stripped}");
        }
        if let Some(stripped) = path.strip_prefix(r"\\?\") {
            return stripped.to_string();
        }
    }
    path.into_owned()
}

fn set_saved_league_install_dir(path: Option<PathBuf>) {
    *league_install_dir_cell()
        .lock()
        .expect("league install dir lock poisoned") = path;
}

/// Idempotent shutdown cleanup. Deactivates pengu (deletes IFEO key if it's
/// still ours, removes the flag file, kills `LeagueClientUx.exe` so the
/// parent respawns it without injection) AND kills any leftover Vite dev
/// server on port 1420 — tauri dev's child cleanup doesn't always propagate
/// Ctrl+C to vite on Windows, so without this the next `pnpm tauri dev`
/// fails with "Port 1420 is already in use".
fn cleanup_on_exit() {
    if let (Some(core_dll), Some(flag)) = (CORE_DLL_PATH.get(), FLAG_PATH.get()) {
        if flag.exists() {
            match pengu::deactivate(core_dll, flag) {
                Ok(()) => eprintln!("[Pengu] cleaned up active hook on exit"),
                Err(e) => eprintln!("[Pengu] cleanup on exit failed: {}", e),
            }
        } else {
            eprintln!("[Pengu] hook inactive on exit; leaving LeagueClientUx alone");
        }
    }
    cleanup_overlay_session("exit");
    patcher::unload();
    kill_dev_server_port();
}

fn install_ctrlc_handler_for_mode() {
    let install = if cfg!(debug_assertions) {
        ctrlc::set_handler(|| {
            cleanup_overlay_session("dev Ctrl+C");
            patcher::unload();
            spawn_detached_dev_cleanup();
            std::process::exit(0);
        })
    } else {
        ctrlc::set_handler(|| {
            cleanup_on_exit();
            std::process::exit(0);
        })
    };

    if let Err(e) = install {
        eprintln!("[Pengu] could not install Ctrl+C handler: {}", e);
    }
}

#[cfg(windows)]
fn spawn_detached_dev_cleanup() {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let core_dll = CORE_DLL_PATH
        .get()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let flag = FLAG_PATH
        .get()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let script = format!(
        "$core = {core}; \
         $flag = {flag}; \
         $subkey = 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options\\LeagueClientUx.exe'; \
         $active = $flag -and (Test-Path -LiteralPath $flag); \
         if ($active) {{ try {{ \
             $debugger = (Get-ItemProperty -Path $subkey -Name Debugger -ErrorAction SilentlyContinue).Debugger; \
             if ($debugger -and $debugger.ToLower().Contains($core.ToLower())) {{ \
                 Remove-Item -Path $subkey -Recurse -Force -ErrorAction SilentlyContinue; \
             }} \
         }} catch {{}}; \
         try {{ Remove-Item -LiteralPath $flag -Force -ErrorAction SilentlyContinue; }} catch {{}}; \
         try {{ Get-Process LeagueClientUx -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue; }} catch {{}} }}",
        core = ps_single_quoted(&core_dll),
        flag = ps_single_quoted(&flag),
    );

    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(windows))]
fn spawn_detached_dev_cleanup() {}

fn ps_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn write_storage_readme(data_dir: &std::path::Path) -> Result<(), String> {
    const README: &str = "\
Talon app data

settings/
  config.json                    User preferences such as the League install path.

library/
  skins/                         Imported .fantome skin files.
  state.json                     Enabled/disabled skin state.
  skins_index.json               Generated in-game carousel index.

user-assets/
  backgrounds/                   Custom background images chosen by the user.
  tiles/                         Custom tile images chosen by the user.

cache/
  previews/splash/               Generated splash previews.
  previews/background/           Generated carousel backgrounds.
  previews/tile/                 Generated champion tile images.
  champion-icons/                Cached Data Dragon champion icons.
cslol-installed/               Legacy/generated cslol install cache.

cslol-tools/
  cslol-dll.dll                User-supplied runtime hook DLL.

runtime/
  overlay/                       Temporary game overlay files.
  pengu.flag                     Injection activation marker.
  overlay.config                 Legacy runtime marker.
";
    let path = data_dir.join("README.txt");
    std::fs::write(&path, README).map_err(|e| format!("writing {}: {e}", path.display()))
}

pub(crate) fn cleanup_overlay_session(reason: &str) {
    patcher::stop();
    if let Some(overlay_dir) = OVERLAY_DIR.get() {
        match overlay::runtime::clear_overlay_dir(overlay_dir) {
            Ok(()) => eprintln!("[Overlay] cleared on {}", reason),
            Err(e) => eprintln!("[Overlay] cleanup on {} failed: {}", reason, e),
        }
    }
}

/// Rebuilds the library skin index from the current library
/// scan + state + champion map. Called on every skin-mutating command so
/// `core.dll`'s talon scheme handler always serves up-to-date data.
/// Best-effort: logs errors but never fails the caller. If the champion
/// map isn't loaded yet (Data Dragon fetch still in flight), this is a
/// no-op and the file will be written later when the fetch completes.
fn regenerate_skin_index(app: &AppHandle) {
    let Ok(paths) = resolve_paths(app) else {
        return;
    };
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
        &paths.skins_index_path,
        &paths.previews_dir,
        &paths.background_previews_dir,
        &paths.custom_background_previews_dir,
        &paths.tile_previews_dir,
        &paths.custom_tile_previews_dir,
        &paths.champion_icons_dir,
        &state,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[SkinIndex] library scan failed: {}", e);
            return;
        }
    };
    if let Err(e) = skins::index::regenerate(&paths.skins_index_path, &skins, &state, champion_map)
    {
        eprintln!("[SkinIndex] regenerate failed: {}", e);
    } else {
        eprintln!(
            "[SkinIndex] regenerated {} entries -> {}",
            skins.len(),
            paths.skins_index_path.display()
        );
    }
}

fn warm_assets_for_library(app: &AppHandle) -> Result<bool, String> {
    let paths = resolve_paths(app)?;
    std::fs::create_dir_all(&paths.previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.background_previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.tile_previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.champion_icons_dir).map_err(|e| e.to_string())?;

    let mut changed = false;
    let entries = std::fs::read_dir(&paths.skins_dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("fantome") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let meta = skins::fantome::read(&path).ok();
        let champion = meta.as_ref().and_then(|m| m.champion.as_deref());

        if skins::preview::warm_all_cached_assets(
            &path,
            &paths.previews_dir,
            &paths.background_previews_dir,
            &paths.tile_previews_dir,
            &paths.champion_icons_dir,
            &stem,
            champion,
        )
        .map_err(|e| e.to_string())?
        {
            changed = true;
        }
    }

    Ok(changed)
}

fn warm_assets_for_skin(app: &AppHandle, path: &std::path::Path) -> Result<bool, String> {
    let paths = resolve_paths(app)?;
    std::fs::create_dir_all(&paths.previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.background_previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.tile_previews_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.champion_icons_dir).map_err(|e| e.to_string())?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("invalid skin path: {}", path.display()))?;
    let meta = skins::fantome::read(path).ok();
    let champion = meta.as_ref().and_then(|m| m.champion.as_deref());

    skins::preview::warm_all_cached_assets(
        path,
        &paths.previews_dir,
        &paths.background_previews_dir,
        &paths.tile_previews_dir,
        &paths.champion_icons_dir,
        stem,
        champion,
    )
    .map_err(|e| e.to_string())
}

/// Releases `ASSET_WARMING` on drop so a panic inside the warmup task
/// doesn't strand the flag in the "in progress" state forever.
struct WarmupGuard;

impl Drop for WarmupGuard {
    fn drop(&mut self) {
        ASSET_WARMING.store(false, Ordering::Release);
    }
}

fn spawn_asset_warmup(app: AppHandle) {
    if ASSET_WARMING.swap(true, Ordering::AcqRel) {
        return;
    }

    tauri::async_runtime::spawn_blocking(move || {
        let _guard = WarmupGuard;
        let changed = match warm_assets_for_library(&app) {
            Ok(changed) => changed,
            Err(e) => {
                eprintln!("[Assets] warmup failed: {}", e);
                false
            }
        };
        if changed {
            regenerate_skin_index(&app);
            let _ = app.emit("library:assets-updated", ());
        }
    });
}

fn spawn_champ_select_refresh() {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = lcu::refresh_champ_select().await {
            eprintln!("[LCU] champ-select refresh failed: {}", e);
        }
    });
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
    skins_index_path: PathBuf,
    overlay_dir: PathBuf,
    previews_dir: PathBuf,
    background_previews_dir: PathBuf,
    custom_background_previews_dir: PathBuf,
    tile_previews_dir: PathBuf,
    custom_tile_previews_dir: PathBuf,
    champion_icons_dir: PathBuf,
}

fn resolve_paths(app: &AppHandle) -> Result<AppPaths, String> {
    let data_dir = talon_data_dir(app)?;
    write_storage_readme(&data_dir)?;
    let library_dir = data_dir.join("library");
    let preview_cache_dir = data_dir.join("cache").join("previews");
    let user_assets_dir = data_dir.join("user-assets");
    let runtime_dir = data_dir.join("runtime");
    Ok(AppPaths {
        skins_dir: library_dir.join("skins"),
        state_path: library_dir.join("state.json"),
        skins_index_path: library_dir.join("skins_index.json"),
        overlay_dir: runtime_dir.join("overlay"),
        previews_dir: preview_cache_dir.join("splash"),
        background_previews_dir: preview_cache_dir.join("background"),
        custom_background_previews_dir: user_assets_dir.join("backgrounds"),
        tile_previews_dir: preview_cache_dir.join("tile"),
        custom_tile_previews_dir: user_assets_dir.join("tiles"),
        champion_icons_dir: data_dir.join("cache").join("champion-icons"),
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
            .map(str::to_string)
        else {
            continue;
        };

        if old_id == normalized_stem {
            continue;
        }

        let target_path = pick_dest(&paths.skins_dir, &normalized_name);
        let Some(new_id) = target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        if let Err(e) = std::fs::rename(&source_path, &target_path) {
            eprintln!(
                "[Migration] skipping {}: rename failed: {e}",
                source_path.display()
            );
            continue;
        }

        // Skin file + enabled state are the atomic unit: once the rename
        // lands, we must update state in lockstep or the skin will appear
        // disabled next startup. Preview renames come after and are
        // best-effort — a stale/missing preview regenerates automatically.
        state.rename_id(old_id, &new_id);
        changed = true;

        soft_rename_preview(&paths.previews_dir, old_id, &new_id, "preview");
        soft_rename_preview(
            &paths.background_previews_dir,
            old_id,
            &new_id,
            "background",
        );
        soft_rename_preview(&paths.tile_previews_dir, old_id, &new_id, "tile");

        eprintln!("[Migration] renamed skin '{old_id}' -> '{new_id}'");
    }

    if changed {
        state.save(&paths.state_path).map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn soft_rename_preview(dir: &std::path::Path, old_id: &str, new_id: &str, kind: &str) {
    let old = dir.join(format!("{old_id}.png"));
    if !old.exists() {
        return;
    }
    let new = dir.join(format!("{new_id}.png"));
    if old == new {
        return;
    }
    if new.exists() {
        let _ = std::fs::remove_file(&new);
    }
    if let Err(e) = std::fs::rename(&old, &new) {
        eprintln!(
            "[Migration] {kind} rename {} -> {} failed: {e}",
            old.display(),
            new.display()
        );
    }
}

#[tauri::command]
fn list_skins(app: AppHandle) -> Result<SkinLibrary, String> {
    let paths = resolve_paths(&app)?;
    let state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    let skins = library::scan(
        &paths.skins_dir,
        &paths.skins_index_path,
        &paths.previews_dir,
        &paths.background_previews_dir,
        &paths.custom_background_previews_dir,
        &paths.tile_previews_dir,
        &paths.custom_tile_previews_dir,
        &paths.champion_icons_dir,
        &state,
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "[Library] scanned {} skin(s) from {}",
        skins.len(),
        paths.skins_dir.display()
    );
    spawn_asset_warmup(app.clone());
    Ok(SkinLibrary {
        dir: paths.skins_dir.to_string_lossy().into_owned(),
        skins,
    })
}

#[tauri::command]
fn set_skin_enabled(app: AppHandle, id: String, enabled: bool) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    let mut state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    state.set(id.clone(), enabled);
    state.save(&paths.state_path).map_err(|e| e.to_string())?;
    eprintln!(
        "[Library] set enabled={} for skin id='{}' via {}",
        enabled,
        id,
        paths.state_path.display()
    );
    regenerate_skin_index(&app);
    spawn_champ_select_refresh();
    Ok(())
}

#[tauri::command]
fn open_skins_folder(app: AppHandle) -> Result<(), String> {
    let paths = resolve_paths(&app)?;
    // Make sure the folder exists before trying to open it — on a fresh
    // install the app data dir may not yet be created.
    std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;
    eprintln!("[Library] opening skins folder {}", paths.skins_dir.display());
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
    eprintln!("[Library] deleting skin id='{}'", id);

    let fantome_path = paths.skins_dir.join(format!("{id}.fantome"));
    if fantome_path.exists() {
        std::fs::remove_file(&fantome_path).map_err(|e| format!("removing .fantome: {e}"))?;
        eprintln!("[Library] removed mod file {}", fantome_path.display());
    }

    // Cached / derived files — the .fantome is already gone so we don't
    // care if any of these fail; the scan will no longer reference this id.
    for cached in [
        paths.previews_dir.join(format!("{id}.png")),
        paths.background_previews_dir.join(format!("{id}.png")),
        paths
            .custom_background_previews_dir
            .join(format!("{id}.png")),
        paths.tile_previews_dir.join(format!("{id}.png")),
        paths.custom_tile_previews_dir.join(format!("{id}.png")),
    ] {
        if cached.exists() {
            let _ = std::fs::remove_file(&cached);
            eprintln!("[Library] removed cached asset {}", cached.display());
        }
    }

    let mut state = SkinState::load(&paths.state_path).map_err(|e| e.to_string())?;
    state.set(id, false);
    state.save(&paths.state_path).map_err(|e| e.to_string())?;
    eprintln!("[Library] deletion complete");

    regenerate_skin_index(&app);
    spawn_champ_select_refresh();
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
async fn import_skin(app: AppHandle, source: String) -> Result<(), String> {
    let task_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let paths = resolve_paths(&task_app)?;
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
        let imported_id = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<invalid>");
        eprintln!(
            "[Library] importing file source={} normalized={} id='{}' dest={}",
            source_path.display(),
            normalized_name,
            imported_id,
            dest.display()
        );

        std::fs::copy(&source_path, &dest).map_err(|e| e.to_string())?;
        let warmed = warm_assets_for_skin(&task_app, &dest)?;
        eprintln!(
            "[Library] import complete id='{}' warmed_assets={}",
            imported_id,
            warmed
        );
        regenerate_skin_index(&task_app);
        if warmed {
            let _ = task_app.emit("library:assets-updated", ());
        }
        spawn_champ_select_refresh();
        spawn_asset_warmup(task_app.clone());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn import_skin_bytes(app: AppHandle, filename: String, bytes: Vec<u8>) -> Result<(), String> {
    let task_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let paths = resolve_paths(&task_app)?;
        std::fs::create_dir_all(&paths.skins_dir).map_err(|e| e.to_string())?;

        if !filename.to_lowercase().ends_with(".fantome") {
            return Err("file must have .fantome extension".into());
        }

        let normalized_name = normalize_import_filename(&filename);
        let dest = pick_dest(&paths.skins_dir, &normalized_name);
        let imported_id = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<invalid>");
        eprintln!(
            "[Library] importing bytes filename={} size={} normalized={} id='{}' dest={}",
            filename,
            bytes.len(),
            normalized_name,
            imported_id,
            dest.display()
        );
        std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
        let warmed = warm_assets_for_skin(&task_app, &dest)?;
        eprintln!(
            "[Library] byte import complete id='{}' warmed_assets={}",
            imported_id,
            warmed
        );
        regenerate_skin_index(&task_app);
        if warmed {
            let _ = task_app.emit("library:assets-updated", ());
        }
        spawn_champ_select_refresh();
        spawn_asset_warmup(task_app.clone());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_custom_tile(app: AppHandle, id: String, source: String) -> Result<(), String> {
    set_custom_asset(app, id, source, CustomAssetKind::Tile).await
}

#[tauri::command]
async fn set_custom_background(app: AppHandle, id: String, source: String) -> Result<(), String> {
    set_custom_asset(app, id, source, CustomAssetKind::Background).await
}

#[tauri::command]
fn clear_custom_tile(app: AppHandle, id: String) -> Result<(), String> {
    clear_custom_asset(&app, &id, CustomAssetKind::Tile)
}

#[tauri::command]
fn clear_custom_background(app: AppHandle, id: String) -> Result<(), String> {
    clear_custom_asset(&app, &id, CustomAssetKind::Background)
}

#[derive(Copy, Clone)]
enum CustomAssetKind {
    Tile,
    Background,
}

fn custom_asset_dest(paths: &AppPaths, kind: CustomAssetKind, id: &str) -> PathBuf {
    let dir = match kind {
        CustomAssetKind::Tile => &paths.custom_tile_previews_dir,
        CustomAssetKind::Background => &paths.custom_background_previews_dir,
    };
    dir.join(format!("{id}.png"))
}

async fn set_custom_asset(
    app: AppHandle,
    id: String,
    source: String,
    kind: CustomAssetKind,
) -> Result<(), String> {
    let task_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let paths = resolve_paths(&task_app)?;
        let parent = match kind {
            CustomAssetKind::Tile => &paths.custom_tile_previews_dir,
            CustomAssetKind::Background => &paths.custom_background_previews_dir,
        };
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;

        let source_bytes = std::fs::read(&source).map_err(|e| format!("reading {source}: {e}"))?;
        let dest = custom_asset_dest(&paths, kind, &id);
        let kind_label = match kind {
            CustomAssetKind::Tile => "tile",
            CustomAssetKind::Background => "background",
        };
        eprintln!(
            "[Library] setting custom {} for id='{}' source={} bytes={} dest={}",
            kind_label,
            id,
            source,
            source_bytes.len(),
            dest.display()
        );
        match kind {
            CustomAssetKind::Tile => {
                skins::preview::save_custom_tile(&source_bytes, &dest).map_err(|e| e.to_string())?
            }
            CustomAssetKind::Background => {
                skins::preview::save_custom_background(&source_bytes, &dest)
                    .map_err(|e| e.to_string())?
            }
        }

        regenerate_skin_index(&task_app);
        spawn_champ_select_refresh();
        let _ = task_app.emit("library:assets-updated", ());
        eprintln!(
            "[Library] custom {} updated for id='{}'",
            kind_label,
            id
        );
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn clear_custom_asset(app: &AppHandle, id: &str, kind: CustomAssetKind) -> Result<(), String> {
    let paths = resolve_paths(app)?;
    let dest = custom_asset_dest(&paths, kind, id);
    let kind_label = match kind {
        CustomAssetKind::Tile => "tile",
        CustomAssetKind::Background => "background",
    };
    if dest.exists() {
        std::fs::remove_file(&dest).map_err(|e| e.to_string())?;
        eprintln!(
            "[Library] cleared custom {} for id='{}' at {}",
            kind_label,
            id,
            dest.display()
        );
    } else {
        eprintln!(
            "[Library] clear custom {} skipped for id='{}' (missing {})",
            kind_label,
            id,
            dest.display()
        );
    }
    regenerate_skin_index(app);
    spawn_champ_select_refresh();
    // Revealing the auto asset — if warmup never ran for this skin,
    // kick it so the fallback file actually lands on disk.
    spawn_asset_warmup(app.clone());
    let _ = app.emit("library:assets-updated", ());
    Ok(())
}

#[tauri::command]
fn get_league_install_path(app: AppHandle) -> Result<Option<String>, String> {
    let config = load_app_config(&app)?;
    Ok(config
        .league_install_dir
        .map(|path| path_to_user_string(std::path::Path::new(&path))))
}

#[tauri::command]
fn get_cslol_dll_status(app: AppHandle) -> Result<CslolDllStatus, String> {
    let path = cslol_dll_path(&app)?;
    Ok(CslolDllStatus {
        path: path_to_user_string(&path),
        exists: path.is_file(),
    })
}

#[tauri::command]
fn open_cslol_dll_folder(app: AppHandle) -> Result<String, String> {
    let dir = cslol_dll_dir_path(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())?;
    Ok(path_to_user_string(&dir))
}

#[tauri::command]
fn set_league_install_path(app: AppHandle, path: String) -> Result<String, String> {
    let normalized = normalize_league_install_dir(std::path::Path::new(&path))?;
    let user_path = path_to_user_string(&normalized);
    let mut config = load_app_config(&app)?;
    config.league_install_dir = Some(user_path.clone());
    save_app_config(&app, &config)?;
    set_saved_league_install_dir(Some(normalized.clone()));
    Ok(user_path)
}

#[tauri::command]
fn detect_league_install_path(app: AppHandle) -> Result<Option<String>, String> {
    let detected = match crate::lcu::process::find_install_directory() {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let normalized = normalize_league_install_dir(&detected)?;
    let normalized_str = path_to_user_string(&normalized);
    let mut config = load_app_config(&app)?;
    if config.league_install_dir.as_deref() != Some(normalized_str.as_str()) {
        config.league_install_dir = Some(normalized_str.clone());
        save_app_config(&app, &config)?;
    }
    set_saved_league_install_dir(Some(normalized));
    Ok(Some(normalized_str))
}

#[tauri::command]
fn activate_pengu(app: AppHandle) -> Result<(), String> {
    let data_dir = talon_data_dir(&app)?;
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let core_dll = pengu::resolve_core_dll_path(&resource_dir);
    let cslol_dll = cslol_dll_path(&app)?;
    if !cslol_dll.is_file() {
        return Err(format!(
            "Missing cslol-dll.dll at {}. Open the cslol-tools folder from setup and add the file first.",
            path_to_user_string(&cslol_dll)
        ));
    }
    let flag = pengu::flag_path(&data_dir);
    let _ = FLAG_PATH.set(flag.clone());
    let _ = CORE_DLL_PATH.set(core_dll.clone());
    pengu::activate(&core_dll, &flag).map_err(|e| e.to_string())?;
    start_injection_services(&app);
    Ok(())
}

#[tauri::command]
fn deactivate_pengu(app: AppHandle) -> Result<(), String> {
    let data_dir = talon_data_dir(&app)?;
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let core_dll = pengu::resolve_core_dll_path(&resource_dir);
    let flag = pengu::flag_path(&data_dir);
    pengu::deactivate(&core_dll, &flag).map_err(|e| e.to_string())
}

#[tauri::command]
fn pengu_status(app: AppHandle) -> Result<bool, String> {
    let data_dir = talon_data_dir(&app)?;
    Ok(pengu::flag_path(&data_dir).exists())
}

fn start_injection_services(app: &AppHandle) {
    if INJECTION_SERVICES_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let data_dir = match talon_data_dir(app) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[Inject] app data dir unavailable, injection disabled: {}", e);
            return;
        }
    };
    let cslol_dll = patcher::resolve_dll_path(&data_dir);
    if let Err(e) = patcher::load(&cslol_dll) {
        eprintln!("[Patcher] load failed: {}", e);
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        lcu::run(handle).await;
    });

    match resolve_paths(app) {
        Ok(paths) => {
            if let Err(e) = std::fs::create_dir_all(&paths.overlay_dir) {
                eprintln!(
                    "[Overlay] create {} failed: {}",
                    paths.overlay_dir.display(),
                    e
                );
            }
            if let Err(e) = overlay::runtime::clear_overlay_dir(&paths.overlay_dir) {
                eprintln!(
                    "[Overlay] startup cleanup {} failed: {}",
                    paths.overlay_dir.display(),
                    e
                );
            }
            let overlay_dir = paths.overlay_dir.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = overlay::runtime::validate_map_cache_startup(&overlay_dir) {
                    eprintln!("[Overlay] startup map-cache validation skipped: {}", e);
                }
            });
            let _ = OVERLAY_DIR.set(paths.overlay_dir.clone());
            let runtime = Arc::new(overlay::HoverRuntime::start(
                app.clone(),
                paths.skins_dir,
                paths.skins_index_path,
                paths.overlay_dir,
            ));
            let _ = HOVER_RUNTIME.set(runtime.clone());

            let bridge_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                bridge::run(bridge_handle, runtime).await;
            });
        }
        Err(e) => {
            eprintln!(
                "[Overlay] could not resolve app paths, hover injection disabled: {}",
                e
            );
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let no_inject = std::env::args().any(|a| a == "--no-inject");

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
            set_custom_tile,
            set_custom_background,
            clear_custom_tile,
            clear_custom_background,
            get_league_install_path,
            get_cslol_dll_status,
            open_cslol_dll_folder,
            set_league_install_path,
            detect_league_install_path,
            activate_pengu,
            deactivate_pengu,
            pengu_status
        ])
        .setup(move |app| {
            let setup_handle = app.handle().clone();
            if let Ok(paths) = resolve_paths(&setup_handle) {
                if let Err(e) = migrate_existing_skin_filenames(&paths) {
                    eprintln!("[Migration] filename normalize failed: {}", e);
                }
            }
            if let Ok(config) = load_app_config(&setup_handle) {
                let saved = config
                    .league_install_dir
                    .as_deref()
                    .and_then(|p| normalize_league_install_dir(std::path::Path::new(p)).ok());
                set_saved_league_install_dir(saved);
            }

            if !no_inject {
                install_ctrlc_handler_for_mode();
            } else {
                eprintln!("[Pengu] --no-inject: skipping activation and LCU poller");
            }
            // One-shot Data Dragon fetch for the champion alias→id map,
            // then generate the initial skin index. Runs async so startup
            // isn't blocked on network I/O.
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match data_dragon::fetch_champion_map().await {
                        Ok(map) => {
                            eprintln!("[DataDragon] loaded {} champion entries", map.len());
                            let _ = CHAMPION_MAP.set(map);
                            regenerate_skin_index(&handle);
                        }
                        Err(e) => {
                            eprintln!("[DataDragon] fetch failed: {}", e);
                        }
                    }
                });
            }

            spawn_asset_warmup(app.handle().clone());
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
