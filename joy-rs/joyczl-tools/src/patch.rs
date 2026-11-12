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
            "No `*** Begin Patch` found — a patch must start at that line (there may be other text around it).".into(),
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
                    return Err(format!("`*** Update File: {path}` has no hunk after it."));
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
                "Unknown directive: `{line}`. Only Update / Add / Delete File are supported (Move is not implemented — better to refuse than to pretend it moved)."
            ));
        }

        match section.as_mut() {
            None => {
                return Err(format!(
                    "This line appears before any file section: `{line}`. It has to sit under a heading like `*** Update File:`."
                ))
            }
            Some(Section::Add { lines, .. }) => {
                let Some(content) = line.strip_prefix('+') else {
                    return Err(format!(
                        "Every line in a `*** Add File:` section must start with `+`; this one does not: `{line}`."
                    ));
                };
                lines.push(content.to_string());
            }
            Some(Section::Delete { .. }) => {
                if !line.trim().is_empty() {
                    return Err(format!("A `*** Delete File:` section must not contain content: `{line}`."));
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
                        "Every line in a hunk must start with ` `, `-` or `+`; this one does not: `{line}`."
                    ));
                };
                let Some(hunk) = hunks.last_mut() else {
                    return Err(format!(
                        "Content appears before the first `@@`: `{line}`. Every hunk starts with `@@`."
                    ));
                };
                hunk.lines.push(parsed);
            }
        }
    }

    if !ended {
        return Err("The patch has no `*** End Patch` — refusing to apply half a patch.".into());
    }
    flush(section.take(), &mut ops)?;
    if ops.is_empty() {
        return Err("The patch has no file section at all.".into());
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
                "Hunk {} (@@ {}) has only added lines and no context — there is no telling where it goes. Keep at least one unchanged line as an anchor.",
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
                "Hunk {} (@@ {}) matches nothing in the file. The context lines in a patch must match the file character for character — read_file first, or write fewer context lines.",
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
        description: "Change files by patch: several files and several places at once, and it lands only if every hunk matches (one mismatch and nothing changes). Format: `*** Begin Patch` / `*** Update File: path` / `@@` / lines starting with space, - or + / `*** Add File: path` (every line starting with +) / `*** Delete File: path` / `*** End Patch`. To change a small stretch use edit; to rewrite a whole file use write_file. The writable scope is the same as edit's."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "The whole patch (512KB at most). Context lines must match the file character for character."}
            },
            "required": ["patch"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let text = require_str(&args, "patch")?;
                if text.len() > MAX_PATCH_BYTES {
                    return Err(anyhow!(
                        "The patch is {} bytes, over the {MAX_PATCH_BYTES} limit — a patch that size is better done with write_file, or split into several.",
                        text.len()
                    ));
                }
                check_content(&text, "patch")?;
                let ops = match parse(&text) {
                    Ok(ops) => ops,
                    Err(why) => return Ok(format!("Error: the patch could not be parsed: {why}")),
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
                                    "File not found: {path} (Update File only changes files that exist; create one with Add File)"
                                ));
                            };
                            let new = match apply(&old, hunks) {
                                Ok(new) => new,
                                Err(why) => return Ok(format!("Error: {path}：{why}")),
                            };
                            check_content(&new, "patch result")?;
                            planned.push((resolved, Some(old), Some(new)));
                        }
                        Op::Add { path, lines } => {
                            let resolved = resolve(&ctx, path);
                            if resolved.exists() {
                                return Err(anyhow!(
                                    "{path} already exists — Add File only creates new files (use Update File to change it)"
                                ));
                            }
                            let mut new = lines.join("\n");
                            if !new.is_empty() {
                                new.push('\n');
                            }
                            check_content(&new, "patch result")?;
                            planned.push((resolved, None, Some(new)));
                        }
                        Op::Delete { path } => {
                            let resolved = resolve(&ctx, path);
                            if !resolved.is_file() {
                                return Err(anyhow!("No such file to delete: {path}"));
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
                        "Error: {why} (a PermissionRequest hook refused this patch)"
                    ));
                }
                let roots = crate::roots::workspace_roots(&ctx, &policy.extra_roots).workspaces;
                // Gate every path **before** writing any of them: "all or nothing" has to include
                // the human's answer, not just the parsing.
                for (path, _, _) in &planned {
                    if let Err(why) = pass_write_gate(
                        "apply_patch",
                        path,
                        &format!("{} files", planned.len()),
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
                            report.push(format!("{}: content unchanged, nothing written", path.display()));
                        }
                        (_, Some(new)) => {
                            if let Some(parent) = path.parent() {
                                if !parent.is_dir() {
                                    std::fs::create_dir_all(parent)
                                        .map_err(|e| anyhow!("Could not create the parent directory {parent:?}: {e}"))?;
                                }
                            }
                            std::fs::write(path, new)
                                .map_err(|e| anyhow!("Could not write {path:?}: {e}"))?;
                            report.push(format!(
                                "Changed {}: {} lines{}",
                                path.display(),
                                new.lines().count().max(1),
                                match old {
                                    Some(old) =>
                                        format!("(was {} lines)", old.lines().count().max(1)),
                                    None => "(new)".to_string(),
                                }
                            ));
                            report.push(render_change(old.as_deref(), new));
                        }
                        (_, None) => {
                            std::fs::remove_file(path)
                                .map_err(|e| anyhow!("Could not delete {path:?}: {e}"))?;
                            report.push(format!("Deleted {}", path.display()));
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
