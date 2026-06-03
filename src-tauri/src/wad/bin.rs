//! Minimal reader for Riot's PROP binary property format (.bin files).
//!
//! The format is a tagged-union value tree used for skin definitions, item
//! data, character configs, etc. A file starts with a 4-byte magic (`PROP`
//! for regular bins or `PTCH` for patches), a u32 version, an optional list
//! of linked-file paths (v2+), then a list of entries — each a typed record
//! with field-name hashes (FNV-1a 32 of the lowercase field name) and
//! typed values.
//!
//! We don't materialise the full tree. Our only use case is finding the
//! loadscreen asset path so we walk every value recursively and collect
//! **strings**. The caller then filters for a string that looks like a
//! loadscreen (contains "loadscreen" + ends in `.tex`/`.dds`).

/// Walks a PROP bin buffer and returns every string value inside it.
/// Returns an empty vec if the buffer isn't a recognized bin or parsing
/// fails partway — we're best-effort since mods ship varied formats.
pub fn collect_strings(data: &[u8]) -> Vec<String> {
    let mut r = Reader::new(data);
    let mut out = Vec::new();

    let Some(magic) = r.read_bytes(4) else {
        return out;
    };
    // PTCH files reference/patch other bins; we only handle PROP.
    if magic != b"PROP" {
        return out;
    }
    let Some(version) = r.u32() else { return out };

    if version >= 2 {
        let Some(link_count) = r.u32() else {
            return out;
        };
        for _ in 0..link_count {
            if r.read_string().is_none() {
                return out;
            }
        }
    }

    let Some(entry_count) = r.u32() else {
        return out;
    };
    // Entry hashes come first, then the entries themselves.
    for _ in 0..entry_count {
        if r.u32().is_none() {
            return out;
        }
    }

    for _ in 0..entry_count {
        if r.u32().is_none() {
            return out;
        } // entry size
        if r.u32().is_none() {
            return out;
        } // class hash
        let Some(field_count) = r.u16() else {
            return out;
        };
        for _ in 0..field_count {
            if r.u32().is_none() {
                return out;
            } // name hash
            let Some(ty) = r.u8() else { return out };
            if !read_value(&mut r, ty, &mut out, 0) {
                return out;
            }
        }
    }
    out
}

/// Recursive value reader. Returns false on any parse failure (caller bails).
/// `depth` guards against malformed/pathological bins — 32 is plenty for real
/// skin configs.
fn read_value(r: &mut Reader, ty: u8, out: &mut Vec<String>, depth: u32) -> bool {
    if depth > 32 {
        return false;
    }
    match ty {
        // Primitives — fixed sizes we just skip past.
        0x00 => true,                    // none
        0x01 | 0x02 | 0x03 => r.skip(1), // bool / i8 / u8
        0x04 | 0x05 => r.skip(2),        // i16 / u16
        0x06 | 0x07 | 0x0A => r.skip(4), // i32 / u32 / f32
        0x08 | 0x09 => r.skip(8),        // i64 / u64
        0x0B => r.skip(8),               // vec2
        0x0C => r.skip(12),              // vec3
        0x0D => r.skip(16),              // vec4
        0x0E => r.skip(64),              // mtx44
        0x0F => r.skip(4),               // rgba
        0x11 => r.skip(4),               // hash (u32 FNV)
        0x12 => r.skip(8),               // file (u64 xxhash path ref)
        0x84 => r.skip(4),               // link (u32)
        0x87 => r.skip(1),               // flag (u8)

        // String — u16 length + bytes. This is what we're collecting.
        0x10 => {
            let Some(len) = r.u16() else { return false };
            let Some(bytes) = r.read_bytes(len as usize) else {
                return false;
            };
            if let Ok(s) = std::str::from_utf8(bytes) {
                out.push(s.to_string());
            }
            true
        }

        // Containers.
        0x80 | 0x81 => {
            // list / list2: element type, size (u32), count (u32), items
            let Some(elem) = r.u8() else { return false };
            if r.u32().is_none() {
                return false;
            }
            let Some(count) = r.u32() else { return false };
            for _ in 0..count {
                if !read_value(r, elem, out, depth + 1) {
                    return false;
                }
            }
            true
        }
        0x82 => {
            // pointer: class_hash (u32); if 0, no payload. Otherwise embed.
            let Some(class_hash) = r.u32() else {
                return false;
            };
            if class_hash == 0 {
                return true;
            }
            read_embed_body(r, out, depth + 1)
        }
        0x83 => {
            // embed: class_hash (u32) + body
            if r.u32().is_none() {
                return false;
            }
            read_embed_body(r, out, depth + 1)
        }
        0x85 => {
            // option: element type, has_value flag, optional value
            let Some(elem) = r.u8() else { return false };
            let Some(has) = r.u8() else { return false };
            if has != 0 {
                return read_value(r, elem, out, depth + 1);
            }
            true
        }
        0x86 => {
            // map: key type, value type, size (u32), count (u32), pairs
            let Some(key_ty) = r.u8() else { return false };
            let Some(val_ty) = r.u8() else { return false };
            if r.u32().is_none() {
                return false;
            }
            let Some(count) = r.u32() else { return false };
            for _ in 0..count {
                if !read_value(r, key_ty, out, depth + 1) {
                    return false;
                }
                if !read_value(r, val_ty, out, depth + 1) {
                    return false;
                }
            }
            true
        }

        // Unknown type tag — stop, we've lost sync with the stream.
        _ => false,
    }
}

/// Shared body reader for `embed` and non-null `pointer` — after the class
/// hash has been consumed: size (u32), field count (u16), then fields.
fn read_embed_body(r: &mut Reader, out: &mut Vec<String>, depth: u32) -> bool {
    if r.u32().is_none() {
        return false;
    }
    let Some(field_count) = r.u16() else {
        return false;
    };
    for _ in 0..field_count {
        if r.u32().is_none() {
            return false;
        } // name hash
        let Some(ty) = r.u8() else { return false };
        if !read_value(r, ty, out, depth + 1) {
            return false;
        }
    }
    true
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        if end > self.data.len() {
            return None;
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn skip(&mut self, n: usize) -> bool {
        let Some(end) = self.pos.checked_add(n) else {
            return false;
        };
        if end > self.data.len() {
            return false;
        }
        self.pos = end;
        true
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.read_bytes(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.read_bytes(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.read_bytes(4)?.try_into().ok()?))
    }

    fn read_string(&mut self) -> Option<&'a [u8]> {
        let len = self.u16()?;
        self.read_bytes(len as usize)
    }
}
