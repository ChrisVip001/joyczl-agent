//! Image header parsing: width and height from the file header alone, no dependencies.
//!
//! Why it exists: vision providers reject oversized images (the tall screenshot is the
//! classic — 1120×11100 and the like), and the far end answers with one sentence:
//! "unsupported image. Please make sure your image is valid and has one of the following
//! formats: webp, png, jpeg, gif" — no real reason, no limit. Measuring the dimensions
//! locally lets the error carry the actual numbers and a next step the user can take.
//!
//! Only the four formats `media_type_for_extension` accepts; anything unrecognised returns
//! `None` — this code does not guess.

/// The per-side pixel limit.
///
/// 4096 is the conservative floor across vendors: anything larger is refused outright
/// (Anthropic allows 8000, OpenAI suggests a short side ≤2048, and OpenAI-compatible
/// endpoints like DeepSeek are stricter still). It is **not** an exact limit — another
/// vendor may be stricter — so the error says "local conservative limit" and does not
/// speak for the far end.
pub const MAX_EDGE: u32 = 4096;

/// Width and height in pixels. `None` = the header is unrecognised: either corrupt, or not
/// one of those four formats.
pub fn size(bytes: &[u8]) -> Option<(u32, u32)> {
    png(bytes)
        .or_else(|| gif(bytes))
        .or_else(|| jpeg(bytes))
        .or_else(|| webp(bytes))
}

/// PNG: IHDR follows the signature; width and height are big-endian u32 at 16..20 and
/// 20..24.
fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !bytes.starts_with(&SIGNATURE) {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// GIF: width and height from the logical screen descriptor, little-endian u16.
fn gif(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"GIF8") {
        return None;
    }
    let width = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
    let height = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
    Some((u32::from(width), u32::from(height)))
}

/// JPEG has no fixed header: walk the segments up to the first SOF (FFC0–FFCF, skipping
/// C4/C8/CC, which share the range but are not SOF markers).
fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut at = 2;
    while at + 9 <= bytes.len() {
        if bytes[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let height = u16::from_be_bytes(bytes.get(at + 5..at + 7)?.try_into().ok()?);
            let width = u16::from_be_bytes(bytes.get(at + 7..at + 9)?.try_into().ok()?);
            return Some((u32::from(width), u32::from(height)));
        }
        // Past the scan header (FFDA) comes entropy-coded data; there are no dimensions
        // beyond this point.
        if marker == 0xDA {
            return None;
        }
        let len = u16::from_be_bytes(bytes.get(at + 2..at + 4)?.try_into().ok()?);
        if len < 2 {
            return None;
        }
        at += 2 + usize::from(len);
    }
    None
}

/// Three WebP variants: the extended format (VP8X) states the canvas size, while the
/// lossy and lossless ones hide it in the frame header.
fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || !bytes.starts_with(b"RIFF") || bytes[8..12] != b"WEBP"[..] {
        return None;
    }
    let kind = bytes.get(12..16)?;
    if kind == &b"VP8X"[..] {
        // 24-bit little-endian "width−1 / height−1": the canvas starts after flags(1) and
        // reserved(3).
        let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
        let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
        return Some((width, height));
    }
    if kind == &b"VP8 "[..] {
        // After a 3-byte frame tag and the 3-byte start code (9D 01 2A) come 14-bit width
        // and height.
        let width = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF);
        let height = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF);
        return Some((width, height));
    }
    if kind == &b"VP8L"[..] {
        // After the 0x2F signature: 14 bits of "width−1" then 14 bits of "height−1", in a
        // little-endian bit stream.
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
    }
    None
}
