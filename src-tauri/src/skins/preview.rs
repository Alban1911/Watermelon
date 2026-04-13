use anyhow::{anyhow, Context, Result};
use image::{imageops, RgbaImage};
use ltk_texture::{Dds, Tex, Texture};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zip::ZipArchive;

use crate::wad::{bin, CompressionType, WadReader};

const DDS_MAGIC: [u8; 4] = *b"DDS ";
const TEX_MAGIC: [u8; 4] = *b"TEX\0";
const DDRAGON_TIMEOUT: Duration = Duration::from_secs(5);
const BACKGROUND_WIDTH: u32 = 1920;
const BACKGROUND_HEIGHT: u32 = 1080;

/// Produces (or refreshes) a PNG preview for a `.fantome` file and returns
/// its path.
///
/// Tries three sources in order:
///   1. **Splash texture inside the WAD** — walk every DDS/TEX entry
///      (packed or unpacked layout), parse headers for dimensions, score
///      by aspect-ratio closeness to 16:9, pick the best match and decode
///      via `ltk_texture`. Handles both Riot's native `.tex` format and
///      standard DDS with one unified pipeline. This is the authoritative
///      in-game asset when the mod ships one.
///   2. **`META/image.png`** — some mod creators bundle a pre-rendered
///      preview PNG. Used as a fallback when the mod has no splash-shaped
///      texture in its WAD, because the bundled image is often a square
///      icon rather than the actual splash.
///   3. **Data Dragon fallback** — fetch the base champion loading
///      portrait from ddragon.leagueoflegends.com using the champion name.
#[allow(dead_code)]
pub fn cached_or_extract(
    fantome_path: &Path,
    previews_dir: &Path,
    skin_id: &str,
    champion: Option<&str>,
) -> Result<Option<PathBuf>> {
    let dest = previews_dir.join(format!("{skin_id}.png"));

    if cache_is_fresh(fantome_path, &dest) {
        return Ok(Some(dest));
    }

    std::fs::create_dir_all(previews_dir).context("creating previews dir")?;

    if let Some(texture_bytes) = find_best_splash_texture(fantome_path, champion)? {
        write_texture_as_png(&texture_bytes, &dest)?;
        return Ok(Some(dest));
    }

    if let Ok(Some(bytes)) = read_meta_image(fantome_path) {
        std::fs::write(&dest, &bytes).context("writing META/image.png to cache")?;
        return Ok(Some(dest));
    }

    if let Some(name) = champion {
        if fetch_ddragon_splash(name, &dest).is_ok() {
            return Ok(Some(dest));
        }
    }

    Ok(None)
}

pub fn cached_preview_path(
    fantome_path: &Path,
    previews_dir: &Path,
    skin_id: &str,
) -> Option<PathBuf> {
    let dest = previews_dir.join(format!("{skin_id}.png"));
    if cache_is_fresh(fantome_path, &dest) {
        Some(dest)
    } else {
        None
    }
}

/// Produces (or refreshes) a PNG tile/icon for a `.fantome` file and returns
/// its path.
///
/// Tries three sources in order:
///   1. **HUD icon texture inside the WAD** â€” parse PROP bins for
///      `IconSquare` / `IconCircle` / `IconAvatar`-style references and
///      decode the matching TEX/DDS entry.
///   2. **`META/image.png`** â€” many mods bundle an already UI-friendly image.
///   3. **Data Dragon fallback** â€” fetch the base champion square tile.
#[allow(dead_code)]
pub fn cached_or_extract_tile(
    fantome_path: &Path,
    tile_previews_dir: &Path,
    skin_id: &str,
    champion: Option<&str>,
) -> Result<Option<PathBuf>> {
    let dest = tile_previews_dir.join(format!("{skin_id}.png"));

    if cache_is_fresh(fantome_path, &dest) {
        return Ok(Some(dest));
    }

    std::fs::create_dir_all(tile_previews_dir).context("creating tile previews dir")?;

    if let Some(texture_bytes) = find_best_tile_texture(fantome_path)? {
        write_texture_as_png(&texture_bytes, &dest)?;
        return Ok(Some(dest));
    }

    if let Some(texture_bytes) = find_best_splash_texture(fantome_path, champion)? {
        write_texture_as_png(&texture_bytes, &dest)?;
        return Ok(Some(dest));
    }

    if let Ok(Some(bytes)) = read_meta_image(fantome_path) {
        std::fs::write(&dest, &bytes).context("writing META/image.png to tile cache")?;
        return Ok(Some(dest));
    }

    if let Some(name) = champion {
        if fetch_ddragon_tile(name, &dest).is_ok() {
            return Ok(Some(dest));
        }
    }

    Ok(None)
}

pub fn cached_tile_preview_path(
    fantome_path: &Path,
    tile_previews_dir: &Path,
    skin_id: &str,
) -> Option<PathBuf> {
    let dest = tile_previews_dir.join(format!("{skin_id}.png"));
    if cache_is_fresh(fantome_path, &dest) {
        Some(dest)
    } else {
        None
    }
}

/// Produces (or refreshes) a carousel-background-safe PNG for a `.fantome`
/// file and returns its path.
///
/// The generated image preserves the splash art's native aspect ratio by
/// centering a contained foreground on top of a blurred cover background.
/// This avoids the client stretching a portrait-ish splash across the full
/// carousel backdrop.
#[allow(dead_code)]
pub fn cached_or_extract_background(
    fantome_path: &Path,
    background_previews_dir: &Path,
    skin_id: &str,
    champion: Option<&str>,
) -> Result<Option<PathBuf>> {
    let dest = background_previews_dir.join(format!("{skin_id}.png"));

    if cache_is_fresh(fantome_path, &dest) {
        return Ok(Some(dest));
    }

    std::fs::create_dir_all(background_previews_dir)
        .context("creating background previews dir")?;

    let source_bytes = if let Some(texture_bytes) = find_best_splash_texture(fantome_path, champion)? {
        texture_bytes
    } else if let Ok(Some(bytes)) = read_meta_image(fantome_path) {
        bytes
    } else if let Some(name) = champion {
        let temp = background_previews_dir.join(format!("{skin_id}.source.png"));
        if fetch_ddragon_splash(name, &temp).is_ok() {
            let bytes = std::fs::read(&temp).context("reading fetched background source")?;
            let _ = std::fs::remove_file(&temp);
            bytes
        } else {
            return Ok(None);
        }
    } else {
        return Ok(None);
    };

    compose_background_png(&source_bytes, &dest)?;
    Ok(Some(dest))
}

pub fn cached_background_preview_path(
    fantome_path: &Path,
    background_previews_dir: &Path,
    skin_id: &str,
) -> Option<PathBuf> {
    let dest = background_previews_dir.join(format!("{skin_id}.png"));
    if cache_is_fresh(fantome_path, &dest) {
        Some(dest)
    } else {
        None
    }
}

fn cache_is_fresh(fantome_path: &Path, cached: &Path) -> bool {
    let fantome_mtime = fantome_path
        .metadata()
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let cache_mtime = match cached.metadata().and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    cache_mtime >= fantome_mtime
}

/// Looks for a `META/image.png` entry inside the `.fantome` archive and
/// returns its raw bytes. Some cslol-manager mods bundle a pre-rendered
/// preview image as part of the mod metadata — when present it's the best
/// possible preview since the mod creator chose it specifically.
#[allow(dead_code)]
fn read_meta_image(fantome_path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;
    read_meta_image_from_zip(&mut zip)
}

fn read_meta_image_from_zip(zip: &mut ZipArchive<File>) -> Result<Option<Vec<u8>>> {
    let mut entry = match zip.by_name("META/image.png") {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).context("read META/image.png")?;
    Ok(Some(buf))
}

/// Finds the best splash-art texture (DDS or TEX) inside a `.fantome`,
/// handling both packed (single `.wad.client` binary) and unpacked
/// (`WAD/Champion.wad.client/...` directory prefix) layouts.
///
/// Strategy, in descending order of authority:
///   1. **Parse the mod's PROP bin files** and read the `Loadscreen.Image`
///      string (or any loadscreen-ish path referenced from any bin). Hash
///      that exact path with xxhash64 and look it up in the WAD TOC. This
///      is the only method that tells us *exactly* which skin slot the
///      mod is targeting — works for base skin, skin11, skin22, whatever.
///   2. **Hardcoded path patterns** — xxhash64 a handful of common paths
///      (`assets/characters/{champion}/skins/base/{champion}loadscreen_0.tex`
///      and variants). Cheap fallback when bin parsing comes up empty.
///   3. **Aspect-ratio heuristic** — walk every DDS/TEX entry and pick
///      the one closest to 16:9. Last resort for mods where neither bin
///      parsing nor path guessing yields a match (e.g. unpacked mods).
#[allow(dead_code)]
fn find_best_splash_texture(
    fantome_path: &Path,
    champion: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;

    if let Some(wad_bytes) = read_packed_wad(&mut zip)? {
        let reader = WadReader::new(&wad_bytes).context("parse WAD")?;

        if let Some(bytes) = find_splash_via_bin(&reader) {
            return Ok(Some(bytes));
        }

        if let Some(name) = champion {
            if let Some(bytes) = find_splash_by_known_paths(&reader, name) {
                return Ok(Some(bytes));
            }
        }

        return Ok(pick_best_splash(&collect_textures_from_reader(&reader)));
    }

    Ok(pick_best_splash(&collect_textures_from_unpacked_zip(
        &mut zip,
    )))
}

/// Finds the most UI-appropriate tile/icon texture inside a `.fantome`.
/// Prefers explicit HUD icon references from PROP bins over heuristics.
#[allow(dead_code)]
fn find_best_tile_texture(fantome_path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;

    if let Some(wad_bytes) = read_packed_wad(&mut zip)? {
        let reader = WadReader::new(&wad_bytes).context("parse WAD")?;

        if let Some(bytes) = find_tile_via_bin(&reader) {
            return Ok(Some(bytes));
        }
        return Ok(None);
    }

    if let Some(bytes) = find_tile_in_unpacked_zip(&mut zip) {
        return Ok(Some(bytes));
    }
    Ok(None)
}

/// Walks every PROP bin in the WAD, collects string values from each,
/// and returns the first one that (a) has `loadscreen` in its basename
/// and (b) ends in `.tex`/`.dds`, **and** hashes to an entry actually
/// present in the WAD TOC. Multi-skin mod packs have many PROP bins and
/// many of them reference loadscreen paths the mod doesn't ship, so we
/// keep scanning until we find one that's actually present.
///
/// The basename check is critical: a naive `path.contains("splash")`
/// filter also matches particle VFX filenames like
/// `..._q_explosion_bighoneysplash.skins_...tex`, which would hit the
/// TOC (mods do ship those) but give a completely wrong preview.
fn find_splash_via_bin(reader: &WadReader) -> Option<Vec<u8>> {
    for entry in reader.entries() {
        if !matches!(
            entry.compression,
            CompressionType::Zstd | CompressionType::Raw
        ) {
            continue;
        }
        let Ok(decoded) = reader.extract(entry) else { continue };
        if decoded.len() < 4 || &decoded[..4] != b"PROP" {
            continue;
        }

        for path in bin::collect_strings(&decoded) {
            let lower = path.to_ascii_lowercase();
            let basename = lower.rsplit('/').next().unwrap_or("");
            if !basename.contains("loadscreen") {
                continue;
            }
            if !(lower.ends_with(".tex") || lower.ends_with(".dds")) {
                continue;
            }
            if let Some(bytes) = lookup_texture_by_path(reader, &lower) {
                return Some(bytes);
            }
        }
    }
    None
}

/// Walks every PROP bin in the WAD and looks for HUD icon references.
/// `IconSquare` is the best fit for the champ-select tile, followed by
/// circle/avatar variants as fallbacks.
fn find_tile_via_bin(reader: &WadReader) -> Option<Vec<u8>> {
    let mut square_paths = Vec::new();
    let mut circle_paths = Vec::new();
    let mut avatar_paths = Vec::new();
    let mut hud_paths = Vec::new();

    for entry in reader.entries() {
        if !matches!(
            entry.compression,
            CompressionType::Zstd | CompressionType::Raw
        ) {
            continue;
        }
        let Ok(decoded) = reader.extract(entry) else { continue };
        if decoded.len() < 4 || &decoded[..4] != b"PROP" {
            continue;
        }

        for path in bin::collect_strings(&decoded) {
            let lower = path.to_ascii_lowercase();
            if !is_texture_path(&lower) {
                continue;
            }
            let basename = lower.rsplit('/').next().unwrap_or("");
            if lower.contains("/hud/") && basename.contains("square") {
                square_paths.push(lower);
            } else if lower.contains("/hud/") && basename.contains("circle") {
                circle_paths.push(lower);
            } else if lower.contains("/hud/") && basename.contains("avatar") {
                avatar_paths.push(lower);
            } else if lower.contains("/hud/") {
                hud_paths.push(lower);
            }
        }
    }

    for path in square_paths
        .iter()
        .chain(circle_paths.iter())
        .chain(avatar_paths.iter())
        .chain(hud_paths.iter())
    {
        if let Some(bytes) = lookup_texture_by_path(reader, path) {
            return Some(bytes);
        }
    }

    None
}

/// Tries a small set of known LoL asset path patterns for the base-skin
/// loadscreen, returning the decoded bytes of the first one that's present
/// in the WAD.
fn find_splash_by_known_paths(reader: &WadReader, champion: &str) -> Option<Vec<u8>> {
    let champ = champion
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if champ.is_empty() {
        return None;
    }
    let candidates = [
        format!("assets/characters/{champ}/skins/base/{champ}loadscreen_0.tex"),
        format!("assets/characters/{champ}/skins/base/{champ}loadscreen_0.dds"),
        format!("assets/characters/{champ}/skins/skin0/{champ}loadscreen_0.tex"),
        format!("assets/characters/{champ}/skins/skin0/{champ}loadscreen_0.dds"),
        format!("assets/characters/{champ}/skins/skin0/{champ}_skin0_loadscreen.tex"),
        format!("assets/characters/{champ}/skins/skin0/{champ}_skin0_loadscreen.dds"),
    ];
    for path in &candidates {
        if let Some(bytes) = lookup_texture_by_path(reader, path) {
            return Some(bytes);
        }
    }
    None
}

fn find_tile_in_unpacked_zip(zip: &mut ZipArchive<File>) -> Option<Vec<u8>> {
    let names: Vec<String> = (0..zip.len())
        .map(|i| {
            zip.by_index(i)
                .map(|e| e.name().to_string())
                .unwrap_or_default()
        })
        .collect();

    let mut ranked = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let lower = name.to_ascii_lowercase();
        let is_wad_content = lower
            .strip_prefix("wad/")
            .and_then(|s| s.split_once(".wad.client/"))
            .is_some();
        if !is_wad_content || !is_texture_path(&lower) || !lower.contains("/hud/") {
            continue;
        }

        let basename = lower.rsplit('/').next().unwrap_or("");
        let rank = if basename.contains("square") {
            0
        } else if basename.contains("circle") {
            1
        } else if basename.contains("avatar") {
            2
        } else {
            3
        };
        ranked.push((rank, i));
    }

    ranked.sort_by_key(|(rank, _)| *rank);
    for (_, i) in ranked {
        let Ok(mut entry) = zip.by_index(i) else { continue };
        let mut buf = Vec::new();
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }
        if is_texture_magic(&buf) {
            return Some(buf);
        }
    }

    None
}

/// Hashes `path` (assumed already lowercase) and fetches the corresponding
/// WAD entry if present, returning the decoded bytes when it parses as
/// either a DDS or a TEX texture.
fn lookup_texture_by_path(reader: &WadReader, path: &str) -> Option<Vec<u8>> {
    let hash = xxhash_rust::xxh64::xxh64(path.as_bytes(), 0);
    let entry = reader.find_by_hash(hash)?;
    let decoded = reader.extract(entry).ok()?;
    if is_texture_magic(&decoded) {
        Some(decoded)
    } else {
        None
    }
}

/// If the archive contains a single packed `WAD/X.wad.client` binary
/// (no further path components), returns its bytes.
fn read_packed_wad(zip: &mut ZipArchive<File>) -> Result<Option<Vec<u8>>> {
    let mut packed_idx: Option<usize> = None;
    for i in 0..zip.len() {
        let is_packed = match zip.by_index(i) {
            Ok(entry) => entry
                .name()
                .strip_prefix("WAD/")
                .map(|s| s.ends_with(".wad.client") && !s.contains('/'))
                .unwrap_or(false),
            Err(_) => false,
        };
        if is_packed {
            packed_idx = Some(i);
            break;
        }
    }

    let Some(i) = packed_idx else { return Ok(None) };
    let mut entry = zip.by_index(i).context("re-open packed WAD")?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).context("read packed WAD")?;
    Ok(Some(buf))
}

fn collect_textures_from_reader(reader: &WadReader) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for entry in reader.entries() {
        if !matches!(
            entry.compression,
            CompressionType::Zstd | CompressionType::Raw
        ) {
            continue;
        }
        let Ok(decoded) = reader.extract(entry) else { continue };
        if is_texture_magic(&decoded) {
            out.push(decoded);
        }
    }
    out
}

/// Walks an unpacked `.fantome` layout where WAD contents are stored as
/// individual zip entries under `WAD/{Champion}.wad.client/...`.
fn collect_textures_from_unpacked_zip(zip: &mut ZipArchive<File>) -> Vec<Vec<u8>> {
    let names: Vec<String> = (0..zip.len())
        .map(|i| {
            zip.by_index(i)
                .map(|e| e.name().to_string())
                .unwrap_or_default()
        })
        .collect();

    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let is_wad_content = name
            .strip_prefix("WAD/")
            .and_then(|s| s.split_once(".wad.client/"))
            .is_some();
        if !is_wad_content {
            continue;
        }
        let Ok(mut entry) = zip.by_index(i) else { continue };
        let mut buf = Vec::new();
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }
        if is_texture_magic(&buf) {
            out.push(buf);
        }
    }
    out
}

fn is_texture_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && (bytes[..4] == DDS_MAGIC || bytes[..4] == TEX_MAGIC)
}

fn is_texture_path(path: &str) -> bool {
    path.ends_with(".tex") || path.ends_with(".dds")
}

struct ScoredCandidate<'a> {
    bytes: &'a [u8],
    deviation: f64,
    pixels: u64,
}

/// Scores candidates by aspect-ratio closeness to 16:9 and hard-rejects
/// anything above 0.15 deviation — keeps us from picking 2:1 banner
/// textures or 1:1 model diffuses when the mod ships no real splash.
/// Uses the texture header (cheap) for dimensions, not the decoded pixels.
fn pick_best_splash(candidates: &[Vec<u8>]) -> Option<Vec<u8>> {
    const SPLASH_RATIO: f64 = 16.0 / 9.0;
    const MAX_DEVIATION: f64 = 0.15;
    const MIN_PIXELS: u64 = 50_000;

    let mut scored: Vec<ScoredCandidate> = Vec::new();
    for bytes in candidates {
        let Some(texture) = parse_texture(bytes) else { continue };
        let width = texture.width();
        let height = texture.height();
        if width == 0 || height == 0 {
            continue;
        }
        let max = width.max(height) as f64;
        let min = width.min(height) as f64;
        let deviation = (max / min - SPLASH_RATIO).abs();
        let pixels = (width as u64) * (height as u64);
        scored.push(ScoredCandidate { bytes, deviation, pixels });
    }

    scored.sort_by(|a, b| {
        a.deviation
            .partial_cmp(&b.deviation)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.pixels.cmp(&a.pixels))
    });

    scored
        .into_iter()
        .find(|c| c.deviation <= MAX_DEVIATION && c.pixels >= MIN_PIXELS)
        .map(|c| c.bytes.to_vec())
}

/// Parses a byte slice as either a DDS or a TEX texture by sniffing the
/// first 4 magic bytes. Returns `None` for anything else or if the header
/// is malformed.
fn parse_texture(bytes: &[u8]) -> Option<Texture> {
    let magic: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let mut cursor = Cursor::new(bytes);
    match magic {
        DDS_MAGIC => Dds::from_reader(&mut cursor).ok().map(Texture::from),
        TEX_MAGIC => Tex::from_reader(&mut cursor).ok().map(Texture::from),
        _ => None,
    }
}

fn compose_background_png(source_bytes: &[u8], dest: &Path) -> Result<()> {
    let src = decode_image_to_rgba(source_bytes)?;
    let (src_w, src_h) = src.dimensions();
    if src_w == 0 || src_h == 0 {
        return Err(anyhow!("background source has zero dimension"));
    }

    let cover_scale = f32::max(
        BACKGROUND_WIDTH as f32 / src_w as f32,
        BACKGROUND_HEIGHT as f32 / src_h as f32,
    );
    let cover_w = ((src_w as f32) * cover_scale).ceil() as u32;
    let cover_h = ((src_h as f32) * cover_scale).ceil() as u32;
    let cover = imageops::resize(&src, cover_w, cover_h, imageops::FilterType::Lanczos3);
    let crop_x = (cover_w.saturating_sub(BACKGROUND_WIDTH)) / 2;
    let crop_y = (cover_h.saturating_sub(BACKGROUND_HEIGHT)) / 2;
    let mut canvas = imageops::crop_imm(&cover, crop_x, crop_y, BACKGROUND_WIDTH, BACKGROUND_HEIGHT)
        .to_image();
    canvas = imageops::blur(&canvas, 18.0);

    // Dim the blurred fill a bit so the centered art remains the focal point.
    for pixel in canvas.pixels_mut() {
        pixel[0] = ((pixel[0] as f32) * 0.55) as u8;
        pixel[1] = ((pixel[1] as f32) * 0.55) as u8;
        pixel[2] = ((pixel[2] as f32) * 0.55) as u8;
    }

    let contain_scale = f32::min(
        (BACKGROUND_WIDTH as f32 * 0.88) / src_w as f32,
        (BACKGROUND_HEIGHT as f32 * 0.9) / src_h as f32,
    );
    let fg_w = ((src_w as f32) * contain_scale).round().max(1.0) as u32;
    let fg_h = ((src_h as f32) * contain_scale).round().max(1.0) as u32;
    let foreground = imageops::resize(&src, fg_w, fg_h, imageops::FilterType::Lanczos3);

    // Lift the art a bit so faces land closer to the carousel focus ring.
    let offset_x = ((BACKGROUND_WIDTH - fg_w) / 2) as i64;
    let offset_y = (((BACKGROUND_HEIGHT - fg_h) as f32) * 0.42).round() as i64;
    imageops::overlay(&mut canvas, &foreground, offset_x, offset_y);

    // Subtle vignette so the blurred fill doesn't compete with the foreground.
    apply_vignette(&mut canvas);
    canvas.save(dest).context("writing background PNG")?;
    Ok(())
}

fn decode_image_to_rgba(bytes: &[u8]) -> Result<RgbaImage> {
    if let Some(texture) = parse_texture(bytes) {
        let surface = texture
            .decode_mipmap(0)
            .map_err(|e| anyhow!("decode mipmap: {e}"))?;
        return surface
            .into_rgba_image()
            .map_err(|e| anyhow!("to rgba: {e}"));
    }

    let img = image::load_from_memory(bytes).context("decoding image source")?;
    Ok(img.into_rgba8())
}

fn apply_vignette(canvas: &mut RgbaImage) {
    let cx = BACKGROUND_WIDTH as f32 / 2.0;
    let cy = BACKGROUND_HEIGHT as f32 / 2.0;
    let max_dist = (cx * cx + cy * cy).sqrt();

    for (x, y, pixel) in canvas.enumerate_pixels_mut() {
        let dx = x as f32 - cx;
        let dy = y as f32 - cy;
        let dist = (dx * dx + dy * dy).sqrt() / max_dist;
        let shade = 1.0 - (dist * dist * 0.28);
        pixel[0] = ((pixel[0] as f32) * shade) as u8;
        pixel[1] = ((pixel[1] as f32) * shade) as u8;
        pixel[2] = ((pixel[2] as f32) * shade) as u8;
    }
}

/// Decodes a DDS or TEX byte slice and writes the result as a PNG at `dest`.
fn write_texture_as_png(bytes: &[u8], dest: &Path) -> Result<()> {
    let texture = parse_texture(bytes).ok_or_else(|| anyhow!("not a DDS or TEX file"))?;
    let surface = texture
        .decode_mipmap(0)
        .map_err(|e| anyhow!("decode mipmap: {e}"))?;
    let img = surface
        .into_rgba_image()
        .map_err(|e| anyhow!("to rgba: {e}"))?;
    img.save(dest).context("save PNG")?;
    Ok(())
}

/// Returns the path to a cached square champion icon (380×380 face tile),
/// fetching it from Data Dragon's `/tiles/` endpoint and re-encoding as PNG
/// on first access. Returns `None` if the champion name is empty or the
/// fetch fails. Used for the champion grid view — one tile per champion
/// showing the official portrait.
pub fn cached_champion_icon(icons_dir: &Path, champion: &str) -> Option<PathBuf> {
    let sanitized = sanitize_champion_name(champion)?;
    let dest = icons_dir.join(format!("{sanitized}.png"));
    if dest.exists() {
        return Some(dest);
    }
    std::fs::create_dir_all(icons_dir).ok()?;
    let url = format!(
        "https://ddragon.leagueoflegends.com/cdn/img/champion/tiles/{sanitized}_0.jpg"
    );
    fetch_and_save_as_png(&url, &dest).ok()?;
    Some(dest)
}

pub fn cached_champion_icon_path(icons_dir: &Path, champion: &str) -> Option<PathBuf> {
    let sanitized = sanitize_champion_name(champion)?;
    let dest = icons_dir.join(format!("{sanitized}.png"));
    if dest.exists() {
        Some(dest)
    } else {
        None
    }
}

pub fn warm_all_cached_assets(
    fantome_path: &Path,
    previews_dir: &Path,
    background_previews_dir: &Path,
    tile_previews_dir: &Path,
    icons_dir: &Path,
    skin_id: &str,
    champion: Option<&str>,
) -> Result<bool> {
    let preview_dest = previews_dir.join(format!("{skin_id}.png"));
    let background_dest = background_previews_dir.join(format!("{skin_id}.png"));
    let tile_dest = tile_previews_dir.join(format!("{skin_id}.png"));

    let needs_preview = !cache_is_fresh(fantome_path, &preview_dest);
    let needs_background = !cache_is_fresh(fantome_path, &background_dest);
    let needs_tile = !cache_is_fresh(fantome_path, &tile_dest);
    let needs_icon = champion
        .map(|name| cached_champion_icon_path(icons_dir, name).is_none())
        .unwrap_or(false);

    if !needs_preview && !needs_background && !needs_tile && !needs_icon {
        return Ok(false);
    }

    std::fs::create_dir_all(previews_dir).context("creating previews dir")?;
    std::fs::create_dir_all(background_previews_dir)
        .context("creating background previews dir")?;
    std::fs::create_dir_all(tile_previews_dir).context("creating tile previews dir")?;
    std::fs::create_dir_all(icons_dir).context("creating champion icons dir")?;

    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;
    let meta_image = read_meta_image_from_zip(&mut zip)?;

    let (splash_bytes, tile_bytes) = if let Some(wad_bytes) = read_packed_wad(&mut zip)? {
        let reader = WadReader::new(&wad_bytes).context("parse WAD")?;
        let splash = find_splash_via_bin(&reader)
            .or_else(|| champion.and_then(|name| find_splash_by_known_paths(&reader, name)))
            .or_else(|| pick_best_splash(&collect_textures_from_reader(&reader)));
        let tile = find_tile_via_bin(&reader);
        (splash, tile)
    } else {
        let tile = find_tile_in_unpacked_zip(&mut zip);
        let splash = pick_best_splash(&collect_textures_from_unpacked_zip(&mut zip));
        (splash, tile)
    };

    let mut changed = false;

    if needs_preview {
        if let Some(bytes) = splash_bytes.as_deref() {
            write_texture_as_png(bytes, &preview_dest)?;
            changed = true;
        } else if let Some(bytes) = meta_image.as_deref() {
            std::fs::write(&preview_dest, bytes).context("writing preview PNG")?;
            changed = true;
        } else if let Some(name) = champion {
            if fetch_ddragon_splash(name, &preview_dest).is_ok() {
                changed = true;
            }
        }
    }

    if needs_background {
        if let Some(bytes) = splash_bytes.as_deref() {
            compose_background_png(bytes, &background_dest)?;
            changed = true;
        } else if let Some(bytes) = meta_image.as_deref() {
            compose_background_png(bytes, &background_dest)?;
            changed = true;
        } else if let Some(name) = champion {
            if let Some(sanitized) = sanitize_champion_name(name) {
                let url = format!(
                    "https://ddragon.leagueoflegends.com/cdn/img/champion/loading/{sanitized}_0.jpg"
                );
                if let Ok(bytes) = fetch_image_bytes(&url) {
                    compose_background_png(&bytes, &background_dest)?;
                    changed = true;
                }
            }
        }
    }

    if needs_tile {
        if let Some(bytes) = tile_bytes.as_deref() {
            write_texture_as_png(bytes, &tile_dest)?;
            changed = true;
        } else if let Some(bytes) = splash_bytes.as_deref() {
            write_texture_as_png(bytes, &tile_dest)?;
            changed = true;
        } else if let Some(bytes) = meta_image.as_deref() {
            std::fs::write(&tile_dest, bytes).context("writing tile PNG")?;
            changed = true;
        } else if let Some(name) = champion {
            if fetch_ddragon_tile(name, &tile_dest).is_ok() {
                changed = true;
            }
        }
    }

    if needs_icon {
        if let Some(name) = champion {
            if cached_champion_icon(icons_dir, name).is_some() {
                changed = true;
            }
        }
    }

    Ok(changed)
}

/// Downloads the base champion loading portrait from Data Dragon, decodes
/// the JPEG, and writes it as a PNG at `dest`. Uses the `/loading/` endpoint
/// (308×560 portrait) rather than `/splash/` (1215×717 landscape) so the
/// aspect ratio matches the textures extracted from mods.
fn fetch_ddragon_splash(champion: &str, dest: &Path) -> Result<()> {
    let sanitized = sanitize_champion_name(champion)
        .ok_or_else(|| anyhow!("empty champion name"))?;
    let url = format!(
        "https://ddragon.leagueoflegends.com/cdn/img/champion/loading/{sanitized}_0.jpg"
    );
    fetch_and_save_as_png(&url, dest)
}

fn fetch_ddragon_tile(champion: &str, dest: &Path) -> Result<()> {
    let sanitized = sanitize_champion_name(champion)
        .ok_or_else(|| anyhow!("empty champion name"))?;
    let url = format!(
        "https://ddragon.leagueoflegends.com/cdn/img/champion/tiles/{sanitized}_0.jpg"
    );
    fetch_and_save_as_png(&url, dest)
}

/// Strips everything non-alphanumeric from a champion name so "Miss Fortune"
/// becomes "MissFortune" etc. — matches Data Dragon's internal naming for
/// most champions. Returns `None` if the result is empty.
fn sanitize_champion_name(champion: &str) -> Option<String> {
    let s: String = champion
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn fetch_and_save_as_png(url: &str, dest: &Path) -> Result<()> {
    let url = url.to_string();
    let dest = dest.to_path_buf();
    let bytes = fetch_image_bytes(&url)?;
    let img = image::load_from_memory(&bytes).context("decoding image")?;
    img.save(&dest).context("writing PNG")?;
    Ok(())
}

fn fetch_image_bytes(url: &str) -> Result<Vec<u8>> {
    let url = url.to_string();
    std::thread::spawn(move || -> Result<Vec<u8>> {
        let client = reqwest::blocking::Client::builder()
            .timeout(DDRAGON_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        let resp = client.get(&url).send().context("fetching Data Dragon")?;
        if !resp.status().is_success() {
            return Err(anyhow!("Data Dragon returned status {}", resp.status()));
        }
        Ok(resp.bytes().context("reading Data Dragon response")?.to_vec())
    })
    .join()
    .map_err(|_| anyhow!("image download thread panicked"))?
}
