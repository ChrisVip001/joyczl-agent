//! 只读的文件工具：`read_file`（分页读文本）与 `list_dir`（列目录）。
//!
//! 在这两个工具出现之前，Joy 的模型读文件只有一条路：`JOY_EXEC=1` 之后再
//! `run_command cat` —— 把「看一眼文件」绑在「执行命令」的闸门上。读取没有
//! 副作用（codex 例外是因为它的沙箱永远在；这里照 dsh/nanobot/pi 的共识：
//! 只读零摩擦，批准只留给写和执行），所以这两个工具**默认注册、不走闸门**。
//!
//! 两条设计都抄的是实测过的最优实现：
//!
//! * **分页续读协议**（pi）：双限 —— 2000 行、50KB，在**行边界**上截，绝不
//!   切半行；截断时附 `[Use offset=N to continue.]`，模型能自主翻页而不是
//!   猜测内容被截掉的部分。
//! * **内容不变短路**（nanobot）：同一会话里重读同一文件的同一段，返回
//!   「没变」占位符。一轮里反复读同一个文件是模型最常见的浪费，这个占位符
//!   把第二次起的读取成本降到接近零。哈希在内存里、只做缓存用，不落盘。
//!
//! 错误路径（都按「工具错误是 `Error:` 文本」的契约走）：
//!
//! * 文件不存在 / 不是文件 → 如实说；
//! * 非 UTF-8 或含 NUL → 明确说「二进制」，并指出要看原始字节该用
//!   `run_command`（那扇门有它自己的闸门）；
//! * 读一半 IO 出错 → 报错，绝不把半截内容当好结果。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::Tool;

/// 单次最多读多少行。pi 与 deepseek-harness 同一口径：2000。
pub const READ_MAX_LINES: u32 = 2_000;
/// 单次最多返回多少字节。在**行边界**上截 —— 切半行会让模型引用不存在的行号。
pub const READ_MAX_BYTES: usize = 50_000;
/// 目录列表最多多少项。目录树不是给模型散步用的，500 项足够定位。
pub const LIST_MAX_ENTRIES: usize = 500;

/// `~` 展开：模型经常写 `~/docs/x.md`，不展开就是一条误导性的「找不到」。
/// 只认 `HOME` / `USERPROFILE`，不引依赖。
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    if path == "~" {
        if let Some(home) = home() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// 流式扫一遍文件：跳到 `offset`、收集到 `limit`/字节上限，同时数出总行数。
/// 一次遍历完成收集与计数 —— 大文件也不必整读进内存。
fn scan_lines(path: PathBuf, offset: u32, limit: u32) -> Result<(Vec<String>, u64, u64, bool)> {
    let file = std::fs::File::open(&path)?;
    let reader = std::io::BufReader::new(file);
    let mut rendered: Vec<String> = Vec::new();
    let mut bytes_used = 0usize;
    let mut byte_capped = false;
    let mut total = 0u64;
    let mut last_shown = 0u64;

    for (index, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(anyhow!(
                    "{:?} 不是 UTF-8 文本（可能是二进制文件），read_file 只读文本；\
                     要看原始字节用 run_command（需 JOY_EXEC=1）。",
                    path
                ));
            }
            Err(e) => return Err(anyhow!("读 {path:?} 出错：{e}")),
        };
        total += 1;
        let n = (index + 1) as u64;
        if n < offset as u64 {
            continue;
        }
        if n - offset as u64 >= limit as u64 {
            continue; // 行数够了，但继续数完总行数（页脚要「of Z」）
        }
        if byte_capped {
            continue;
        }
        if line.contains('\0') {
            return Err(anyhow!(
                "{:?} 看起来是二进制文件（含 NUL 字节），read_file 只读文本。",
                path
            ));
        }
        let rendered_line = format!("{n:>6}\t{line}");
        let needed = rendered_line.len() + 1;
        if bytes_used + needed > READ_MAX_BYTES && !rendered.is_empty() {
            // 到字节上限：停在**这一行之前**（行边界），后面继续数总行数。
            byte_capped = true;
            continue;
        }
        bytes_used += needed;
        last_shown = n;
        rendered.push(rendered_line);
    }
    Ok((rendered, total, last_shown, byte_capped))
}

pub fn read_file() -> Tool {
    // 会话级「读过什么」：(session, 路径, offset, limit) -> 内容摘要。
    // 只活在进程内：重启后自然清空，第一次重读照常给全文。
    let cache: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
    Tool {
        name: "read_file".to_string(),
        description: "读一个文本文件，带行号、按页返回（默认 2000 行 / 50KB，\
                      绝不切半行）。返回里带 [Use offset=N to continue.] 时，\
                      用那个 offset 继续读。同一会话里重复读同一段会得到\
                      「没变」占位符 —— 内容你已经有了，别再读。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "文件路径。支持 ~ 展开；相对路径相对 Joy 进程的工作目录。"
                },
                "offset": {
                    "type": "integer",
                    "description": "从第几行开始（1 起，默认 1）。续读用上一页页脚给的值。"
                },
                "limit": {
                    "type": "integer",
                    "description": "最多读多少行（默认与上限 2000）。"
                }
            },
            "required": ["path"]
        }),
        handler: Arc::new(move |ctx, args| {
            let cache = cache.clone();
            Box::pin(async move { execute_read(&ctx, &args, &cache).await })
        }),
    }
}

async fn execute_read(
    ctx: &crate::ToolCtx,
    args: &Value,
    cache: &Mutex<HashMap<String, u64>>,
) -> Result<String> {
    let raw = crate::require_str(args, "path")?;
    let path = expand_tilde(&raw);
    let offset = crate::opt_u32(args, "offset", 1)?.max(1);
    let limit = crate::opt_u32(args, "limit", READ_MAX_LINES)?.clamp(1, READ_MAX_LINES);

    if !path.is_file() {
        // 目录也挡在这里：给一句「想列目录用哪个工具」的实话。
        if path.is_dir() {
            return Err(anyhow!("{raw} 是目录，不是文件 —— 列目录用 list_dir。"));
        }
        return Err(anyhow!(
            "找不到文件：{raw}（相对路径相对 Joy 启动时所在目录）"
        ));
    }

    let scan_path = path.clone();
    let (rendered, total, last_shown, byte_capped) =
        tokio::task::spawn_blocking(move || scan_lines(scan_path, offset, limit))
            .await
            .map_err(|e| anyhow!("读取任务失败：{e}"))??;

    let from = offset as u64;
    let mut out = String::new();
    if total == 0 {
        return Ok("（空文件，0 行）".to_string());
    }
    if rendered.is_empty() {
        out.push_str(&format!(
            "[第 {from} 行起没有内容：文件共 {total} 行。offset 超出范围了。]"
        ));
    } else {
        out.push_str(&rendered.join("\n"));
        out.push_str(&format!(
            "\n\n[Showing lines {from}–{last_shown} of {total}]"
        ));
        if last_shown < total {
            out.push_str(&format!("\n[Use offset={} to continue.]", last_shown + 1));
        }
        if byte_capped {
            out.push_str(&format!(
                "\n[Output capped at {READ_MAX_BYTES} bytes at a line boundary.]"
            ));
        }
    }

    // 内容不变短路：同会话、同路径、同范围，读出同一段 → 占位符。
    let mut hasher = DefaultHasher::new();
    out.hash(&mut hasher);
    let digest = hasher.finish();
    let key = format!("{}|{}|{offset}|{limit}", ctx.session_id, path.display());
    let mut map = cache.lock().expect("read_file 缓存锁不该中毒");
    if map.get(&key) == Some(&digest) {
        return Ok(format!(
            "[File unchanged since last read: lines {from}–{last_shown} of {total} \
             are identical. Content omitted — use what you already have.]"
        ));
    }
    map.insert(key, digest);
    drop(map);
    Ok(out)
}

pub fn list_dir() -> Tool {
    Tool {
        name: "list_dir".to_string(),
        description: "列一个目录的内容：目录在前（带 / 后缀）、文件在后（带字节大小），\
                      最多 500 项。找文件、确认路径拼没拼对，先用它。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "目录路径（默认当前目录）。支持 ~ 展开。"
                }
            }
        }),
        handler: Arc::new(|_ctx, args| Box::pin(async move { execute_list(&args).await })),
    }
}

async fn execute_list(args: &Value) -> Result<String> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let path = expand_tilde(raw);
    if !path.is_dir() {
        return Err(anyhow!(
            "{raw} 不是目录（不存在，或者是个文件 —— 文件用 read_file 读）。"
        ));
    }

    let mut reader = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| anyhow!("列不了 {raw}：{e}"))?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| anyhow!("列 {raw} 时出错：{e}"))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        // 不跟随符号链接判类型（坏链不该让整个列表失败），大小取得到就要。
        let mut is_dir = entry.file_type().await?.is_dir();
        let mut size = 0u64;
        if let Ok(meta) = entry.metadata().await {
            is_dir = is_dir || meta.is_dir();
            size = meta.len();
        }
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(format!("{name}  ({size} B)"));
        }
    }

    dirs.sort();
    files.sort();
    let mut lines = dirs;
    lines.extend(files);
    if lines.is_empty() {
        return Ok(format!("{raw}（空目录）"));
    }
    let total = lines.len();
    let mut note = String::new();
    if total > LIST_MAX_ENTRIES {
        lines.truncate(LIST_MAX_ENTRIES);
        note = format!(
            "\n[+{} more not shown — narrow the path.]",
            total - LIST_MAX_ENTRIES
        );
    }
    Ok(format!("{}\n\n[{total} entries]", lines.join("\n")) + &note)
}
