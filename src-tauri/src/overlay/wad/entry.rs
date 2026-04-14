use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use xxhash_rust::xxh3::xxh3_64;

use super::toc::{EntryLoc, EntryType};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Shared handle to entry payload + metadata. Cloning is cheap (Arc bump);
/// conflict walks and archive merges lean on this to avoid copying bytes.
#[derive(Clone)]
pub struct EntryData {
    inner: Arc<EntryInner>,
}

struct EntryInner {
    entry_type: EntryType,
    subchunk_count: u8,
    subchunk_index: u32,
    size_decompressed: u64,
    /// Set via `mark_optimal` after loading a game WAD so we don't
    /// re-compress already-compressed entries on write. Unmarked entries
    /// from fresh directory inputs go through `into_optimal` which
    /// compresses them if they're not audio payloads.
    is_optimal: AtomicBool,
    /// Zero until computed — XXH3 returns 0 essentially never for real input,
    /// so 0 is a safe sentinel matching cslol's `mutable std::uint64_t checksum`.
    cached_checksum: AtomicU64,
    bytes: Vec<u8>,
}

impl EntryData {
    pub fn from_raw(bytes: Vec<u8>, checksum: u64) -> Self {
        let size = bytes.len() as u64;
        Self {
            inner: Arc::new(EntryInner {
                entry_type: EntryType::Raw,
                subchunk_count: 0,
                subchunk_index: 0,
                size_decompressed: size,
                is_optimal: AtomicBool::new(false),
                cached_checksum: AtomicU64::new(checksum),
                bytes,
            }),
        }
    }

    pub fn from_link(bytes: Vec<u8>, checksum: u64) -> Self {
        let size = bytes.len() as u64;
        Self {
            inner: Arc::new(EntryInner {
                entry_type: EntryType::Link,
                subchunk_count: 0,
                subchunk_index: 0,
                size_decompressed: size,
                is_optimal: AtomicBool::new(false),
                cached_checksum: AtomicU64::new(checksum),
                bytes,
            }),
        }
    }

    pub fn from_zstd(bytes: Vec<u8>, size_decompressed: u64, checksum: u64) -> Self {
        Self {
            inner: Arc::new(EntryInner {
                entry_type: EntryType::Zstd,
                subchunk_count: 0,
                subchunk_index: 0,
                size_decompressed,
                is_optimal: AtomicBool::new(false),
                cached_checksum: AtomicU64::new(checksum),
                bytes,
            }),
        }
    }

    pub fn from_zstd_multi(
        bytes: Vec<u8>,
        size_decompressed: u64,
        checksum: u64,
        subchunk_count: u8,
        subchunk_index: u32,
    ) -> Self {
        Self {
            inner: Arc::new(EntryInner {
                entry_type: EntryType::ZstdMulti,
                subchunk_count,
                subchunk_index,
                size_decompressed,
                is_optimal: AtomicBool::new(false),
                cached_checksum: AtomicU64::new(checksum),
                bytes,
            }),
        }
    }

    pub fn from_gzip(bytes: Vec<u8>, size_decompressed: u64, checksum: u64) -> Self {
        Self {
            inner: Arc::new(EntryInner {
                entry_type: EntryType::Gzip,
                subchunk_count: 0,
                subchunk_index: 0,
                size_decompressed,
                is_optimal: AtomicBool::new(false),
                cached_checksum: AtomicU64::new(checksum),
                bytes,
            }),
        }
    }

    /// Slices an entry payload out of a parent WAD byte buffer at the
    /// location described by the TOC entry. Mirrors `EntryData::from_loc`
    /// in cslol: the loaded type + payload are trusted, and the stored
    /// `loc.checksum` is used as the initial cache.
    pub fn from_loc(src: &[u8], loc: &EntryLoc) -> Result<Self> {
        let start = loc.offset as usize;
        let end = start
            .checked_add(loc.size as usize)
            .ok_or_else(|| anyhow!("entry offset+size overflow"))?;
        if end > src.len() {
            return Err(anyhow!(
                "entry payload out of bounds: {}..{} (buffer size {})",
                start,
                end,
                src.len()
            ));
        }
        let bytes = src[start..end].to_vec();
        Ok(match loc.entry_type {
            EntryType::Raw => Self::from_raw(bytes, loc.checksum),
            EntryType::Link => Self::from_link(bytes, loc.checksum),
            EntryType::Zstd => Self::from_zstd(bytes, loc.size_decompressed, loc.checksum),
            EntryType::ZstdMulti => Self::from_zstd_multi(
                bytes,
                loc.size_decompressed,
                loc.checksum,
                loc.subchunk_count,
                loc.subchunk_index,
            ),
            EntryType::Gzip => Self::from_gzip(bytes, loc.size_decompressed, loc.checksum),
        })
    }

    pub fn entry_type(&self) -> EntryType {
        self.inner.entry_type
    }

    pub fn subchunk_count(&self) -> u8 {
        self.inner.subchunk_count
    }

    pub fn subchunk_index(&self) -> u32 {
        self.inner.subchunk_index
    }

    pub fn size_decompressed(&self) -> u64 {
        self.inner.size_decompressed
    }

    pub fn bytes(&self) -> &[u8] {
        &self.inner.bytes
    }

    pub fn bytes_len(&self) -> usize {
        self.inner.bytes.len()
    }

    /// XXH3 of the current payload bytes, cached. Note: this is the checksum
    /// of the ON-DISK form (whatever type the entry is in), not the
    /// decompressed content. Raw and Zstd of the same content have different
    /// checksums — that's by design; it's how identical blobs get deduped in
    /// `Archive::write_to_file`.
    pub fn checksum(&self) -> u64 {
        let cached = self.inner.cached_checksum.load(Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }
        let computed = xxh3_64(&self.inner.bytes);
        self.inner.cached_checksum.store(computed, Ordering::Relaxed);
        computed
    }

    pub fn mark_optimal(&self) {
        self.inner.is_optimal.store(true, Ordering::Relaxed);
    }

    pub fn is_optimal(&self) -> bool {
        self.inner.is_optimal.load(Ordering::Relaxed)
    }

    /// Decompresses payload to raw bytes if needed, returning a fresh
    /// `EntryData`. Raw and Link entries return a clone of self.
    pub fn into_decompressed(&self) -> Result<Self> {
        match self.inner.entry_type {
            EntryType::Raw | EntryType::Link => Ok(self.clone()),
            EntryType::Zstd => {
                let out = zstd::bulk::decompress(
                    &self.inner.bytes,
                    self.inner.size_decompressed as usize,
                )
                .context("zstd decompression failed")?;
                Ok(Self::from_raw(out, 0))
            }
            EntryType::ZstdMulti => {
                let out = decompress_zstd_hack(
                    &self.inner.bytes,
                    self.inner.size_decompressed as usize,
                )?;
                Ok(Self::from_raw(out, 0))
            }
            EntryType::Gzip => Err(anyhow!(
                "gzip-compressed WAD entries are not supported — re-export the mod from cslol-manager"
            )),
        }
    }

    /// Compresses payload to a single zstd frame if needed, returning a fresh
    /// `EntryData`. Zstd and Link entries return a clone of self; Raw gets
    /// compressed; Gzip and ZstdMulti are round-tripped through `Raw`.
    pub fn into_compressed(&self) -> Result<Self> {
        match self.inner.entry_type {
            EntryType::Link | EntryType::Zstd => Ok(self.clone()),
            EntryType::Raw => {
                let compressed = zstd::bulk::compress(&self.inner.bytes, 3)
                    .context("zstd compression failed")?;
                Ok(Self::from_zstd(
                    compressed,
                    self.inner.size_decompressed,
                    0,
                ))
            }
            EntryType::ZstdMulti | EntryType::Gzip => {
                self.into_decompressed()?.into_compressed()
            }
        }
    }

    /// Applies the cslol optimization rules:
    ///   - Raw that isn't an audio payload → compress to Zstd
    ///   - Raw audio (`.bnk` / `.wpk`) → stay Raw (the client won't play them decompressed)
    ///   - Zstd audio → decompress back to Raw
    ///   - ZstdMulti audio → decompress back to Raw
    ///   - ZstdMulti non-audio → re-encode as plain Zstd
    ///   - Link → unchanged
    ///
    /// Entries already marked optimal (game WADs via `mark_optimal`) short-circuit.
    pub fn into_optimal(&self) -> Result<Self> {
        if self.is_optimal() {
            return Ok(self.clone());
        }
        let result = match self.inner.entry_type {
            EntryType::Raw => {
                if is_audio(detect_extension(&self.inner.bytes)) {
                    self.clone()
                } else {
                    self.into_compressed()?
                }
            }
            EntryType::Link => self.clone(),
            EntryType::Gzip => self.into_decompressed()?.into_optimal()?,
            EntryType::Zstd => {
                let head = decompress_head(&self.inner.bytes, EntryType::Zstd, 16)?;
                if is_audio(detect_extension(&head)) {
                    self.into_decompressed()?
                } else {
                    self.clone()
                }
            }
            EntryType::ZstdMulti => {
                let head = decompress_head(&self.inner.bytes, EntryType::ZstdMulti, 16)?;
                if is_audio(detect_extension(&head)) {
                    self.into_decompressed()?
                } else {
                    self.into_decompressed()?.into_compressed()?
                }
            }
        };
        result.mark_optimal();
        Ok(result)
    }
}

/// Result of magic-number sniffing for the optimization rules. Only the two
/// audio formats matter; everything else is "compress me".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedKind {
    Bnk,
    Wpk,
    Other,
}

fn is_audio(kind: DetectedKind) -> bool {
    matches!(kind, DetectedKind::Bnk | DetectedKind::Wpk)
}

/// Minimal magic-byte sniffer matching cslol's Magic table for the two
/// formats `into_optimal` cares about. cslol has a 50-entry table but only
/// `.bnk` and `.wpk` change the optimization path; everything else maps to
/// "compress" which is also `Other`'s behavior here.
///
/// The `r3d2` check guards against cslol's more specific `r3d2{Mesh,sklt,...}`
/// matches which precede the generic `r3d2` entry in their table — we
/// replicate that so a `.scb` mesh doesn't get misclassified as audio.
fn detect_extension(bytes: &[u8]) -> DetectedKind {
    if bytes.starts_with(b"BKHD") {
        return DetectedKind::Bnk;
    }
    if bytes.starts_with(b"r3d2") {
        const SPECIFIC: &[&[u8]] = &[
            b"r3d2Mesh",
            b"r3d2aims",
            b"r3d2anmd",
            b"r3d2canm",
            b"r3d2sklt",
            b"r3d2blnd",
            b"r3d2wght",
        ];
        if !SPECIFIC.iter().any(|m| bytes.starts_with(m)) {
            return DetectedKind::Wpk;
        }
    }
    DetectedKind::Other
}

/// Decompresses up to `max` bytes from the head of a (possibly multi-frame)
/// zstd payload for extension detection. 16 bytes is enough to cover every
/// magic we care about — the whole point is to avoid decompressing multi-GB
/// textures just to look at the first few bytes.
fn decompress_head(src: &[u8], kind: EntryType, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max);
    let frame_start = if kind == EntryType::ZstdMulti {
        find_zstd_magic(src)
    } else {
        0
    };
    if frame_start > 0 {
        let take = frame_start.min(max);
        out.extend_from_slice(&src[..take]);
        if out.len() >= max {
            return Ok(out);
        }
    }
    let remaining = &src[frame_start..];
    if remaining.is_empty() {
        return Ok(out);
    }
    let mut decoder = zstd::stream::read::Decoder::new(remaining)
        .context("initialising zstd streaming decoder")?;
    let want = max - out.len();
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match decoder.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            // Partial results are fine — we just need enough bytes to see the magic.
            Err(_) => break,
        }
    }
    out.extend_from_slice(&buf[..filled]);
    Ok(out)
}

/// Full-payload decompressor for `ZstdMulti`. Entries of this type may have a
/// raw prefix (typically a SubChunkTOC header) that precedes the zstd magic;
/// the prefix is copied verbatim and the rest is decompressed as a normal
/// zstd frame sized to `(total_decompressed - prefix_len)`.
fn decompress_zstd_hack(src: &[u8], total_decompressed: usize) -> Result<Vec<u8>> {
    let frame_start = find_zstd_magic(src);
    if frame_start > total_decompressed {
        return Err(anyhow!(
            "zstd-multi prefix {} exceeds expected size {}",
            frame_start,
            total_decompressed
        ));
    }
    let mut out = Vec::with_capacity(total_decompressed);
    if frame_start > 0 {
        out.extend_from_slice(&src[..frame_start]);
    }
    let remaining = &src[frame_start..];
    if remaining.is_empty() {
        return Ok(out);
    }
    let remaining_size = total_decompressed - frame_start;
    let decompressed = zstd::bulk::decompress(remaining, remaining_size)
        .context("zstd-multi decompression failed")?;
    if decompressed.len() != remaining_size {
        return Err(anyhow!(
            "zstd-multi size mismatch: expected {} got {}",
            remaining_size,
            decompressed.len()
        ));
    }
    out.extend_from_slice(&decompressed);
    Ok(out)
}

fn find_zstd_magic(src: &[u8]) -> usize {
    src.windows(4).position(|w| w == ZSTD_MAGIC).unwrap_or(0)
}
