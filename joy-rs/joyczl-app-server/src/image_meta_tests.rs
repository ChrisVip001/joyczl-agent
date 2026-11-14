//! `image_meta`'s header parsing: a real header for each of the four formats, plus three
//! shapes that must come back unrecognised.

use super::image_meta;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    out.extend_from_slice(&13u32.to_be_bytes()); // IHDR data length 13
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth / colour type / compression / filter / interlace
    out
}

fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&[0xF7, 0, 0, 0, 0, 0]);
    out
}

fn jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0: segment length 16
    out.extend_from_slice(&[0u8; 14]);
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]); // SOF0: segment length 17, precision 8
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[0u8; 9]);
    out
}

fn webp_vp8x(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8X");
    out.extend_from_slice(&10u32.to_le_bytes());
    out.push(0); // flags
    out.extend_from_slice(&[0, 0, 0]); // reserved
    out.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    out.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    out
}

fn webp_vp8(width: u16, height: u16) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8 ");
    out.extend_from_slice(&10u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0]); // the frame tag
    out.extend_from_slice(&[0x9D, 0x01, 0x2A]); // start code
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out
}

fn webp_vp8l(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8L");
    out.extend_from_slice(&10u32.to_le_bytes());
    out.push(0x2F); // signature
    let bits = ((width - 1) & 0x3FFF) | (((height - 1) & 0x3FFF) << 14);
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(&[0u8; 5]); // a real VP8L block is longer; padded out to 30 bytes
    out
}

/// The image the user reported: 1120×11100, only 2.4 MB — the old byte gate let it through and
/// the far end refused it.
#[test]
fn a_long_screenshot_reports_its_real_size() {
    assert_eq!(image_meta::size(&png(1120, 11_100)), Some((1120, 11_100)));
}

#[test]
fn the_other_three_formats_are_read_from_their_own_headers() {
    assert_eq!(image_meta::size(&gif(640, 480)), Some((640, 480)));
    assert_eq!(image_meta::size(&jpeg(1760, 1470)), Some((1760, 1470)));
    assert_eq!(image_meta::size(&webp_vp8x(800, 600)), Some((800, 600)));
    assert_eq!(image_meta::size(&webp_vp8(320, 240)), Some((320, 240)));
    assert_eq!(image_meta::size(&webp_vp8l(1920, 1080)), Some((1920, 1080)));
}

/// JPEG does not keep the dimensions at a fixed offset: the APP0 and friends have to be walked
/// past before the SOF shows up.
#[test]
fn jpeg_walks_past_other_segments_to_the_sof() {
    let bytes = jpeg(4000, 3000);
    assert_eq!(image_meta::size(&bytes), Some((4000, 3000)));
}

/// Unrecognised returns None — better for the caller to say "the header gives no dimensions"
/// than to invent a number.
#[test]
fn unknown_or_truncated_headers_answer_none() {
    assert_eq!(image_meta::size(b""), None);
    assert_eq!(image_meta::size(b"not an image at all"), None);
    // The PNG signature alone, without the width and height in IHDR.
    assert_eq!(
        image_meta::size(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        None
    );
    // A JPEG that reaches the scan header without a SOF: no guessing.
    let mut scan = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08];
    scan.extend_from_slice(&[0u8; 8]);
    assert_eq!(image_meta::size(&scan), None);
}

/// The limit is conservative on purpose: lower and ordinary screenshots get hurt, higher and
/// images the far end will surely refuse get through.
#[test]
fn the_edge_ceiling_is_the_documented_one() {
    assert_eq!(image_meta::MAX_EDGE, 4096);
}
