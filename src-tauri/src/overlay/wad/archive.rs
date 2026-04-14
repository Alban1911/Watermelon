use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use xxhash_rust::xxh3::Xxh3;

use super::entry::EntryData;
use super::toc::{
    write_header, write_v34_entry, EntryLoc, Toc, ENTRY_SIZE, HEADER_SIZE, LATEST_MAJOR,
    LATEST_MINOR,
};
use crate::overlay::hash::xxh64_from_path;

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_ENTRY_SIZE: u64 = 4 * GIB;

pub type EntryMap = BTreeMap<u64, EntryData>;

#[derive(Clone)]
pub struct Archive {
    pub entries: EntryMap,
    pub signature: [u8; 256],
}

impl Default for Archive {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            signature: [0u8; 256],
        }
    }
}

impl Archive {
    /// Reads an archive out of an already-parsed TOC backed by the full WAD
    /// byte buffer. Entries that share a TOC checksum share the same
    /// `EntryData` handle — matches cslol's `descriptors_by_checksum` dedupe.
    pub fn read_from_toc(src: &[u8], toc: &Toc) -> Result<Self> {
        let mut archive = Archive {
            entries: BTreeMap::new(),
            signature: toc.signature,
        };
        let mut descriptors_by_checksum: HashMap<u64, EntryData> = HashMap::new();
        for toc_entry in &toc.entries {
            let data = if toc_entry.loc.checksum != 0 {
                if let Some(existing) = descriptors_by_checksum.get(&toc_entry.loc.checksum) {
                    existing.clone()
                } else {
                    let d = EntryData::from_loc(src, &toc_entry.loc)?;
                    descriptors_by_checksum.insert(toc_entry.loc.checksum, d.clone());
                    d
                }
            } else {
                EntryData::from_loc(src, &toc_entry.loc)?
            };
            archive.entries.insert(toc_entry.name, data);
        }
        Ok(archive)
    }

    pub fn read_from_bytes(src: &[u8]) -> Result<Self> {
        let toc = Toc::read(src)?;
        Self::read_from_toc(src, &toc)
    }

    pub fn read_from_file(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("reading WAD file {}", path.display()))?;
        Self::read_from_bytes(&bytes)
    }

    /// Packs all files in `dir` (recursive) into a fresh archive as Raw
    /// entries, hashing paths via `xxh64_from_path`. Used for fantome `RAW/`
    /// trees.
    pub fn pack_from_directory(dir: &Path) -> Result<Self> {
        let mut archive = Archive::default();
        pack_recursive(dir, dir, &mut archive)?;
        Ok(archive)
    }

    pub fn mark_optimal(&self) {
        for entry in self.entries.values() {
            entry.mark_optimal();
        }
    }

    pub fn estimate_size(&self) -> usize {
        let mut est = HEADER_SIZE + ENTRY_SIZE * self.entries.len();
        for entry in self.entries.values() {
            est += entry.bytes_len();
        }
        est
    }

    /// Writes the archive to `path` as a v3.4 WAD. Idempotent: if the first
    /// `HEADER_SIZE` bytes already match what we'd write, no-op (the header
    /// checksum is a deterministic function of the archive's current state).
    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        let desc_count = self.entries.len() as u32;

        // 1. Compute header checksum over pre-optimization state.
        // cslol's write_to_file pattern: the header's checksum is hashed from
        // each entry's CURRENT payload checksum (not the post-`into_optimal`
        // checksum), because game-loaded entries are already marked optimal
        // and have their pre-computed checksums stored from the source TOC.
        // That makes "load an unmodified WAD and write it back out" a true
        // no-op via the idempotency check below.
        let header_checksum: [u8; 8] = {
            let mut hasher = Xxh3::new();
            hasher.update(&[b'R', b'W', LATEST_MAJOR, LATEST_MINOR]);
            for (name, data) in &self.entries {
                hasher.update(&name.to_le_bytes());
                hasher.update(&data.checksum().to_le_bytes());
            }
            hasher.digest().to_le_bytes()
        };

        let mut new_header = [0u8; HEADER_SIZE];
        write_header(
            &mut new_header,
            &self.signature,
            header_checksum,
            desc_count,
        );

        // 2. Idempotency: short-circuit if on-disk header already matches.
        if let Ok(mut f) = fs::File::open(path) {
            let mut existing = [0u8; HEADER_SIZE];
            if f.read_exact(&mut existing).is_ok() && existing == new_header {
                return Ok(());
            }
        }

        // 3. Build TOC entries + data buffer.
        // BTreeMap iterates in u64-ascending order, which matches the "TOC
        // sorted by name" invariant at the end of write — we don't need a
        // separate sort pass.
        let data_start = HEADER_SIZE + ENTRY_SIZE * desc_count as usize;
        let mut data_buf: Vec<u8> = Vec::new();
        let mut toc_entries: Vec<(u64, EntryLoc)> = Vec::with_capacity(self.entries.len());
        let mut loc_by_checksum: HashMap<u64, EntryLoc> = HashMap::with_capacity(self.entries.len());

        for (name, entry) in &self.entries {
            let optimized = entry.into_optimal()?;
            let cksum = optimized.checksum();

            let loc = if let Some(existing) = loc_by_checksum.get(&cksum) {
                *existing
            } else {
                let offset = (data_start + data_buf.len()) as u64;
                let size = optimized.bytes_len() as u64;
                let size_decompressed = optimized.size_decompressed();
                if offset >= MAX_ENTRY_SIZE
                    || size >= MAX_ENTRY_SIZE
                    || size_decompressed >= MAX_ENTRY_SIZE
                {
                    return Err(anyhow!("WAD entry exceeds 4 GiB limit (offset {} size {} size_decompressed {})", offset, size, size_decompressed));
                }
                data_buf.extend_from_slice(optimized.bytes());
                let new_loc = EntryLoc {
                    entry_type: optimized.entry_type(),
                    subchunk_count: optimized.subchunk_count(),
                    subchunk_index: optimized.subchunk_index(),
                    offset,
                    size,
                    size_decompressed,
                    checksum: cksum,
                };
                loc_by_checksum.insert(cksum, new_loc);
                new_loc
            };
            toc_entries.push((*name, loc));
        }

        // 4. Sort TOC by name — BTreeMap order is already ascending, but the
        // cslol invariant is explicit and cheap to enforce so callers that
        // construct archives from other sources still get a valid WAD.
        toc_entries.sort_by_key(|(n, _)| *n);

        // 5. Materialize header + TOC + data into one buffer and write.
        let total_len = data_start + data_buf.len();
        let mut out = vec![0u8; total_len];
        out[..HEADER_SIZE].copy_from_slice(&new_header);
        for (i, (name, loc)) in toc_entries.iter().enumerate() {
            let off = HEADER_SIZE + i * ENTRY_SIZE;
            write_v34_entry(&mut out[off..off + ENTRY_SIZE], *name, loc);
        }
        out[data_start..].copy_from_slice(&data_buf);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", path.display()))?;
        }
        fs::write(path, &out)
            .with_context(|| format!("writing WAD file {}", path.display()))?;
        Ok(())
    }

    /// Two-pointer merge walk over sorted key ranges. Invokes `f` once per
    /// name hash present in both archives.
    pub fn for_each_overlap<F>(&self, other: &Archive, mut f: F)
    where
        F: FnMut(u64, &EntryData, &EntryData),
    {
        let mut lhs = self.entries.iter();
        let mut rhs = other.entries.iter();
        let mut l = lhs.next();
        let mut r = rhs.next();
        while let (Some((lk, lv)), Some((rk, rv))) = (l, r) {
            if lk < rk {
                l = lhs.next();
            } else if lk > rk {
                r = rhs.next();
            } else {
                f(*lk, lv, rv);
                l = lhs.next();
                r = rhs.next();
            }
        }
    }

    /// Mutable version of `for_each_overlap`. Collects the overlapping key
    /// set first, then mutates in a second pass — borrow-checker friendly and
    /// still O(n+m) on the walk.
    pub fn for_each_overlap_mut<F>(&mut self, other: &Archive, mut f: F)
    where
        F: FnMut(u64, &mut EntryData, &EntryData),
    {
        let matches = overlap_keys(&self.entries, &other.entries);
        for name in matches {
            if let (Some(lv), Some(rv)) = (
                self.entries.get_mut(&name),
                other.entries.get(&name),
            ) {
                f(name, lv, rv);
            }
        }
    }

    /// Removes every overlapping entry for which `f` returns true.
    pub fn erase_overlap<F>(&mut self, other: &Archive, mut f: F)
    where
        F: FnMut(u64, &EntryData, &EntryData) -> bool,
    {
        let matches = overlap_keys(&self.entries, &other.entries);
        for name in matches {
            let erase = {
                let Some(lv) = self.entries.get(&name) else {
                    continue;
                };
                let Some(rv) = other.entries.get(&name) else {
                    continue;
                };
                f(name, lv, rv)
            };
            if erase {
                self.entries.remove(&name);
            }
        }
    }

    /// Returns an archive containing the entries from `upper` that also
    /// exist in `self`. Used by `Index::add_overlay_mod` to figure out which
    /// mod changes leak into other game WADs.
    pub fn overlapping(&self, upper: &Archive) -> Archive {
        let mut result = Archive::default();
        self.for_each_overlap(upper, |name, _lhs, rhs| {
            result.entries.insert(name, rhs.clone());
        });
        result
    }

    /// Inserts every entry from `other`, overwriting on name collision.
    pub fn merge_in(&mut self, other: &Archive) {
        for (name, data) in &other.entries {
            self.entries.insert(*name, data.clone());
        }
    }
}

fn overlap_keys(lhs: &EntryMap, rhs: &EntryMap) -> Vec<u64> {
    let mut out = Vec::new();
    let mut li = lhs.keys();
    let mut ri = rhs.keys();
    let mut l = li.next();
    let mut r = ri.next();
    while let (Some(&lk), Some(&rk)) = (l, r) {
        if lk < rk {
            l = li.next();
        } else if lk > rk {
            r = ri.next();
        } else {
            out.push(lk);
            l = li.next();
            r = ri.next();
        }
    }
    out
}

fn pack_recursive(root: &Path, current: &Path, archive: &mut Archive) -> Result<()> {
    for entry in fs::read_dir(current)
        .with_context(|| format!("reading directory {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            pack_recursive(root, &path, archive)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let hash = xxh64_from_path(&rel);
            let bytes = fs::read(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            archive.entries.insert(hash, EntryData::from_raw(bytes, 0));
        }
    }
    Ok(())
}
