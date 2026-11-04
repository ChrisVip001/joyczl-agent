//! Read-only file tools: `read_file` (paged text reads) and `list_dir` (directory listing).
//!
//! Before these two existed, the model had one way to read a file: flip `JOY_EXEC=1`, then
//! `run_command cat` — binding "peek at a file" to the "run a command" gate. Reads have no
//! side effects (consensus: read-only is zero-friction; approval is for writes and execution),
//! so these two tools are **registered by default and skip the gate**.
//!
//! Two designs:
//!
//! * **Paged-resume protocol**: dual caps — 2000 lines, 50KB, cut on **line boundaries**,
//!   never mid-line; truncation appends `[Use offset=N to continue.]` so the model can
//!   page by itself instead of guessing at the cut part.
//! * **Unchanged-content short circuit**: re-reading the same slice of the same file in the
//!   same session returns an "unchanged" placeholder — repeat reads of one file are the
//!   model's most common waste; this drops the cost to near zero. Hash in memory, cache only.
//!
//! Error paths (all follow the "tool errors are `Error:` text" contract):
//!
//! * missing / not a file → say so honestly;
//! * non-UTF-8 or containing NUL → call it "binary" and point out that raw bytes need
//!   `run_command` (that door has its own gate);
//! * mid-read IO error → error out; never present half the content as a good result.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::Tool;

/// Max lines per read: 2000.
pub const READ_MAX_LINES: u32 = 2_000;
/// Max bytes per response. Cut on a **line boundary** — half a line would make the model cite wrong line numbers.
pub const READ_MAX_BYTES: usize = 50_000;
/// Max entries in a directory listing. The tree isn't for the model to stroll; 500 is enough.
pub const LIST_MAX_ENTRIES: usize = 500;

/// `~` expansion: models often write `~/docs/x.md`; unexpanded it's a misleading "not found".
/// Only `HOME` / `USERPROFILE` are honored; no dependencies.
/// The base for resolving relative paths (session-level working-directory semantics):
/// this turn's working directory first; if unset, keep the status quo (process cwd). Absolute paths pass through.
pub(crate) fn resolve_against_cwd(ctx: &crate::ToolCtx, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match &ctx.cwd {
        Some(base) => base.join(path),
        None => path,
    }
}

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

/// Stream-scan a file: jump to `offset`, collect up to `limit`/the byte cap, count total lines.
/// One pass does both — even large files never load fully into memory.
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
            continue; // enough lines, but keep counting total (the footer needs "of Z")
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
            // At the byte cap: stop **before this line** (line boundary), keep counting total lines.
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
    // Session-level "what was read": (session, path, offset, limit) -> content digest.
    // Lives only in-process: cleared on restart, so the first re-read gets the full text again.
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
    let path = resolve_against_cwd(ctx, expand_tilde(&raw));
    let offset = crate::opt_u32(args, "offset", 1)?.max(1);
    let limit = crate::opt_u32(args, "limit", READ_MAX_LINES)?.clamp(1, READ_MAX_LINES);

    if !path.is_file() {
        // Directories are blocked here too: with an honest "use that tool to list directories".
        if path.is_dir() {
            return Err(anyhow!("{raw} 是目录，不是文件 —— 列目录用 list_dir。"));
        }
        return Err(anyhow!("找不到文件：{raw}（相对路径相对本轮工作目录）"));
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

    // Unchanged-content short circuit: same session, path, range, same content → placeholder.
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
        handler: Arc::new(move |ctx, args| {
            Box::pin(async move { execute_list(&ctx, &args).await })
        }),
    }
}

async fn execute_list(ctx: &crate::ToolCtx, args: &Value) -> Result<String> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let path = resolve_against_cwd(ctx, expand_tilde(raw));
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
        // Don't follow symlinks to decide type (a bad link must not fail the listing); take the size when available.
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
