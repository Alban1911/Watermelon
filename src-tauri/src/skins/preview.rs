use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zip::ZipArchive;

use crate::wad::{CompressionType, WadReader};

const DDS_MAGIC: [u8; 4] = *b"DDS ";
const DDRAGON_TIMEOUT: Duration = Duration::from_secs(5);

/// Produces (or refreshes) a PNG preview for a `.fantome` file and returns
/// its path.
///
/// Tries two sources in order:
///   1. **Inside the mod** — find the largest DDS entry in the inner WAD
///      that's closest to 16:9, decode with `image_dds`, write PNG.
///   2. **Data Dragon fallback** — if the mod ships no usable DDS (e.g.
///      texture-only mods for newer champions where Riot moved to .tex),
///      fetch the base champion splash from ddragon.leagueoflegends.com
///      using the champion name and re-encode as PNG.
///
/// Returns `Ok(None)` only if both sources fail; the caller treats that as
/// "no preview available" and renders a placeholder.
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

    if let Some(dds_bytes) = extract_largest_dds(fantome_path)? {
        write_dds_as_png(&dds_bytes, &dest)?;
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

/// Opens the `.fantome` archive, pulls the inner WAD, and returns the
/// decompressed bytes of the best splash-art DDS candidate.
///
/// Heuristic: decompress every entry, parse each DDS header for dimensions,
/// and score by aspect-ratio closeness to 16:9 (the portrait 9:16 form
/// collapses to the same score because we normalize with max/min). Model
/// diffuse/normal maps are square (ratio 1.0, deviation ~0.78) and score
/// badly; splash and loadscreen art is typically 1280×720, 1215×717, or
/// 308×560 and scores near zero. Ties broken by pixel count (bigger wins).
/// Candidates below 50 000 pixels are excluded to skip tiny icons, with a
/// fallback to the single largest DDS if nothing meets the bar.
fn extract_largest_dds(fantome_path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(fantome_path).context("open .fantome")?;
    let mut zip = ZipArchive::new(file).context("read zip")?;

    let mut wad_bytes: Option<Vec<u8>> = None;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("zip entry")?;
        let name = entry.name().to_string();
        if name.starts_with("WAD/") && name.ends_with(".wad.client") {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf).context("read WAD")?;
            wad_bytes = Some(buf);
            break;
        }
    }
    let wad_bytes = match wad_bytes {
        Some(b) => b,
        None => return Ok(None),
    };

    let reader = WadReader::new(&wad_bytes).context("parse WAD")?;

    let mut entries: Vec<_> = reader
        .entries()
        .iter()
        .filter(|e| {
            matches!(
                e.compression,
                CompressionType::Zstd | CompressionType::Raw
            )
        })
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.size_decompressed));

    struct Candidate {
        bytes: Vec<u8>,
        deviation: f64,
        pixels: u64,
    }
    const SPLASH_RATIO: f64 = 16.0 / 9.0;
    const MIN_PIXELS: u64 = 50_000;

    let mut scored: Vec<Candidate> = Vec::new();
    for entry in &entries {
        let decoded = match reader.extract(entry) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let (width, height) = match dds_dimensions(&decoded) {
            Some(wh) => wh,
            None => continue,
        };
        if width == 0 || height == 0 {
            continue;
        }
        let max = width.max(height) as f64;
        let min = width.min(height) as f64;
        let ratio = max / min;
        let deviation = (ratio - SPLASH_RATIO).abs();
        let pixels = (width as u64) * (height as u64);
        scored.push(Candidate {
            bytes: decoded,
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

    let winner = scored
        .iter()
        .find(|c| c.pixels >= MIN_PIXELS)
        .or_else(|| scored.first());

    Ok(winner.map(|c| c.bytes.clone()))
}

/// Downloads the base champion loading portrait from Data Dragon, decodes
/// the JPEG, and writes it as a PNG at `dest`. Uses the `/loading/` endpoint
/// (308×560 portrait) rather than `/splash/` (1215×717 landscape) so the
/// aspect ratio matches the DDS-extracted splashes from mods. Sanitizes
/// the champion name by stripping everything non-alphanumeric so "Miss
/// Fortune" becomes "MissFortune" etc. — matches Data Dragon's internal
/// naming for most champions.
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

fn write_dds_as_png(dds_bytes: &[u8], dest: &Path) -> Result<()> {
    let dds = image_dds::ddsfile::Dds::read(dds_bytes)
        .map_err(|e| anyhow!("parse DDS header: {e}"))?;
    let img = image_dds::image_from_dds(&dds, 0)
        .map_err(|e| anyhow!("decode DDS: {e}"))?;
    img.save(dest).context("save PNG")?;
    Ok(())
}

/// Parses a DDS header and returns `(width, height)`. Returns `None` if the
/// buffer isn't a DDS file or is too small.
fn dds_dimensions(dds: &[u8]) -> Option<(u32, u32)> {
    if dds.len() < 20 || dds[..4] != DDS_MAGIC {
        return None;
    }
    // DDS_HEADER layout: magic(4) size(4) flags(4) height(4) width(4) ...
    let height = u32::from_le_bytes(dds[12..16].try_into().ok()?);
    let width = u32::from_le_bytes(dds[16..20].try_into().ok()?);
    Some((width, height))
}
