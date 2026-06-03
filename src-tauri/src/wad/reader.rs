use anyhow::{anyhow, Context, Result};

/// WAD v3 container reader. The format is a 272-byte header followed by a
/// table of contents with one 32-byte entry per file. Entries are sorted by
/// `name_hash` (xxhash64 of the lowercase path), so `find_by_hash` is a
/// binary search.
///
/// This reader handles the header + TOC fully and decompresses `Raw` and
/// `Zstd` entries. Other compression types (`Gzip`, `ZstdChunked`,
/// `Satellite`) are recognized but `extract()` returns an error — those are
/// added if and when we hit a file that needs them.

pub const MAGIC: [u8; 2] = *b"RW";
pub const HEADER_SIZE: usize = 272;
pub const TOC_ENTRY_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    Raw,
    Gzip,
    Satellite,
    Zstd,
    ZstdChunked,
    Unknown(u8),
}

impl From<u8> for CompressionType {
    fn from(b: u8) -> Self {
        match b {
            0 => Self::Raw,
            1 => Self::Gzip,
            2 => Self::Satellite,
            3 => Self::Zstd,
            4 => Self::ZstdChunked,
            n => Self::Unknown(n),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WadEntry {
    pub name_hash: u64,
    pub offset: u32,
    pub size_compressed: u32,
    pub size_decompressed: u32,
    pub compression: CompressionType,
}

pub struct WadReader<'a> {
    data: &'a [u8],
    entries: Vec<WadEntry>,
}

impl<'a> WadReader<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(anyhow!(
                "file too small for WAD header ({} bytes)",
                data.len()
            ));
        }
        if data[0..2] != MAGIC {
            return Err(anyhow!(
                "invalid WAD magic: {:02x} {:02x}",
                data[0],
                data[1]
            ));
        }
        let version_major = data[2];
        if version_major != 3 {
            return Err(anyhow!("unsupported WAD version {}.x", version_major));
        }

        let entry_count = u32::from_le_bytes(data[268..272].try_into().unwrap()) as usize;
        let toc_end = HEADER_SIZE + entry_count * TOC_ENTRY_SIZE;
        if data.len() < toc_end {
            return Err(anyhow!(
                "TOC extends past end of file ({} entries, need {} bytes, have {})",
                entry_count,
                toc_end,
                data.len()
            ));
        }

        let mut entries = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let off = HEADER_SIZE + i * TOC_ENTRY_SIZE;
            let e = &data[off..off + TOC_ENTRY_SIZE];
            entries.push(WadEntry {
                name_hash: u64::from_le_bytes(e[0..8].try_into().unwrap()),
                offset: u32::from_le_bytes(e[8..12].try_into().unwrap()),
                size_compressed: u32::from_le_bytes(e[12..16].try_into().unwrap()),
                size_decompressed: u32::from_le_bytes(e[16..20].try_into().unwrap()),
                compression: CompressionType::from(e[20]),
                // bytes 21-31: subchunk info (3 bytes) + checksum (8 bytes), ignored
            });
        }

        Ok(Self { data, entries })
    }

    pub fn entries(&self) -> &[WadEntry] {
        &self.entries
    }

    /// Finds an entry by its pre-computed xxhash64 path hash. Entries are
    /// sorted by hash in the TOC so this is a binary search. Use together
    /// with `xxhash_rust::xxh64::xxh64(path.to_ascii_lowercase().as_bytes(), 0)`
    /// to look up a specific asset path.
    pub fn find_by_hash(&self, hash: u64) -> Option<&WadEntry> {
        self.entries
            .binary_search_by_key(&hash, |e| e.name_hash)
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn extract(&self, entry: &WadEntry) -> Result<Vec<u8>> {
        let start = entry.offset as usize;
        let end = start
            .checked_add(entry.size_compressed as usize)
            .ok_or_else(|| anyhow!("entry size overflow"))?;
        if end > self.data.len() {
            return Err(anyhow!(
                "entry payload out of bounds: {}..{} (file size {})",
                start,
                end,
                self.data.len()
            ));
        }
        let compressed = &self.data[start..end];

        match entry.compression {
            CompressionType::Raw => Ok(compressed.to_vec()),
            CompressionType::Zstd => {
                zstd::bulk::decompress(compressed, entry.size_decompressed as usize)
                    .context("zstd decompression failed")
            }
            CompressionType::Gzip => Err(anyhow!("gzip compression not yet supported")),
            CompressionType::ZstdChunked => {
                Err(anyhow!("zstd-chunked compression not yet supported"))
            }
            CompressionType::Satellite => Err(anyhow!("satellite entry type not supported")),
            CompressionType::Unknown(n) => Err(anyhow!("unknown compression type: {}", n)),
        }
    }
}
