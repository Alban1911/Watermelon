use xxhash_rust::xxh64::xxh64;

/// XXH64 over the ASCII-lowercased bytes of `s`, seed 0.
/// Matches `lol::hash::Xxh64::Xxh64(std::string_view)`: it does NOT strip any
/// prefix — used for identifiers like `.SubChunkTOC` paths that must be hashed
/// in their entirety.
pub fn xxh64_lower(s: &str) -> u64 {
    let lower: Vec<u8> = s
        .bytes()
        .map(|c| {
            if c >= b'A' && c <= b'Z' {
                c - b'A' + b'a'
            } else {
                c
            }
        })
        .collect();
    xxh64(&lower, 0)
}

/// Path-hash from `lol::hash::Xxh64::from_path`:
///   1. Strip any leading `./` or `/` characters.
///   2. If the segment before the first `.` is exactly 16 ASCII hex digits,
///      parse it directly as a u64 hash (Riot-style pre-hashed filenames).
///   3. Otherwise fall back to `xxh64_lower` over the stripped string.
pub fn xxh64_from_path(s: &str) -> u64 {
    let mut rest = s;
    while let Some(first) = rest.as_bytes().first() {
        if *first == b'.' || *first == b'/' {
            rest = &rest[1..];
        } else {
            break;
        }
    }
    let tmp_end = rest.find('.').unwrap_or(rest.len());
    let tmp = &rest[..tmp_end];
    if tmp.len() == 16 && tmp.bytes().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(v) = u64::from_str_radix(tmp, 16) {
            return v;
        }
    }
    xxh64_lower(rest)
}
