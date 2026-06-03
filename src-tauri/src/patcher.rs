#[cfg(windows)]
mod imp {
    use anyhow::{anyhow, Context, Result};
    use std::ffi::{CStr, OsStr};
    use std::os::raw::{c_char, c_uint};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    const CSLOL_HOOK_DISABLE_NONE: c_uint = 0;
    const CSLOL_LOG_DEBUG: c_uint = 0x20;
    const HOOK_POST_ITERS: c_uint = 300_000;
    const HOOK_EVENT_ITERS: c_uint = 100;

    type CslolInit = unsafe extern "C" fn() -> *const c_char;
    type CslolSetFlags = unsafe extern "C" fn(c_uint) -> *const c_char;
    type CslolSetLogLevel = unsafe extern "C" fn(c_uint) -> *const c_char;
    type CslolSetConfig = unsafe extern "C" fn(*const u16) -> *const c_char;
    type CslolFind = unsafe extern "C" fn() -> c_uint;
    type CslolHook = unsafe extern "C" fn(c_uint, c_uint, c_uint) -> *const c_char;
    type CslolLogPull = unsafe extern "C" fn() -> *const c_char;

    #[derive(Clone, Copy)]
    struct Functions {
        init: CslolInit,
        set_flags: CslolSetFlags,
        set_log_level: CslolSetLogLevel,
        set_config: CslolSetConfig,
        find: CslolFind,
        hook: CslolHook,
        log_pull: CslolLogPull,
    }

    #[derive(Clone, Copy)]
    struct Dll {
        handle: usize,
        functions: Functions,
    }

    #[derive(Default)]
    struct State {
        dll: Option<Dll>,
        stop: Option<Arc<AtomicBool>>,
        worker: Option<JoinHandle<()>>,
    }

    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    static RUNNING: AtomicBool = AtomicBool::new(false);

    fn state() -> &'static Mutex<State> {
        STATE.get_or_init(|| Mutex::new(State::default()))
    }

    pub fn resolve_dll_path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join("cslol-tools").join("cslol-dll.dll")
    }

    pub fn load(dll_path: &Path) -> Result<()> {
        let mut state = state().lock().expect("patcher state lock poisoned");
        if state.dll.is_some() {
            return Ok(());
        }
        if !dll_path.is_file() {
            return Err(anyhow!("DLL not found at {}", dll_path.display()));
        }

        let dll_path_w = to_utf16_z(dll_path.as_os_str());
        let handle = unsafe { LoadLibraryW(dll_path_w.as_ptr()) };
        if handle.is_null() {
            return Err(anyhow!(
                "LoadLibraryW({}) failed: {}",
                dll_path.display(),
                std::io::Error::last_os_error()
            ));
        }

        let functions = match unsafe { load_functions(handle) } {
            Ok(functions) => functions,
            Err(e) => {
                unsafe {
                    FreeLibrary(handle);
                }
                return Err(e);
            }
        };

        state.dll = Some(Dll {
            handle: handle as usize,
            functions,
        });
        eprintln!("[Patcher] loaded {}", dll_path.display());
        Ok(())
    }

    pub fn start(overlay_path: PathBuf) -> Result<()> {
        let mut state = state().lock().expect("patcher state lock poisoned");
        if state
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(());
        }
        if let Some(worker) = state.worker.take() {
            let _ = worker.join();
            state.stop = None;
        }

        let dll = state
            .dll
            .ok_or_else(|| anyhow!("patcher DLL is not loaded"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let functions = dll.functions;
        let worker = thread::Builder::new()
            .name("watermelon-cslol-patcher".into())
            .spawn(move || run_loop(functions, overlay_path, thread_stop))
            .context("spawning cslol patcher thread")?;

        state.stop = Some(stop);
        state.worker = Some(worker);
        Ok(())
    }

    pub fn stop() {
        let (stop, worker) = {
            let mut state = state().lock().expect("patcher state lock poisoned");
            let stop = state.stop.take();
            let worker = state.worker.take();
            (stop, worker)
        };

        if let Some(stop) = stop {
            stop.store(true, Ordering::Release);
        }
        RUNNING.store(false, Ordering::Release);

        if let Some(worker) = worker {
            if worker.join().is_err() {
                eprintln!("[Patcher] patcher thread panicked during shutdown");
            }
        }
    }

    pub fn unload() {
        stop();
        let dll = {
            let mut state = state().lock().expect("patcher state lock poisoned");
            state.dll.take()
        };
        if let Some(dll) = dll {
            unsafe {
                FreeLibrary(dll.handle as HMODULE);
            }
            eprintln!("[Patcher] unloaded DLL");
        }
    }

    #[allow(dead_code)]
    pub fn is_running() -> bool {
        RUNNING.load(Ordering::Acquire)
    }

    unsafe fn load_functions(handle: HMODULE) -> Result<Functions> {
        Ok(Functions {
            init: load_symbol(handle, "cslol_init")?,
            set_flags: load_symbol(handle, "cslol_set_flags")?,
            set_log_level: load_symbol(handle, "cslol_set_log_level")?,
            set_config: load_symbol(handle, "cslol_set_config")?,
            find: load_symbol(handle, "cslol_find")?,
            hook: load_symbol(handle, "cslol_hook")?,
            log_pull: load_symbol(handle, "cslol_log_pull")?,
        })
    }

    unsafe fn load_symbol<T: Copy>(handle: HMODULE, name: &'static str) -> Result<T> {
        let mut symbol = Vec::with_capacity(name.len() + 1);
        symbol.extend_from_slice(name.as_bytes());
        symbol.push(0);

        let proc = GetProcAddress(handle, symbol.as_ptr())
            .ok_or_else(|| anyhow!("missing DLL symbol {}", name))?;
        debug_assert_eq!(
            std::mem::size_of::<T>(),
            std::mem::size_of_val(&proc),
            "function pointer sizes differ"
        );
        Ok(std::mem::transmute_copy(&proc))
    }

    fn run_loop(functions: Functions, overlay_path: PathBuf, stop: Arc<AtomicBool>) {
        let result = unsafe { run_loop_inner(functions, overlay_path, &stop) };
        if let Err(e) = result {
            eprintln!("[Patcher] {}", e);
        }
        RUNNING.store(false, Ordering::Release);
        eprintln!("[Patcher] loop exited");
    }

    unsafe fn run_loop_inner(
        functions: Functions,
        overlay_path: PathBuf,
        stop: &AtomicBool,
    ) -> Result<()> {
        let overlay_w = to_utf16_z(overlay_path.as_os_str());

        check_call("cslol_init", (functions.init)())?;
        check_call(
            "cslol_set_flags",
            (functions.set_flags)(CSLOL_HOOK_DISABLE_NONE),
        )?;
        check_call(
            "cslol_set_log_level",
            (functions.set_log_level)(CSLOL_LOG_DEBUG),
        )?;
        check_call(
            "cslol_set_config",
            (functions.set_config)(overlay_w.as_ptr()),
        )?;

        RUNNING.store(true, Ordering::Release);
        eprintln!(
            "[Patcher] initialized with overlay {}, waiting for League...",
            overlay_path.display()
        );

        while !stop.load(Ordering::Acquire) {
            pull_logs(functions);

            let tid = (functions.find)();
            if tid == 0 {
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            eprintln!("[Patcher] found League process thread {}, hooking...", tid);
            check_call(
                "cslol_hook",
                (functions.hook)(tid, HOOK_POST_ITERS, HOOK_EVENT_ITERS),
            )?;
            eprintln!("[Patcher] hooked League, waiting for game exit");

            while !stop.load(Ordering::Acquire) {
                pull_logs(functions);
                if (functions.find)() != tid {
                    eprintln!("[Patcher] League game process exited");
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
        }

        Ok(())
    }

    unsafe fn pull_logs(functions: Functions) {
        loop {
            let msg = (functions.log_pull)();
            if msg.is_null() {
                break;
            }
            eprintln!("[Patcher][DLL] {}", cstr_lossy(msg));
        }
    }

    unsafe fn check_call(name: &str, ptr: *const c_char) -> Result<()> {
        if ptr.is_null() {
            Ok(())
        } else {
            Err(anyhow!("{} failed: {}", name, cstr_lossy(ptr)))
        }
    }

    unsafe fn cstr_lossy(ptr: *const c_char) -> String {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }

    fn to_utf16_z(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{anyhow, Result};
    use std::path::{Path, PathBuf};

    pub fn resolve_dll_path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join("cslol-tools").join("cslol-dll.dll")
    }

    pub fn load(_dll_path: &Path) -> Result<()> {
        Err(anyhow!("cslol patcher is only supported on Windows"))
    }

    pub fn start(_overlay_path: PathBuf) -> Result<()> {
        Err(anyhow!("cslol patcher is only supported on Windows"))
    }

    pub fn stop() {}

    pub fn unload() {}

    #[allow(dead_code)]
    pub fn is_running() -> bool {
        false
    }
}

pub use imp::*;
