use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use super::game_paths::GamePathIndex;
use super::pipeline::{
    build_overlay_fast, restore_map_cache_patches, validate_map_cache_against_game,
};

/// Lowest ID the skin index assigns to user-imported custom skins
/// (`make_custom_id` in `skins/index.rs` uses the 9,000,000+ range).
/// Anything below this is a vanilla Riot skin we don't inject for.
const CUSTOM_SKIN_ID_BASE: i64 = 9_000_000;

#[derive(Debug, Clone)]
enum HoverRequest {
    /// Player unfocused the carousel / hovered a vanilla skin → drop
    /// whatever overlay is currently on disk.
    Clear,
    /// Player hovered a custom skin. The ID is decoded to a fantome path
    /// inside the worker so the producer (the websocket handler) never
    /// blocks on JSON parsing.
    Skin(i64),
}

/// Coordinates rebuilds of the WAD overlay in response to skin-hover
/// events from the in-client plugin. Only one build can be in flight at
/// a time; incoming requests coalesce onto "latest wins" so a user
/// scrubbing through the carousel doesn't queue up dozens of stale
/// builds.
pub struct HoverRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    app: AppHandle,
    skins_dir: PathBuf,
    skins_index_path: PathBuf,
    overlay_dir: PathBuf,
    game_paths: Mutex<Option<Arc<GamePathIndex>>>,
    latest: Mutex<Option<HoverRequest>>,
    wake: Condvar,
}

impl HoverRuntime {
    /// Spawns the worker thread and returns a handle. Call once during
    /// Tauri `setup()` and keep the result alive for the app's lifetime
    /// — dropping the `HoverRuntime` detaches the worker but doesn't
    /// stop it, so leaking is fine for the singleton use case.
    pub fn start(
        app: AppHandle,
        skins_dir: PathBuf,
        skins_index_path: PathBuf,
        overlay_dir: PathBuf,
    ) -> Self {
        let inner = Arc::new(Inner {
            app,
            skins_dir,
            skins_index_path,
            overlay_dir,
            game_paths: Mutex::new(None),
            latest: Mutex::new(None),
            wake: Condvar::new(),
        });

        let worker = inner.clone();
        thread::Builder::new()
            .name("talon-overlay-worker".into())
            .spawn(move || worker.run())
            .expect("spawning overlay worker thread");

        Self { inner }
    }

    /// Records the latest hover and wakes the worker. Non-blocking.
    ///
    /// `skin_id`:
    ///   * `Some(id)` with `id >= CUSTOM_SKIN_ID_BASE` → rebuild overlay
    ///     for that custom skin.
    ///   * `Some(id)` with a vanilla ID → ignored; leave the overlay
    ///     alone so the previously-hovered custom skin stays injected.
    ///   * `None` → ignored; the client clears hover during game launch,
    ///     so gameflow cleanup owns tearing down the prepared overlay.
    pub fn handle_hover(&self, skin_id: Option<i64>) {
        let request = match skin_id {
            None => {
                eprintln!("[Inject] hover=null → ignored");
                return;
            }
            Some(id) if id >= CUSTOM_SKIN_ID_BASE => {
                eprintln!("[Inject] hover={} (custom) → queue build", id);
                HoverRequest::Skin(id)
            }
            Some(id) => {
                eprintln!("[Inject] hover={} (vanilla) → ignored", id);
                return;
            }
        };
        let mut latest = self.inner.latest.lock().expect("hover runtime latest lock poisoned");
        let replaced = latest.is_some();
        *latest = Some(request);
        self.inner.wake.notify_one();
        if replaced {
            eprintln!("[Inject] coalesced — replacing pending request");
        }
    }
}

impl Inner {
    fn run(self: Arc<Self>) {
        eprintln!(
            "[Inject] worker started — overlay_dir={} skins_dir={} skins_index={}",
            self.overlay_dir.display(),
            self.skins_dir.display(),
            self.skins_index_path.display()
        );
        loop {
            // Wrap the whole iteration so a panic anywhere inside the
            // build path (bad zstd payload, corrupt fantome, poisoned
            // mutex) doesn't kill the worker thread and leave future
            // hover events silently queued forever.
            let me = self.clone();
            let result = catch_unwind(AssertUnwindSafe(move || {
                let request = me.wait_for_request();
                me.dispatch(request);
            }));
            if let Err(panic) = result {
                eprintln!("[Inject] worker iteration panicked: {:?}", panic);
            }
        }
    }

    fn dispatch(&self, request: HoverRequest) {
        match request {
            HoverRequest::Clear => {
                eprintln!("[Inject] dispatch=Clear");
                crate::patcher::stop();
                match clear_overlay_dir(&self.overlay_dir) {
                    Ok(()) => {
                        eprintln!(
                            "[Inject] cleared {}",
                            self.overlay_dir.display()
                        );
                        let _ = self.app.emit("overlay:cleared", ());
                    }
                    Err(e) => eprintln!("[Inject] clear failed: {}", e),
                }
            }
            HoverRequest::Skin(skin_id) => {
                eprintln!("[Inject] dispatch=Skin({})", skin_id);
                let started = Instant::now();
                match self.build_for_skin(skin_id) {
                    Ok(stem) => {
                        eprintln!(
                            "[Inject] === BUILD DONE === skin={} stem={} dir={} elapsed={:.2?}",
                            skin_id,
                            stem,
                            self.overlay_dir.display(),
                            started.elapsed()
                        );
                        match crate::patcher::start(self.overlay_dir.clone()) {
                            Ok(()) => eprintln!(
                                "[Patcher] started for overlay {}",
                                self.overlay_dir.display()
                            ),
                            Err(e) => {
                                eprintln!("[Patcher] start failed: {}", e);
                                let _ = self.app.emit(
                                    "overlay:patcher-failed",
                                    format!("{}: {}", skin_id, e),
                                );
                            }
                        }
                        let _ = self.app.emit("overlay:built", &stem);
                    }
                    Err(e) => {
                        eprintln!(
                            "[Inject] === BUILD FAILED === skin={} error={}",
                            skin_id, e
                        );
                        let _ = self.app.emit(
                            "overlay:failed",
                            format!("{}: {}", skin_id, e),
                        );
                    }
                }
            }
        }
    }

    fn wait_for_request(&self) -> HoverRequest {
        let mut latest = self.latest.lock().expect("hover runtime latest lock poisoned");
        loop {
            if let Some(req) = latest.take() {
                return req;
            }
            latest = self
                .wake
                .wait(latest)
                .expect("hover runtime wake condvar poisoned");
        }
    }

    fn build_for_skin(&self, skin_id: i64) -> Result<String> {
        eprintln!("[Inject] === BUILD START === skin={}", skin_id);
        eprintln!("[Inject] step 1/3: resolve skin id → fantome");
        let fantome = resolve_custom_skin_id(
            &self.skins_index_path,
            &self.skins_dir,
            skin_id,
        )?;
        let stem = fantome
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        eprintln!(
            "[Inject] resolved: skin {} → {} ({} bytes)",
            skin_id,
            fantome.display(),
            fs::metadata(&fantome).map(|m| m.len()).unwrap_or(0)
        );

        eprintln!("[Inject] step 2/3: game path index (cache or build)");
        let game_paths = self.get_or_build_game_paths()?;

        eprintln!(
            "[Inject] step 3/3: fast-mkoverlay against {} game mount(s)",
            game_paths.len()
        );
        build_overlay_fast(
            game_paths.as_ref(),
            &fantome,
            &self.overlay_dir,
            /* ignore_conflict */ true,
        )
        .with_context(|| format!("building overlay for {}", stem))?;
        Ok(stem)
    }

    fn get_or_build_game_paths(&self) -> Result<Arc<GamePathIndex>> {
        {
            let cache = self.game_paths.lock().expect("game paths cache poisoned");
            if let Some(index) = cache.as_ref() {
                eprintln!(
                    "[Inject] game paths cache hit ({} mounts)",
                    index.len()
                );
                return Ok(Arc::clone(index));
            }
        }
        // Release the cache lock before walking the filesystem — so a
        // concurrent hover arriving mid-walk doesn't spin on the mutex.
        eprintln!("[Inject] game paths cache miss — discovering game path");
        let game_path = discover_game_path()?;
        eprintln!(
            "[Inject] building game path index from {}",
            game_path.display()
        );
        let started = Instant::now();
        let index = GamePathIndex::build(&game_path)
            .with_context(|| format!("indexing {}", game_path.display()))?;
        if index.is_empty() {
            return Err(anyhow!(
                "no game WADs found under {}/DATA/FINAL — is this the correct install path?",
                game_path.display()
            ));
        }
        let arc = Arc::new(index);
        eprintln!(
            "[Inject] game path index ready: {} mounts, took {:.2?}",
            arc.len(),
            started.elapsed()
        );
        let mut cache = self.game_paths.lock().expect("game paths cache poisoned");
        *cache = Some(Arc::clone(&arc));
        Ok(arc)
    }
}

fn discover_game_path() -> Result<PathBuf> {
    let install = if let Some(saved) = crate::saved_league_install_dir() {
        eprintln!("[Inject] using saved League install dir {}", saved.display());
        saved
    } else {
        let detected = crate::lcu::process::find_install_directory()
            .context("locating LeagueClient.exe — is the client running?")?;
        eprintln!("[Inject] LeagueClient.exe parent = {}", detected.display());
        detected
    };
    // `find_install_directory` returns the directory that contains
    // `LeagueClient.exe`. The WADs live under that directory's `Game`
    // subfolder (`.../League of Legends/Game/DATA/FINAL/...`).
    let game = install.join("Game");
    if !game.join("DATA").join("FINAL").is_dir() {
        return Err(anyhow!(
            "expected DATA/FINAL under {} — wrong game folder layout?",
            game.display()
        ));
    }
    eprintln!("[Inject] game path = {}", game.display());
    Ok(game)
}

pub fn validate_map_cache_startup(overlay_dir: &Path) -> Result<()> {
    let game_path = discover_game_path()?;
    let index = GamePathIndex::build(&game_path)
        .with_context(|| format!("indexing {}", game_path.display()))?;
    validate_map_cache_against_game(&index, overlay_dir)?;
    Ok(())
}

/// Removes transient overlay files under `overlay_dir`, leaving cached
/// base `Maps/Shipping/Map*.wad.client` files in place so later runtime
/// injections can patch them in place instead of recopying multi-GB map
/// WADs every time.
pub fn clear_overlay_dir(overlay_dir: &Path) -> Result<()> {
    if !overlay_dir.exists() {
        return Ok(());
    }
    restore_map_cache_patches(overlay_dir)?;
    let mut removed = 0usize;
    let mut preserved = 0usize;
    for entry in fs::read_dir(overlay_dir)
        .with_context(|| format!("reading {}", overlay_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            removed += clear_overlay_subdir(&path, overlay_dir)?;
        } else {
            if is_cached_map_wad(&path, overlay_dir) {
                preserved += 1;
                continue;
            }
            if remove_transient_overlay_file(&path) {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        eprintln!("[Inject] removed {} entries from overlay dir", removed);
    }
    if preserved > 0 {
        eprintln!("[Inject] preserved {} cached map WAD(s)", preserved);
    }
    Ok(())
}

fn clear_overlay_subdir(dir: &Path, overlay_dir: &Path) -> Result<usize> {
    let mut removed = 0usize;
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            removed += clear_overlay_subdir(&path, overlay_dir)?;
            if fs::read_dir(&path)?.next().is_none() {
                let _ = fs::remove_dir(&path);
            }
        } else {
            if is_cached_map_wad(&path, overlay_dir) {
                continue;
            }
            if remove_transient_overlay_file(&path) {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn remove_transient_overlay_file(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(_) => false,
    }
}

fn is_cached_map_wad(path: &Path, overlay_dir: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(overlay_dir) else {
        return false;
    };
    let mut comps = rel.components();
    matches!(
        (
            comps.next(),
            comps.next(),
            comps.next(),
            comps.next(),
            comps.next(),
            comps.next()
        ),
        (
            Some(std::path::Component::Normal(a)),
            Some(std::path::Component::Normal(b)),
            Some(std::path::Component::Normal(c)),
            Some(std::path::Component::Normal(d)),
            Some(std::path::Component::Normal(e)),
            None
        ) if os_eq_ignore_ascii_case(a, "DATA")
            && os_eq_ignore_ascii_case(b, "FINAL")
            && os_eq_ignore_ascii_case(c, "Maps")
            && os_eq_ignore_ascii_case(d, "Shipping")
            && is_base_map_wad_filename(e)
    )
}

fn os_eq_ignore_ascii_case(value: &std::ffi::OsStr, expected: &str) -> bool {
    value
        .to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(expected))
}

fn is_base_map_wad_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if !name.starts_with("Map") || !name.ends_with(".wad.client") {
        return false;
    }
    let stem = &name[..name.len() - ".wad.client".len()];
    stem[3..].bytes().all(|b| b.is_ascii_digit())
}

/// Looks up a custom skin id in `skins_index.json` and returns the
/// absolute path of its backing `.fantome`. The index is regenerated by
/// `regenerate_skin_index` on every library mutation; we re-read it on
/// each hover (small JSON file) and retry once on parse error to cope
/// with a half-written state.
fn resolve_custom_skin_id(
    skins_index_path: &Path,
    skins_dir: &Path,
    skin_id: i64,
) -> Result<PathBuf> {
    if skin_id < CUSTOM_SKIN_ID_BASE {
        return Err(anyhow!(
            "skin id {} is not a Talon custom skin (< {})",
            skin_id,
            CUSTOM_SKIN_ID_BASE
        ));
    }
    let champion_id = (skin_id - CUSTOM_SKIN_ID_BASE) / 100;
    let within = (skin_id - CUSTOM_SKIN_ID_BASE) % 100;
    eprintln!(
        "[Inject] decode: skin {} → champion_id={} slot={}",
        skin_id, champion_id, within
    );
    let key = champion_id.to_string();

    let value = read_skins_index(skins_index_path)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("skins_index.json is not a JSON object"))?;
    let entries = object
        .get(&key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no entries for champion {} in skins index", champion_id))?;
    eprintln!(
        "[Inject] index has {} custom skin(s) for champion {}",
        entries.len(),
        champion_id
    );

    let entry = entries
        .iter()
        .find(|e| e.get("id").and_then(|v| v.as_i64()) == Some(skin_id))
        .ok_or_else(|| anyhow!("skin {} not found in skins index", skin_id))?;
    let file_stem = entry
        .get("fileStem")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("skin index entry {} missing fileStem", skin_id))?;

    let fantome = skins_dir.join(format!("{}.fantome", file_stem));
    if !fantome.is_file() {
        return Err(anyhow!("fantome file missing on disk: {}", fantome.display()));
    }
    Ok(fantome)
}

fn read_skins_index(path: &Path) -> Result<Value> {
    match try_read(path) {
        Ok(v) => Ok(v),
        Err(_) => {
            // `skins_index.json` is rewritten from scratch on every
            // library mutation, and on Windows an unlucky read that
            // races the write sees a truncated file. Sleep briefly and
            // retry once — if the second read also fails, propagate.
            thread::sleep(Duration::from_millis(50));
            try_read(path)
        }
    }
}

fn try_read(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(value)
}
