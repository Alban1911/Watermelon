use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn main() {
    println!("cargo:rerun-if-env-changed=DEBUG");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        build_core_dll();
    }

    tauri_build::build();
}

const SOURCES: &[&str] = &[
    "dllmain.cc",
    "libcef.cc",
    "config.cc",
    "browser/browser.cc",
    "browser/assets.cc",
    "browser/talon.cc",
    "renderer/renderer.cc",
    "renderer/v8_helper.cc",
    "utils/dylib.cc",
    "utils/cefstr.cc",
    "utils/file.cc",
];

fn build_core_dll() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let native_dir = manifest_dir.join("native");
    let src_dir = native_dir.join("src");
    let cef_dir = native_dir.join("cef");
    let resources_dir = manifest_dir.join("resources");
    let out_dll = resources_dir.join("core.dll");
    let obj_dir = manifest_dir.join("target").join("native_obj");

    println!("cargo:rerun-if-changed={}", native_dir.display());

    if !needs_rebuild(&out_dll, &src_dir) {
        return;
    }

    std::fs::create_dir_all(&resources_dir).unwrap();
    std::fs::create_dir_all(&obj_dir).unwrap();

    let tool = cc::Build::new().cpp(true).get_compiler();
    if !tool.is_like_msvc() {
        panic!(
            "core.dll build requires the MSVC toolchain (saw compiler at {})",
            tool.path().display()
        );
    }

    let libcef = cef_dir.join("lib").join("win").join("libcef.lib");
    let intermediate_dll = obj_dir.join("core.dll");

    // libcef.lib is built against the release CRT, so /MD and /O2 stay
    // pinned regardless of cargo profile — we cannot mix CRTs across
    // libraries. The only thing that flips between dev and release is
    // whether NDEBUG is defined: in dev we leave it undefined so the
    // `#ifndef NDEBUG` blocks (F11 devtools keyboard handler) compile in.
    let debug = env::var("DEBUG").map(|v| v == "true").unwrap_or(false);

    let mut cmd = tool.to_command();
    cmd.current_dir(&obj_dir);
    cmd.args([
        "/nologo",
        "/LD",
        "/std:c++20",
        "/EHsc",
        "/MD",
        "/O2",
        "/permissive",
        "/DOS_WIN=1",
        "/DOS_MAC=0",
        "/DUNICODE",
        "/D_UNICODE",
        "/DWIN32_LEAN_AND_MEAN",
        "/DNOMINMAX",
    ]);
    if !debug {
        cmd.arg("/DNDEBUG");
    }

    cmd.arg(prefixed_path("/I", &cef_dir));
    cmd.arg(prefixed_path("/I", &src_dir));
    cmd.arg(prefixed_path("/Fe", &intermediate_dll));

    for src in SOURCES {
        cmd.arg(src_dir.join(src));
    }

    cmd.arg("/link");
    cmd.arg("/MACHINE:X64");
    cmd.arg(&libcef);
    cmd.arg("user32.lib");
    cmd.arg("shell32.lib");

    println!("cargo:warning=building core.dll → {}", out_dll.display());

    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke cl.exe: {}", e));
    if !status.success() {
        panic!("core.dll compile/link failed (exit {:?})", status.code());
    }

    std::fs::copy(&intermediate_dll, &out_dll)
        .unwrap_or_else(|e| panic!("copying core.dll to resources: {}", e));
}

fn prefixed_path(prefix: &str, path: &Path) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(path.as_os_str());
    s
}

fn needs_rebuild(dll: &Path, src_dir: &Path) -> bool {
    let Ok(dll_meta) = dll.metadata() else {
        return true;
    };
    let Ok(dll_mtime) = dll_meta.modified() else {
        return true;
    };

    for src in SOURCES {
        if newer(&src_dir.join(src), dll_mtime) {
            return true;
        }
    }
    walk_headers(src_dir, dll_mtime)
}

fn newer(path: &Path, baseline: SystemTime) -> bool {
    matches!(
        path.metadata().and_then(|m| m.modified()),
        Ok(t) if t > baseline
    )
}

fn walk_headers(dir: &Path, baseline: SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if walk_headers(&path, baseline) {
                return true;
            }
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("h") | Some("hpp")
        ) && newer(&path, baseline)
        {
            return true;
        }
    }
    false
}
