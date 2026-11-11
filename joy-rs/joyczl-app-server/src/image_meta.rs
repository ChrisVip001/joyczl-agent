//! 图片头解析：只读文件头拿宽高，零依赖。
//!
//! 为什么需要它：视觉 provider 普遍拒超大图（长截图最典型 —— 1120×11100 这种），
//! 而远端只回一句 "unsupported image. Please make sure your image is valid and has
//! one of the following formats: webp, png, jpeg, gif"：既不说真实原因，也不说上限。
//! 本地先量一次宽高，报错才能给出真实数字和可执行的下一步。
//!
//! 只覆盖 `media_type_for_extension` 放行的四种格式；认不出来就返回 `None`，不猜。

/// 单边像素上限。
///
/// 4096 是各家的保守下限：再大的图会直接被远端拒（Anthropic 允许 8000，OpenAI 建议
/// 短边 ≤2048，DeepSeek 这类 OpenAI 兼容端点更严）。**不是**精确上限 —— 换一家仍可能
/// 更严，所以报错文案说的是「本地保守上限」，不替远端下结论。
pub const MAX_EDGE: u32 = 4096;

/// 宽高（像素）。`None` = 文件头不认识：已损坏，或者不是这四种格式。
pub fn size(bytes: &[u8]) -> Option<(u32, u32)> {
    png(bytes)
        .or_else(|| gif(bytes))
        .or_else(|| jpeg(bytes))
        .or_else(|| webp(bytes))
}

/// PNG：签名之后紧跟 IHDR，宽高是 16..20 与 20..24 的大端 u32。
fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !bytes.starts_with(&SIGNATURE) {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// GIF：逻辑屏描述符里的宽高，小端 u16。
fn gif(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"GIF8") {
        return None;
    }
    let width = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
    let height = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
    Some((u32::from(width), u32::from(height)))
}

/// JPEG 没有固定头：一段段跳到第一个 SOF（FFC0–FFCF，去掉 C4/C8/CC 这三个非 SOF 段）。
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
        // 扫描头（FFDA）之后就是熵编码数据，再往后找不到尺寸了。
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

/// WebP 三种变体：扩展格式（VP8X）直接给画布尺寸，有损/无损把尺寸藏在帧头里。
fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || !bytes.starts_with(b"RIFF") || bytes[8..12] != b"WEBP"[..] {
        return None;
    }
    let kind = bytes.get(12..16)?;
    if kind == &b"VP8X"[..] {
        // 24 位小端的「宽-1 / 高-1」：flags(1) + reserved(3) 之后才是画布。
        let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
        let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
        return Some((width, height));
    }
    if kind == &b"VP8 "[..] {
        // 3 字节帧标签 + 3 字节起始码（9D 01 2A）之后，是 14 位宽高。
        let width = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF);
        let height = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF);
        return Some((width, height));
    }
    if kind == &b"VP8L"[..] {
        // 0x2F 签名之后，14 位「宽-1」接 14 位「高-1」，小端位流。
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
    }
    None
}
