use anyhow::{anyhow, Context, Result};
use ltk_texture::{Dds, Tex, Texture};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zip::ZipArchive;

use crate::wad::{CompressionType, WadReader};

const DDS_MAGIC: [u8; 4] = *b"DDS ";
const TEX_MAGIC: [u8; 4] = *b"TEX\0";
const DDRAGON_TIMEOUT: Duration = Duration::from_secs(5);

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

    if let Some(texture_bytes) = find_best_splash_texture(fantome_path)? {
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
fn read_meta_image(fantome_path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;
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
fn find_best_splash_texture(fantome_path: &Path) -> Result<Option<Vec<u8>>> {
    let candidates = collect_texture_candidates(fantome_path)?;
    Ok(pick_best_splash(&candidates))
}

fn collect_texture_candidates(fantome_path: &Path) -> Result<Vec<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;

    if let Some(wad_bytes) = read_packed_wad(&mut zip)? {
        return Ok(collect_textures_from_packed_wad(&wad_bytes));
    }
    Ok(collect_textures_from_unpacked_zip(&mut zip))
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

fn collect_textures_from_packed_wad(wad_bytes: &[u8]) -> Vec<Vec<u8>> {
    let reader = match WadReader::new(wad_bytes) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
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

/// Downloads the base champion loading portrait from Data Dragon, decodes
/// the JPEG, and writes it as a PNG at `dest`. Uses the `/loading/` endpoint
/// (308×560 portrait) rather than `/splash/` (1215×717 landscape) so the
/// aspect ratio matches the textures extracted from mods. Sanitizes the
/// champion name by stripping non-alphanumeric characters so "Miss Fortune"
/// becomes "MissFortune" etc. — matches Data Dragon's internal naming.
fn fetch_ddragon_splash(champion: &str, dest: &Path) -> Result<()> {
    let sanitized: String = champion
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if sanitized.is_empty() {
        return Err(anyhow!("empty champion name"));
    }
    let url = format!(
        "https://ddragon.leagueoflegends.com/cdn/img/champion/loading/{sanitized}_0.jpg"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(DDRAGON_TIMEOUT)
        .build()
        .context("building HTTP client")?;
    let resp = client.get(&url).send().context("fetching Data Dragon")?;
    if !resp.status().is_success() {
        return Err(anyhow!("Data Dragon returned status {}", resp.status()));
    }
    let bytes = resp.bytes().context("reading Data Dragon response")?;

    let img = image::load_from_memory(&bytes).context("decoding JPEG")?;
    img.save(dest).context("writing PNG")?;
    Ok(())
}
