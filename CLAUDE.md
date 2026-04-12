# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Talon is a Windows-only desktop app for managing custom League of Legends skins. The end goal is to make `.fantome` skin mods appear directly inside the in-client skin carousel. Current state: an import-and-library view (file picker + drag-and-drop) with an LCU gameflow poller running as a background task. The in-client injection piece is not yet built.

Stack: **Tauri 2 + Rust** (backend) and **React 19 + TypeScript + Vite + Tailwind v4 + shadcn/ui** (frontend). Package manager is **pnpm** via corepack. shadcn components are built on `@base-ui/react`, not Radix.

## Build & run

```bash
pnpm install                                       # first time
pnpm tauri dev                                      # dev server — launches window, HMR for frontend, watches Rust
pnpm tauri build                                    # production build → src-tauri/target/release/bundle/
pnpm build                                          # frontend only (tsc + vite build)
cargo check --manifest-path src-tauri/Cargo.toml    # fast Rust-only check, no window
```

No tests yet. Rust changes and any edit to `src-tauri/tauri.conf.json` require a `tauri dev` restart; frontend changes HMR through Vite.

## Architecture

The Rust backend has **two unrelated subsystems** glued together in `src-tauri/src/lib.rs`:

- **LCU poller** (`src-tauri/src/lcu/`) — discovers the League Client and polls gameflow-phase.
- **Skin library** (`src-tauri/src/skins/`) — scans `%APPDATA%\com.talon.app\skins\` for `.fantome` files, parses metadata, persists enabled state, handles import.

They don't share memory or state. The poller is spawned as a background task in `setup()` via `tauri::async_runtime::spawn`; the library is exposed as `#[tauri::command]` handlers.

### LCU poller (`src-tauri/src/lcu/`)

`poller::run` is structured as **two nested loops** with no explicit shutdown — the task dies with the Tauri runtime on app exit:

1. **Outer** — finds `LeagueClient.exe` via `sysinfo`, reads the lockfile (retries up to 20×500ms because the lockfile appears slightly after process start), builds an `LcuClient`.
2. **Inner** — GETs `/lol-gameflow/v1/gameflow-phase` every 500ms. **Any HTTP error `continue 'outer`s to re-discover the client from scratch.** This is intentional: the client can be closed or restarted at any time, and the tool recovers transparently. Don't add per-request retries inside the inner loop — let failures bubble up to the outer reconnect.

**Self-signed cert bypass:** `http::LcuClient::new` sets `reqwest::ClientBuilder::danger_accept_invalid_certs(true)`. The LCU uses a self-signed localhost cert; don't remove this or the TLS handshake fails.

**Auth header:** `discovery::LcuInfo` stores the full pre-built `"Basic <b64>"` string rather than raw credentials, so every request can attach it without rebuilding.

**Phase events:** the poller emits `app.emit("lcu:phase-changed", phase)` when the phase changes. The frontend doesn't subscribe yet, but the channel is live.

### Skin library (`src-tauri/src/skins/`)

On-disk layout (resolved via `resolve_paths()` in `lib.rs`):

```
%APPDATA%\com.talon.app\
├── skins\*.fantome
└── state.json        # JSON array of enabled skin IDs
```

**`state.rs`** — `SkinState` wraps a `HashSet<String>` of enabled IDs. It's serialized as a **JSON array** (e.g. `["id1", "id2"]`), not a map. Loaded and saved per-command; no in-memory cache. Changing the format is a breaking migration.

**`fantome.rs`** — opens a `.fantome` (a zip archive) and reads `META/info.json`. **Fields are PascalCase**: `Name`, `Author`, `Version`, `Description`, `Heroes`. Every field is optional — malformed mods still parse with whatever can be recovered. The champion is derived from `Heroes[0]` when non-empty, otherwise from the `WAD/{Champion}.wad.client` filename convention inside the archive.

**`library.rs`** — `scan()` tries `fantome::read()` for each file and falls back to a filename-derived stub on any failure, so broken mods still show up and can be diagnosed instead of disappearing.

### Two import paths

Both commands use a shared `pick_dest()` helper in `lib.rs` that appends `(1)`, `(2)`, etc. on filename collision:

- **`import_skin(source: String)`** — from the native file picker via `@tauri-apps/plugin-dialog`. Gets a real filesystem path, uses `std::fs::copy`.
- **`import_skin_bytes(filename: String, bytes: Vec<u8>)`** — from drag-and-drop. HTML5 `File` objects don't expose filesystem paths, so the frontend reads the file as a `Uint8Array` and sends bytes over IPC.

### The drag-and-drop trap — READ THIS

**`src-tauri/tauri.conf.json` sets `"dragDropEnabled": false` on the main window. Do not change this.**

The name is counterintuitive. `dragDropEnabled: true` enables Tauri's *internal* drag-drop event system, which on Windows in Tauri 2.x **silently doesn't fire events at all** (see tauri issues #9448 and #14373 — the problem was confirmed during bringup). Setting it to `false` disables Tauri's interception and lets the webview receive normal HTML5 drag-drop events, which is what `App.tsx` actually listens to.

Because HTML5 `File` objects don't expose filesystem paths, the drag-drop path goes through `import_skin_bytes` (bytes over IPC) rather than `import_skin` (path). If someone "fixes" this by re-enabling `dragDropEnabled`, drag-drop import will silently break on Windows and no automated check will catch it — only a manual drag test will.

### Frontend (`src/App.tsx`)

A single file (~180 lines) that owns all the UI. Key behaviors:

- **Library load** — `invoke<SkinLibrary>("list_skins")` on mount, and again on every window `focus` event so that dropping a file into the folder via Explorer and alt-tab-back refreshes the list automatically.
- **Toggle** — optimistic UI update, then `invoke("set_skin_enabled", ...)` to persist. On failure, reload to reset to ground truth.
- **Drag-and-drop** — document-level `dragenter / dragover / dragleave / drop` listeners with a `dragCounter` ref so nested DOM elements don't flicker the fullscreen overlay.
- **Capitalization** — CSS `capitalize` class on `skin.name`, `skin.champion`, and `skin.author` spans individually. Static literals like "by" stay lowercase.

### Tauri plugins and capabilities

Plugins loaded in `lib.rs`:

- **`tauri-plugin-opener`** — used by the `open_skins_folder` command (Rust side) to reveal the skins folder in Explorer. Capability: `opener:default`.
- **`tauri-plugin-dialog`** — used by the frontend for the native file picker. Capability: `dialog:default`.

Capabilities live in `src-tauri/capabilities/default.json`.

## Conventions

- **Command errors:** return `Result<T, String>` from `#[tauri::command]` handlers. Use `anyhow::Result` internally and convert at the command boundary with `.map_err(|e| e.to_string())`.
- **Command params:** keep single-word (`source`, `filename`, `id`, `enabled`) to avoid Tauri's camelCase↔snake_case mapping becoming a gotcha.
- **Module layout:** each backend feature is a directory with `mod.rs` + leaf files; the public API is re-exported from `mod.rs`.
