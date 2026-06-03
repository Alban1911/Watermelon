use anyhow::{anyhow, Context, Result};
use image::codecs::png::{CompressionType as PngCompression, FilterType as PngFilter, PngEncoder};
use image::{imageops, ExtendedColorType, ImageEncoder, RgbaImage};
use ltk_texture::{Dds, Tex, Texture};
use std::fs::File;
use std::io::{BufWriter, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zip::ZipArchive;

use crate::wad::{bin, CompressionType, WadReader};

const DDS_MAGIC: [u8; 4] = *b"DDS ";
const TEX_MAGIC: [u8; 4] = *b"TEX\0";
const DDRAGON_TIMEOUT: Duration = Duration::from_secs(5);
const BACKGROUND_WIDTH: u32 = 1280;
const BACKGROUND_HEIGHT: u32 = 720;
// Max edge length for carousel tiles — anything bigger gets Lanczos-scaled
// down before encoding. Splash textures are typically 2048² which would
// bloat the tile cache for zero visible gain.
const TILE_MAX_DIM: u32 = 384;

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

/// Reads an optional `META/image.png` entry from the `.fantome` archive.
/// Some cslol-manager mods bundle a pre-rendered preview that's often the
/// best possible fallback since the mod author chose it specifically.
fn read_meta_image(zip: &mut ZipArchive<File>) -> Result<Option<Vec<u8>>> {
    let mut entry = match zip.by_name("META/image.png") {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).context("read META/image.png")?;
    Ok(Some(buf))
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
        let Ok(decoded) = reader.extract(entry) else {
            continue;
        };
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
        let Ok(decoded) = reader.extract(entry) else {
            continue;
        };
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
        let Ok(mut entry) = zip.by_index(i) else {
            continue;
        };
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
        let Ok(decoded) = reader.extract(entry) else {
            continue;
        };
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
        let Ok(mut entry) = zip.by_index(i) else {
            continue;
        };
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
        let Some(texture) = parse_texture(bytes) else {
            continue;
        };
        let width = texture.width();
        let height = texture.height();
        if width == 0 || height == 0 {
            continue;
        }
        let max = width.max(height) as f64;
        let min = width.min(height) as f64;
        let deviation = (max / min - SPLASH_RATIO).abs();
        let pixels = (width as u64) * (height as u64);
        scored.push(ScoredCandidate {
            bytes,
            deviation,
            pixels,
        });
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

// Portrait background generation settings.
// Custom skins usually only ship vertical loading/splash art, so for backgrounds
// we create a blurred 16:9 atmosphere layer and blend the sharp portrait on top.
const PORTRAIT_BG_BLUR: f32 = 20.0;
const PORTRAIT_BG_DARKEN: f32 = 0.82;
const PORTRAIT_FG_OPACITY: f32 = 0.86;
const PORTRAIT_FEATHER_X: u32 = 180;
const PORTRAIT_FEATHER_Y: u32 = 60;

// Final vignette strength over the generated 1280×720 background.
const FINAL_VIGNETTE_STRENGTH: f32 = 0.38;

// For true landscape images, keep the normal cover-crop behavior.
const LANDSCAPE_BG_DARKEN: f32 = 0.84;

/// Generates a 1280×720 champ-select background from either vertical or
/// landscape source art.
///
/// Portrait source:
/// - stretched/blurred/darkened full-screen background
/// - sharp centered portrait blended with 2D feathering
///
/// Landscape source:
/// - cover-cropped to 1280×720
/// - lightly darkened
fn compose_background_png(src: &RgbaImage, dest: &Path) -> Result<()> {
    let (src_w, src_h) = src.dimensions();
    if src_w == 0 || src_h == 0 {
        return Err(anyhow!("background source has zero dimension"));
    }

    let mut canvas = if src_h > src_w {
        compose_background_from_portrait(src)
    } else {
        compose_background_from_landscape(src)
    };

    apply_vignette(&mut canvas, FINAL_VIGNETTE_STRENGTH);
    save_rgba_as_png(&canvas, dest)
}

fn compose_background_from_portrait(src: &RgbaImage) -> RgbaImage {
    // Background layer:
    // Stretch the whole portrait to 16:9, then blur it. The stretch is hidden
    // by the blur and keeps the source colors/composition across the full canvas.
    let mut bg = imageops::resize(
        src,
        BACKGROUND_WIDTH,
        BACKGROUND_HEIGHT,
        imageops::FilterType::Lanczos3,
    );

    bg = imageops::blur(&bg, PORTRAIT_BG_BLUR);
    darken_image(&mut bg, PORTRAIT_BG_DARKEN);

    // Foreground layer:
    // Keep the full portrait visible, scaled to canvas height.
    let (src_w, src_h) = src.dimensions();
    let fg_h = BACKGROUND_HEIGHT;
    let scale = fg_h as f32 / src_h as f32;
    let fg_w = ((src_w as f32) * scale).round().max(1.0) as u32;

    let fg = imageops::resize(src, fg_w, fg_h, imageops::FilterType::Lanczos3);

    let x = ((BACKGROUND_WIDTH as i64) - (fg_w as i64)) / 2;
    let y = 0;

    overlay_with_2d_feather(
        &mut bg,
        &fg,
        x,
        y,
        PORTRAIT_FEATHER_X,
        PORTRAIT_FEATHER_Y,
        PORTRAIT_FG_OPACITY,
    );

    bg
}

fn compose_background_from_landscape(src: &RgbaImage) -> RgbaImage {
    let (src_w, src_h) = src.dimensions();

    // Cover resize: fill the whole 1280×720 area.
    let cover_scale = f32::max(
        BACKGROUND_WIDTH as f32 / src_w as f32,
        BACKGROUND_HEIGHT as f32 / src_h as f32,
    );

    let cover_w = ((src_w as f32) * cover_scale).round().max(1.0) as u32;
    let cover_h = ((src_h as f32) * cover_scale).round().max(1.0) as u32;

    let covered = imageops::resize(src, cover_w, cover_h, imageops::FilterType::Lanczos3);

    let crop_x = cover_w.saturating_sub(BACKGROUND_WIDTH) / 2;
    let crop_y = cover_h.saturating_sub(BACKGROUND_HEIGHT) / 2;

    let mut canvas = imageops::crop_imm(
        &covered,
        crop_x,
        crop_y,
        BACKGROUND_WIDTH,
        BACKGROUND_HEIGHT,
    )
    .to_image();

    darken_image(&mut canvas, LANDSCAPE_BG_DARKEN);
    canvas
}

fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn edge_fade_factor(dist_to_edge: u32, feather: u32) -> f32 {
    if feather == 0 {
        return 1.0;
    }

    if dist_to_edge >= feather {
        1.0
    } else {
        let t = dist_to_edge as f32 / feather as f32;
        smoothstep01(t)
    }
}

fn overlay_with_2d_feather(
    bg: &mut RgbaImage,
    fg: &RgbaImage,
    x0: i64,
    y0: i64,
    feather_x: u32,
    feather_y: u32,
    opacity: f32,
) {
    let bg_w = bg.width() as i64;
    let bg_h = bg.height() as i64;

    let fg_w = fg.width();
    let fg_h = fg.height();

    for fy in 0..fg_h {
        for fx in 0..fg_w {
            let bx = x0 + fx as i64;
            let by = y0 + fy as i64;

            if bx < 0 || by < 0 || bx >= bg_w || by >= bg_h {
                continue;
            }

            let src_px = fg.get_pixel(fx, fy);
            let dst_px = bg.get_pixel_mut(bx as u32, by as u32);

            let left_dist = fx;
            let right_dist = fg_w.saturating_sub(1).saturating_sub(fx);
            let top_dist = fy;
            let bottom_dist = fg_h.saturating_sub(1).saturating_sub(fy);

            let nearest_x_edge = left_dist.min(right_dist);
            let nearest_y_edge = top_dist.min(bottom_dist);

            let fade_x = edge_fade_factor(nearest_x_edge, feather_x);
            let fade_y = edge_fade_factor(nearest_y_edge, feather_y);

            // Multiplying both fades softens corners too.
            let feather_alpha = fade_x * fade_y;

            let src_alpha = (src_px[3] as f32 / 255.0) * feather_alpha * opacity.clamp(0.0, 1.0);

            let inv_alpha = 1.0 - src_alpha;

            dst_px[0] = ((src_px[0] as f32 * src_alpha) + (dst_px[0] as f32 * inv_alpha))
                .clamp(0.0, 255.0) as u8;

            dst_px[1] = ((src_px[1] as f32 * src_alpha) + (dst_px[1] as f32 * inv_alpha))
                .clamp(0.0, 255.0) as u8;

            dst_px[2] = ((src_px[2] as f32 * src_alpha) + (dst_px[2] as f32 * inv_alpha))
                .clamp(0.0, 255.0) as u8;

            dst_px[3] = 255;
        }
    }
}

fn darken_image(img: &mut RgbaImage, factor: f32) {
    let factor = factor.clamp(0.0, 1.0);

    for px in img.pixels_mut() {
        px[0] = ((px[0] as f32) * factor).clamp(0.0, 255.0) as u8;
        px[1] = ((px[1] as f32) * factor).clamp(0.0, 255.0) as u8;
        px[2] = ((px[2] as f32) * factor).clamp(0.0, 255.0) as u8;
    }
}

fn apply_vignette(img: &mut RgbaImage, strength: f32) {
    let strength = strength.clamp(0.0, 1.0);

    let (w, h) = img.dimensions();
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let max_dist = (cx * cx + cy * cy).sqrt();

    for (x, y, px) in img.enumerate_pixels_mut() {
        let dx = x as f32 - cx;
        let dy = y as f32 - cy;
        let dist = (dx * dx + dy * dy).sqrt();
        let t = (dist / max_dist).min(1.0);

        let darken = 1.0 - strength * t;

        px[0] = ((px[0] as f32) * darken).clamp(0.0, 255.0) as u8;
        px[1] = ((px[1] as f32) * darken).clamp(0.0, 255.0) as u8;
        px[2] = ((px[2] as f32) * darken).clamp(0.0, 255.0) as u8;
        // alpha untouched
    }
}

/// Writes an `RgbaImage` as PNG using the fast-compression encoder path.
/// The `image` crate's default `.save()` runs zlib at its slowest level,
/// which doubles encode time for no benefit in a cache folder.
fn save_rgba_as_png(rgba: &RgbaImage, dest: &Path) -> Result<()> {
    let file = File::create(dest).context("creating PNG file")?;
    let writer = BufWriter::new(file);
    let encoder = PngEncoder::new_with_quality(writer, PngCompression::Fast, PngFilter::NoFilter);
    encoder
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .context("encoding PNG")?;
    Ok(())
}

/// Writes a tile PNG, downscaling first if either edge is larger than
/// `TILE_MAX_DIM`. Preserves aspect ratio.
fn save_rgba_as_tile_png(src: &RgbaImage, dest: &Path) -> Result<()> {
    let (w, h) = src.dimensions();
    if w <= TILE_MAX_DIM && h <= TILE_MAX_DIM {
        return save_rgba_as_png(src, dest);
    }
    let scale = f32::min(
        TILE_MAX_DIM as f32 / w as f32,
        TILE_MAX_DIM as f32 / h as f32,
    );
    let tw = ((w as f32) * scale).round().max(1.0) as u32;
    let th = ((h as f32) * scale).round().max(1.0) as u32;
    let resized = imageops::resize(src, tw, th, imageops::FilterType::Lanczos3);
    save_rgba_as_png(&resized, dest)
}

/// Decodes a user-provided image file's bytes, caps its dimensions at the
/// standard tile size, and writes it to `dest` as PNG. Used by the
/// `set_custom_tile` command to store carousel tile overrides.
pub fn save_custom_tile(source_bytes: &[u8], dest: &Path) -> Result<()> {
    let rgba = decode_image_to_rgba(source_bytes)?;
    save_rgba_as_tile_png(&rgba, dest)
}

/// Decodes a user-provided image file's bytes and writes it as the custom
/// background PNG using the same portrait-aware 1280×720 composition path
/// as auto-generated backgrounds.
pub fn save_custom_background(source_bytes: &[u8], dest: &Path) -> Result<()> {
    let src = decode_image_to_rgba(source_bytes)?;
    compose_background_png(&src, dest)
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
    fetch_and_save_as_png(&ddragon_tile_url(&sanitized), &dest).ok()?;
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
    std::fs::create_dir_all(background_previews_dir).context("creating background previews dir")?;
    std::fs::create_dir_all(tile_previews_dir).context("creating tile previews dir")?;
    std::fs::create_dir_all(icons_dir).context("creating champion icons dir")?;

    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;
    let meta_image = read_meta_image(&mut zip)?;

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

    // Decode each texture at most once. The splash RGBA feeds preview,
    // background, and (if no dedicated HUD icon exists) tile fallback, so
    // without caching we'd run the BC decode path three times on the same
    // bytes. The dedicated tile texture is decoded only if it's present
    // AND we actually need to write a tile.
    let need_splash_rgba =
        needs_preview || needs_background || (needs_tile && tile_bytes.is_none());
    let splash_rgba: Option<RgbaImage> = if need_splash_rgba {
        splash_bytes
            .as_deref()
            .and_then(|b| decode_image_to_rgba(b).ok())
    } else {
        None
    };
    let tile_rgba: Option<RgbaImage> = if needs_tile {
        tile_bytes
            .as_deref()
            .and_then(|b| decode_image_to_rgba(b).ok())
    } else {
        None
    };

    let mut changed = false;

    if needs_preview {
        if let Some(rgba) = splash_rgba.as_ref() {
            save_rgba_as_png(rgba, &preview_dest)?;
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
        if let Some(rgba) = splash_rgba.as_ref() {
            compose_background_png(rgba, &background_dest)?;
            changed = true;
        } else if let Some(bytes) = meta_image.as_deref() {
            if let Ok(rgba) = decode_image_to_rgba(bytes) {
                compose_background_png(&rgba, &background_dest)?;
                changed = true;
            }
        } else if let Some(name) = champion {
            if let Some(sanitized) = sanitize_champion_name(name) {
                if let Ok(bytes) = fetch_image_bytes(&ddragon_loading_url(&sanitized)) {
                    if let Ok(rgba) = decode_image_to_rgba(&bytes) {
                        compose_background_png(&rgba, &background_dest)?;
                        changed = true;
                    }
                }
            }
        }
    }

    if needs_tile {
        if let Some(rgba) = tile_rgba.as_ref() {
            save_rgba_as_tile_png(rgba, &tile_dest)?;
            changed = true;
        } else if let Some(rgba) = splash_rgba.as_ref() {
            save_rgba_as_tile_png(rgba, &tile_dest)?;
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
    let sanitized =
        sanitize_champion_name(champion).ok_or_else(|| anyhow!("empty champion name"))?;
    fetch_and_save_as_png(&ddragon_loading_url(&sanitized), dest)
}

fn fetch_ddragon_tile(champion: &str, dest: &Path) -> Result<()> {
    let sanitized =
        sanitize_champion_name(champion).ok_or_else(|| anyhow!("empty champion name"))?;
    fetch_and_save_as_png(&ddragon_tile_url(&sanitized), dest)
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

fn ddragon_loading_url(sanitized: &str) -> String {
    format!("https://ddragon.leagueoflegends.com/cdn/img/champion/loading/{sanitized}_0.jpg")
}

fn ddragon_tile_url(sanitized: &str) -> String {
    format!("https://ddragon.leagueoflegends.com/cdn/img/champion/tiles/{sanitized}_0.jpg")
}

fn fetch_and_save_as_png(url: &str, dest: &Path) -> Result<()> {
    let bytes = fetch_image_bytes(url)?;
    let img = image::load_from_memory(&bytes).context("decoding image")?;
    save_rgba_as_png(&img.into_rgba8(), dest)
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
        Ok(resp
            .bytes()
            .context("reading Data Dragon response")?
            .to_vec())
    })
    .join()
    .map_err(|_| anyhow!("image download thread panicked"))?
}
