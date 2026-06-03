use anyhow::{anyhow, Result};

pub const HEADER_SIZE: usize = 272;
pub const ENTRY_SIZE: usize = 32;

pub const MAGIC: [u8; 2] = *b"RW";

/// Version 3.4 is what the live client accepts; we only write this version.
pub const LATEST_MAJOR: u8 = 3;
pub const LATEST_MINOR: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryType {
    Raw = 0,
    Link = 1,
    Gzip = 2,
    Zstd = 3,
    ZstdMulti = 4,
}

impl EntryType {
    pub fn from_nibble(v: u8) -> Result<Self> {
        match v & 0x0F {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Link),
            2 => Ok(Self::Gzip),
            3 => Ok(Self::Zstd),
            4 => Ok(Self::ZstdMulti),
            n => Err(anyhow!("unknown WAD entry type nibble: {}", n)),
        }
    }
}

/// Normalized location + metadata for a single entry as read from a TOC.
/// Matches `lol::wad::EntryLoc` — the shape is the same regardless of which
/// WAD v3.x sub-version the entry was serialized in.
#[derive(Debug, Clone, Copy)]
pub struct EntryLoc {
    pub entry_type: EntryType,
    pub subchunk_count: u8,
    pub subchunk_index: u32,
    pub offset: u64,
    pub size: u64,
    pub size_decompressed: u64,
    pub checksum: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TocEntry {
    pub name: u64,
    pub loc: EntryLoc,
}

pub struct Toc {
    pub major: u8,
    pub minor: u8,
    pub signature: [u8; 256],
    pub entries: Vec<TocEntry>,
}

impl Toc {
    /// Parses a WAD header + TOC out of `src`. Supports v3.0, v3.1-3.3 and
    /// v3.4 on-disk entry layouts (the normalized form is the same). Rejects
    /// v0/v1/v2 — the game refuses anything but the latest version, and
    /// overlay output is always v3.4, so older variants can never round-trip.
    pub fn read(src: &[u8]) -> Result<Self> {
        if src.len() < 4 {
            return Err(anyhow!("file too small for WAD version header"));
        }
        if src[0..4] == [0, 0, 0, 0] {
            return Err(anyhow!("zero-initialized file (empty or locale artifact)"));
        }
        if src[0..2] != MAGIC {
            return Err(anyhow!(
                "not a WAD file (bad magic: {:02x} {:02x})",
                src[0],
                src[1]
            ));
        }
        let major = src[2];
        let minor = src[3];
        if major != 3 {
            return Err(anyhow!(
                "unsupported WAD major version: {}.{}",
                major,
                minor
            ));
        }

        if src.len() < HEADER_SIZE {
            return Err(anyhow!(
                "file too small for v3 header ({} bytes)",
                src.len()
            ));
        }
        let mut signature = [0u8; 256];
        signature.copy_from_slice(&src[4..260]);
        // src[260..268] = stored header checksum — we don't validate it, we
        // recompute on write.
        let desc_count = u32::from_le_bytes(src[268..272].try_into().unwrap()) as usize;

        let layout = EntryLayout::for_minor(minor);
        let toc_end = HEADER_SIZE + ENTRY_SIZE * desc_count;
        if src.len() < toc_end {
            return Err(anyhow!(
                "TOC extends past end of file ({} entries need {} bytes, have {})",
                desc_count,
                toc_end,
                src.len()
            ));
        }

        let mut entries = Vec::with_capacity(desc_count);
        for i in 0..desc_count {
            let off = HEADER_SIZE + i * ENTRY_SIZE;
            let e = &src[off..off + ENTRY_SIZE];
            entries.push(parse_v3_entry(e, layout)?);
        }

        Ok(Toc {
            major,
            minor,
            signature,
            entries,
        })
    }
}

#[derive(Clone, Copy)]
enum EntryLayout {
    V3_0,
    V3_1,
    V3_4,
}

impl EntryLayout {
    fn for_minor(minor: u8) -> Self {
        match minor {
            0 => Self::V3_0,
            1 | 2 | 3 => Self::V3_1,
            _ => Self::V3_4,
        }
    }
}

fn parse_v3_entry(e: &[u8], layout: EntryLayout) -> Result<TocEntry> {
    let name = u64::from_le_bytes(e[0..8].try_into().unwrap());
    let offset = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
    let size = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
    let size_decompressed = u32::from_le_bytes(e[16..20].try_into().unwrap()) as u64;
    // MSVC/clang pack the `{EntryType : 4; subchunk_count : 4}` bitfield LSB
    // first on little-endian, so the entry type lives in the low nibble of
    // byte 20 and subchunk count in the high nibble.
    let type_byte = e[20];
    let entry_type = EntryType::from_nibble(type_byte & 0x0F)?;
    let subchunk_count = (type_byte >> 4) & 0x0F;

    let (subchunk_index, checksum) = match layout {
        EntryLayout::V3_0 => {
            // byte 21 = is_duplicate (ignored), bytes 22..24 = subchunk_index (u16 LE),
            // bytes 24..32 = checksum_old (ignored; v3.0 has no real checksum).
            let subchunk_index = u16::from_le_bytes(e[22..24].try_into().unwrap()) as u32;
            (subchunk_index, 0u64)
        }
        EntryLayout::V3_1 => {
            // byte 21 = is_duplicate (ignored), bytes 22..24 = subchunk_index (u16 LE),
            // bytes 24..32 = checksum (u64 LE).
            let subchunk_index = u16::from_le_bytes(e[22..24].try_into().unwrap()) as u32;
            let checksum = u64::from_le_bytes(e[24..32].try_into().unwrap());
            (subchunk_index, checksum)
        }
        EntryLayout::V3_4 => {
            // bytes 21..24 = UInt24ME subchunk_index with disk layout [hi, lo, mi],
            // bytes 24..32 = checksum (u64 LE).
            let hi = e[21] as u32;
            let lo = e[22] as u32;
            let mi = e[23] as u32;
            let subchunk_index = (hi << 16) | (mi << 8) | lo;
            let checksum = u64::from_le_bytes(e[24..32].try_into().unwrap());
            (subchunk_index, checksum)
        }
    };

    Ok(TocEntry {
        name,
        loc: EntryLoc {
            entry_type,
            subchunk_count,
            subchunk_index,
            offset,
            size,
            size_decompressed,
            checksum,
        },
    })
}

/// Writes a v3.4 header into the first `HEADER_SIZE` bytes of `dst`.
pub fn write_header(dst: &mut [u8], signature: &[u8; 256], checksum: [u8; 8], desc_count: u32) {
    assert!(dst.len() >= HEADER_SIZE);
    dst[0..2].copy_from_slice(&MAGIC);
    dst[2] = LATEST_MAJOR;
    dst[3] = LATEST_MINOR;
    dst[4..260].copy_from_slice(signature);
    dst[260..268].copy_from_slice(&checksum);
    dst[268..272].copy_from_slice(&desc_count.to_le_bytes());
}

/// Writes a v3.4 TOC entry (32 bytes) into `dst`.
pub fn write_v34_entry(dst: &mut [u8], name: u64, loc: &EntryLoc) {
    assert!(dst.len() >= ENTRY_SIZE);
    dst[0..8].copy_from_slice(&name.to_le_bytes());
    dst[8..12].copy_from_slice(&(loc.offset as u32).to_le_bytes());
    dst[12..16].copy_from_slice(&(loc.size as u32).to_le_bytes());
    dst[16..20].copy_from_slice(&(loc.size_decompressed as u32).to_le_bytes());
    dst[20] = ((loc.subchunk_count & 0x0F) << 4) | ((loc.entry_type as u8) & 0x0F);
    // UInt24ME on-disk order: [hi, lo, mi]
    dst[21] = ((loc.subchunk_index >> 16) & 0xFF) as u8;
    dst[22] = (loc.subchunk_index & 0xFF) as u8;
    dst[23] = ((loc.subchunk_index >> 8) & 0xFF) as u8;
    dst[24..32].copy_from_slice(&loc.checksum.to_le_bytes());
}
