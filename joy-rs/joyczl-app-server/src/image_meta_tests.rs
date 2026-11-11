//! `image_meta` 的头部解析：四种格式各给一个真头，外加三种「认不出来」。

use super::image_meta;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    out.extend_from_slice(&13u32.to_be_bytes()); // IHDR 数据长 13
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0]); // 位深 / 颜色类型 / 压缩 / 过滤 / 隔行
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
    out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0：段长 16
    out.extend_from_slice(&[0u8; 14]);
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]); // SOF0：段长 17，精度 8
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
    out.extend_from_slice(&[0, 0, 0]); // 帧标签
    out.extend_from_slice(&[0x9D, 0x01, 0x2A]); // 起始码
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
    out.push(0x2F); // 签名
    let bits = ((width - 1) & 0x3FFF) | (((height - 1) & 0x3FFF) << 14);
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(&[0u8; 5]); // 真实 VP8L 块更长，补齐到 30 字节
    out
}

/// 用户报的那张图：1120×11100，只有 2.4 MB —— 旧的字节闸门放行，远端才拒。
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

/// JPEG 的尺寸不在固定偏移：要先跳过 APP0 之类的段，才落到 SOF。
#[test]
fn jpeg_walks_past_other_segments_to_the_sof() {
    let bytes = jpeg(4000, 3000);
    assert_eq!(image_meta::size(&bytes), Some((4000, 3000)));
}

/// 认不出来就返回 None —— 宁可让上层说「文件头读不出宽高」，也不猜一个数字。
#[test]
fn unknown_or_truncated_headers_answer_none() {
    assert_eq!(image_meta::size(b""), None);
    assert_eq!(image_meta::size(b"not an image at all"), None);
    // 只有 PNG 签名，没有 IHDR 里的宽高。
    assert_eq!(
        image_meta::size(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        None
    );
    // JPEG 走到扫描头都没遇到 SOF：不猜。
    let mut scan = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08];
    scan.extend_from_slice(&[0u8; 8]);
    assert_eq!(image_meta::size(&scan), None);
}

/// 上限是保守值，改小会误伤正常截图，改大会放行必然被远端拒的图。
#[test]
fn the_edge_ceiling_is_the_documented_one() {
    assert_eq!(image_meta::MAX_EDGE, 4096);
}
