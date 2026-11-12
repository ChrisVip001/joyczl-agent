//! `apply_patch`: a **patch**, not a file.
//!
//! `edit` replaces one exact string; `write_file` rewrites a whole file. Neither can express
//! "three hunks in two files" without either a 1:1 call each or shipping untouched bytes back.
//! This tool takes the `*** Begin Patch` text (the format codex uses, so models produce it
//! unprompted) and applies it **all or nothing**: every hunk is resolved in memory first, and a
//! single hunk that does not fit means nothing is written — a half-applied patch is worse than a
//! rejected one, because it looks like it worked.
//!
//! Parsing and application are pure functions ([`parse`], [`apply`]) so the rules are testable
//! without a filesystem or a sandbox; the tool wrapper only adds paths, the write gate and the
//! diff.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::fs_write::{
    check_content, pass_write_gate, permission_hook, read_text, render_change, WritePolicy,
};
use crate::{require_str, Tool, ToolCtx};

/// The largest patch accepted. A patch is instructions, not a payload: one this big is a
/// `write_file` in disguise, and paying twice for it (parse + apply) helps nobody.
const MAX_PATCH_BYTES: usize = 512 * 1024;

/// One file's worth of instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Replace hunks in an existing file.
    Update { path: String, hunks: Vec<Hunk> },
    /// Create a file (it must **not** exist — "add" that silently overwrites is a trap).
    Add { path: String, lines: Vec<String> },
    /// Remove a file.
    Delete { path: String },
}

/// One hunk: the lines as the patch spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The text after `@@`, if any. Kept for the error message ("which hunk failed") — matching
    /// is by line sequence, so a wrong hint can never move a hunk somewhere it does not belong.
    pub hint: String,
    pub lines: Vec<PatchLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Parse the `*** Begin Patch` format.
///
/// Unknown section headers are refused rather than ignored: silently dropping a `*** Move to:`
/// would apply the rest of the patch and quietly not move anything.
pub fn parse(text: &str) -> Result<Vec<Op>, String> {
    let mut ops: Vec<Op> = Vec::new();
    let mut lines = text.lines().peekable();
    let mut started = false;

    // The model sometimes wraps the patch in prose or a fence; accept anything before `Begin`.
    for line in lines.by_ref() {
        if line.trim() == "*** Begin Patch" {
            started = true;
            break;
        }
    }
    if !started {
        return Err(
            "找不到 `*** Begin Patch` —— 补丁必须从这一行开始（前后可以有别的文字）。".into(),
        );
    }

    // What the lines collected so far belong to.
    enum Section {
        Update { path: String, hunks: Vec<Hunk> },
        Add { path: String, lines: Vec<String> },
        Delete { path: String },
    }
    let mut section: Option<Section> = None;
    let mut ended = false;

    fn flush(section: Option<Section>, ops: &mut Vec<Op>) -> Result<(), String> {
        match section {
            None => Ok(()),
            Some(Section::Update { path, hunks }) => {
                if hunks.is_empty() {
                    return Err(format!("`*** Update File: {path}` 后面没有 hunk。"));
                }
                ops.push(Op::Update { path, hunks });
                Ok(())
            }
            Some(Section::Add { path, lines }) => {
                ops.push(Op::Add { path, lines });
                Ok(())
            }
            Some(Section::Delete { path }) => {
                ops.push(Op::Delete { path });
                Ok(())
            }
        }
    }

    for line in lines {
        if line.trim() == "*** End Patch" {
            ended = true;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Update File:") {
            flush(section.take(), &mut ops)?;
            section = Some(Section::Update {
                path: path.trim().to_string(),
                hunks: Vec::new(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File:") {
            flush(section.take(), &mut ops)?;
            section = Some(Section::Add {
                path: path.trim().to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File:") {
            flush(section.take(), &mut ops)?;
            section = Some(Section::Delete {
                path: path.trim().to_string(),
            });
            continue;
        }
        if line.starts_with("*** ") {
            return Err(format!(
                "不认识的指令：`{line}`。只支持 Update / Add / Delete File（Move 没实现，\
                 宁可拒绝也不装作移动了）。"
            ));
        }

        match section.as_mut() {
            None => {
                return Err(format!(
                    "这行出现在任何文件段之前：`{line}`。它得落在 `*** Update File:` 之类的\
                     标题下面。"
                ))
            }
            Some(Section::Add { lines, .. }) => {
                let Some(content) = line.strip_prefix('+') else {
                    return Err(format!(
                        "`*** Add File:` 段里每一行都要以 `+` 开头，这行不是：`{line}`。"
                    ));
                };
                lines.push(content.to_string());
            }
            Some(Section::Delete { .. }) => {
                if !line.trim().is_empty() {
                    return Err(format!("`*** Delete File:` 段里不该有内容：`{line}`。"));
                }
            }
            Some(Section::Update { hunks, .. }) => {
                if let Some(hint) = line.strip_prefix("@@") {
                    hunks.push(Hunk {
                        // `@@ fn main @@` and `@@ -1,3 +1,4 @@` both appear in the wild; the hint
                        // is only ever shown in an error message, so keeping it clean is enough.
                        hint: hint.trim().trim_end_matches("@@").trim().to_string(),
                        lines: Vec::new(),
                    });
                    continue;
                }
                // `\ No newline at end of file` and similar annotations carry no text.
                if line.starts_with('\\') {
                    continue;
                }
                let parsed = if let Some(text) = line.strip_prefix('+') {
                    PatchLine::Add(text.to_string())
                } else if let Some(text) = line.strip_prefix('-') {
                    PatchLine::Remove(text.to_string())
                } else if let Some(text) = line.strip_prefix(' ') {
                    PatchLine::Context(text.to_string())
                } else if line.is_empty() {
                    // An empty line is a context line whose content is empty; models write it
                    // this way often enough that refusing it would be pedantry.
                    PatchLine::Context(String::new())
                } else {
                    return Err(format!(
                        "hunk 里的每一行要以 ` `/`-`/`+` 开头，这行不是：`{line}`。"
                    ));
                };
                let Some(hunk) = hunks.last_mut() else {
                    return Err(format!(
                        "内容出现在第一个 `@@` 之前：`{line}`。每个 hunk 要用 `@@` 开头。"
                    ));
                };
                hunk.lines.push(parsed);
            }
        }
    }

    if !ended {
        return Err("补丁没有 `*** End Patch` 结尾 —— 半截补丁不敢套用。".into());
    }
    flush(section.take(), &mut ops)?;
    if ops.is_empty() {
        return Err("补丁里没有任何文件段。".into());
    }
    Ok(ops)
}

/// Apply every hunk of one file, in order, all or nothing.
///
/// Matching is by **line sequence** from the previous hunk's end, so two identical hunks apply at
/// two different places instead of both hitting the first one. A hunk that does not fit refuses
/// the whole file; a hunk that fits in more than one place takes the first one at or after the
/// cursor (that is what the surrounding context is for, and the diff we hand back shows where it
/// landed).
pub fn apply(current: &str, hunks: &[Hunk]) -> Result<String, String> {
    let trailing_newline = current.ends_with('\n');
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
    let mut cursor = 0usize;

    for (index, hunk) in hunks.iter().enumerate() {
        let before: Vec<&String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(text) | PatchLine::Remove(text) => Some(text),
                PatchLine::Add(_) => None,
            })
            .collect();
        let after: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(text) | PatchLine::Add(text) => Some(text.clone()),
                PatchLine::Remove(_) => None,
            })
            .collect();
        if before.is_empty() {
            return Err(format!(
                "第 {} 个 hunk（@@ {}）只有新增行、没有任何上下文 —— 不知道往哪儿加。\
                 至少留一行不改的内容作为锚点。",
                index + 1,
                hunk.hint
            ));
        }

        let found = if before.len() > lines.len() {
            None
        } else {
            (cursor..=lines.len() - before.len()).find(|start| {
                lines[*start..*start + before.len()]
                    .iter()
                    .zip(&before)
                    .all(|(actual, expected)| actual == *expected)
            })
        };
        let Some(start) = found else {
            return Err(format!(
                "第 {} 个 hunk（@@ {}）在文件里找不到对应的原文。补丁里的上下文行必须与文件\
                 逐字符一致 —— 先 read_file 看一眼，或者少写几行上下文。",
                index + 1,
                hunk.hint
            ));
        };
        lines.splice(start..start + before.len(), after.iter().cloned());
        cursor = start + after.len();
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The `apply_patch` tool. Registered next to `write_file` / `edit`, only when `JOY_WRITE=1`.
pub fn apply_patch(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "apply_patch".to_string(),
        description: "按补丁改文件：一次可以改多个文件、多处，全中才落盘（有一处对不上就一处\
                      也不改）。格式：`*** Begin Patch` / `*** Update File: 路径` / `@@` / \
                      以空格、-、+ 开头的行 / `*** Add File: 路径`（每行 + 开头）/ \
                      `*** Delete File: 路径` / `*** End Patch`。只改文件的一小段用 edit，\
                      整文件重写用 write_file。可写范围与 edit 相同。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "补丁全文（上限 512KB）。上下文行要与文件逐字符一致。"}
            },
            "required": ["patch"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let text = require_str(&args, "patch")?;
                if text.len() > MAX_PATCH_BYTES {
                    return Err(anyhow!(
                        "补丁有 {} 字节，超过上限 {MAX_PATCH_BYTES} —— 这么大的补丁不如直接用 \
                         write_file，或者拆成几次。",
                        text.len()
                    ));
                }
                check_content(&text, "patch")?;
                let ops = match parse(&text) {
                    Ok(ops) => ops,
                    Err(why) => return Ok(format!("Error: 补丁没解析出来：{why}")),
                };

                // Everything is resolved in memory first: a rejected hunk must leave no trace.
                let mut planned: Vec<(PathBuf, Option<String>, Option<String>)> = Vec::new();
                for op in &ops {
                    match op {
                        Op::Update { path, hunks } => {
                            let resolved = resolve(&ctx, path);
                            let old = if resolved.is_file() {
                                read_text(&resolved)?
                            } else {
                                return Err(anyhow!(
                                    "找不到文件：{path}（Update File 只改已存在的文件；新建用 Add File）"
                                ));
                            };
                            let new = match apply(&old, hunks) {
                                Ok(new) => new,
                                Err(why) => return Ok(format!("Error: {path}：{why}")),
                            };
                            check_content(&new, "补丁结果")?;
                            planned.push((resolved, Some(old), Some(new)));
                        }
                        Op::Add { path, lines } => {
                            let resolved = resolve(&ctx, path);
                            if resolved.exists() {
                                return Err(anyhow!(
                                    "{path} 已经存在 —— Add File 只用来建新文件（要改它用 Update File）"
                                ));
                            }
                            let mut new = lines.join("\n");
                            if !new.is_empty() {
                                new.push('\n');
                            }
                            check_content(&new, "补丁结果")?;
                            planned.push((resolved, None, Some(new)));
                        }
                        Op::Delete { path } => {
                            let resolved = resolve(&ctx, path);
                            if !resolved.is_file() {
                                return Err(anyhow!("找不到要删除的文件：{path}"));
                            }
                            planned.push((resolved, None, None));
                        }
                    }
                }

                let (approval, blocked) = permission_hook(
                    &ctx,
                    "apply_patch",
                    json!({ "patch": text, "files": planned.iter().map(|(p, _, _)| p.display().to_string()).collect::<Vec<_>>() }),
                )
                .await;
                if let Some(why) = blocked {
                    return Ok(format!(
                        "Error: {why}（PermissionRequest hook 拒绝了这次补丁）"
                    ));
                }
                let roots = crate::roots::workspace_roots(&ctx, &policy.extra_roots).workspaces;
                // Gate every path **before** writing any of them: "all or nothing" has to include
                // the human's answer, not just the parsing.
                for (path, _, _) in &planned {
                    if let Err(why) = pass_write_gate(
                        "apply_patch",
                        path,
                        &format!("{} 个文件", planned.len()),
                        &policy,
                        &roots,
                        approval.clone(),
                    )
                    .await
                    {
                        return Ok(format!("Error: {why}"));
                    }
                }

                let mut report = Vec::new();
                for (path, old, new) in &planned {
                    match (old, new) {
                        (Some(old), Some(new)) if old == new => {
                            report.push(format!("{}：内容未变，没有写盘", path.display()));
                        }
                        (_, Some(new)) => {
                            if let Some(parent) = path.parent() {
                                if !parent.is_dir() {
                                    std::fs::create_dir_all(parent)
                                        .map_err(|e| anyhow!("创建父目录 {parent:?} 失败：{e}"))?;
                                }
                            }
                            std::fs::write(path, new)
                                .map_err(|e| anyhow!("写 {path:?} 失败：{e}"))?;
                            report.push(format!(
                                "已修改 {}：{} 行{}",
                                path.display(),
                                new.lines().count().max(1),
                                match old {
                                    Some(old) =>
                                        format!("（原 {} 行）", old.lines().count().max(1)),
                                    None => "（新建）".to_string(),
                                }
                            ));
                            report.push(render_change(old.as_deref(), new));
                        }
                        (_, None) => {
                            std::fs::remove_file(path)
                                .map_err(|e| anyhow!("删除 {path:?} 失败：{e}"))?;
                            report.push(format!("已删除 {}", path.display()));
                        }
                    }
                }
                Ok(report.join("\n"))
            })
        }),
    }
}

fn resolve(ctx: &ToolCtx, raw: &str) -> PathBuf {
    crate::fs::resolve_against_cwd(ctx, crate::fs::expand_tilde(raw))
}
