//! Write tools: `write_file` (whole-file write) and `edit` (one exact-match replacement).
//!
//! **Off by default** (`JOY_WRITE=1` registers them; off, the model can't even see the names).
//! The trust boundary is `crate::roots` — the chosen workspace plus
//! `JOY_EXEC_WRITABLE_ROOTS`, **the same definition the exec sandbox uses**. There is no
//! process sandbox here (the write happens in-process), so the containment check is the hard
//! gate: outside the roots is refused before approval is ever consulted — approval only ever
//! unlocks a write that is already inside the boundary, never one outside it.
//!
//! The Joy home is deliberately **not** a root: an assistant with a workspace must not wander
//! into its own control plane (`settings.json`, `hooks.json`, memory, skills).
//!
//! Like every tool: **refusals and failures are `Error: …` text, never Err** — the loop keeps going.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::approval::{ApprovalBroker, RememberTarget};
use crate::{require_str, Tool, ToolCtx};

/// Raw string argument — **no trimming**. `require_str` trims, which is right for
/// commands and paths but would silently corrupt file content: a trailing newline or
/// leading spaces are part of the file, and a write tool must not "tidy" its data.
fn raw_str(args: &Value, key: &str) -> Result<String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        None | Some(Value::Null) => Err(anyhow!("Missing parameter '{key}'")),
        Some(v) => Err(anyhow!("Parameter '{key}' should be a string, got {v}")),
    }
}

/// One write may carry at most 1 MiB. A note fits easily; a dataset should go through
/// `run_command` (where output spills) or a proper channel — a context window is not a disk.
pub const MAX_WRITE_BYTES: usize = 1_000_000;

/// How many context lines a change hunk shows around the edited region.
const DIFF_CONTEXT: usize = 3;
/// Cap on rendered hunk lines before it is truncated (with an honest note).
const DIFF_MAX_LINES: usize = 40;

/// The write policy fixed at startup (from `JOY_WRITE` / `JOY_APPROVAL`).
#[derive(Debug, Clone, Default)]
pub struct WritePolicy {
    /// `JOY_APPROVAL == "on-request"`: **every** write asks once before it lands.
    /// Writes are rare and consequential, and there is no allow table to match, so
    /// on-request means "ask each time". Default (`never`) = inside the roots, no asking.
    pub approval: bool,
    /// Seconds to wait for a human answer (`JOY_APPROVAL_TIMEOUT`).
    pub approval_timeout_secs: i64,
    /// Extra writable roots (`JOY_EXEC_WRITABLE_ROOTS` — shared with the exec sandbox:
    /// one variable, one meaning — "where Joy may put bytes").
    pub extra_roots: Vec<PathBuf>,
    /// Directories a human answered "always"/"this session" to: writes inside them are not asked
    /// about again. The persisted ones (`settings.json` `write_allow_dirs`) are loaded into this
    /// same cell at startup, and a fresh yes is pushed here at once — an approval that only takes
    /// effect after a restart is not what the click promised.
    ///
    /// This **never widens the boundary**: `contained` still runs first, so an approved directory
    /// outside the writable roots unlocks nothing.
    pub approved_dirs: Arc<Mutex<Vec<PathBuf>>>,
}

/// "Someone already said yes" — same trick as exec.rs: a `PermissionRequest` hook that
/// explicitly allowed replaces the approval flow for this one call.
struct AlreadyApproved;

impl ApprovalBroker for AlreadyApproved {
    fn request(&self, _request: crate::approval::ApprovalRequest) -> crate::approval::ApprovalFut {
        Box::pin(async { true })
    }
}

/// **The single containment rule** — see `joyczl_agent::paths`. Re-exported here so the write
/// tools and the task verifier cannot drift into two different definitions of "inside".
pub use joyczl_agent::paths::inside_workspace;

/// The hard gate: `path` (after symlink resolution) must land inside one of `roots`.
/// Not configurable, not negotiable — approval is never consulted for an outside path.
fn contained(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if roots.is_empty() {
        return Err(
            "No working directory has been chosen yet: pick a folder in the cockpit first — the working directory is the only writable scope.".to_string(),
        );
    }
    for root in roots {
        if inside_workspace(path, root).is_ok() {
            return Ok(());
        }
    }
    let listed = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "{path:?} is outside the writable scope ({listed}). write_file / edit may only write files in these directories; to widen it, have the user add the directory to JOY_EXEC_WRITABLE_ROOTS, or pick it as the working directory."
    ))
}

/// Already covered by a directory someone said yes to ("本会话总是允许" / "记住这条")?
///
/// A poisoned lock reads as "nothing approved" — asking once more is a safe fallback, skipping the
/// question is not.
fn covered_by_approval(policy: &WritePolicy, path: &Path) -> bool {
    let dirs = match policy.approved_dirs.lock() {
        Ok(dirs) => dirs,
        Err(poisoned) => poisoned.into_inner(),
    };
    dirs.iter().any(|dir| inside_workspace(path, dir).is_ok())
}

/// Ask once when `JOY_APPROVAL=on-request`. Silence, timeout, no broker — all refusals.
pub(crate) async fn pass_write_gate(
    tool: &str,
    path: &Path,
    preview: &str,
    policy: &WritePolicy,
    roots: &[PathBuf],
    approval: Option<Arc<dyn ApprovalBroker>>,
) -> Result<(), String> {
    // Hard gate first: an approved directory outside the boundary unlocks nothing.
    contained(path, roots)?;
    if covered_by_approval(policy, path) || !policy.approval {
        return Ok(());
    }
    let Some(broker) = approval else {
        return Err(
            "JOY_APPROVAL=on-request is set, so a write needs to ask you — but nobody can answer (only a prompt from the terminal or the cockpit has somebody to answer it), so this write did not happen."
                .to_string(),
        );
    };
    let request = crate::approval::ApprovalRequest {
        tool: tool.to_string(),
        args_preview: preview.to_string(),
        reason: format!(
            "Write {path:?} (inside the writable scope; in on-request mode every write asks first)"
        ),
        timeout_secs: policy.approval_timeout_secs,
        // "记住这条" records the **directory**: the human approved this folder, not this file name.
        remember: path
            .parent()
            .map(|dir| RememberTarget::Directory(dir.to_path_buf())),
    };
    if broker.request(request).await {
        Ok(())
    } else {
        Err("This write was not approved (or nobody answered); skipped.".to_string())
    }
}

/// Fire the `PermissionRequest` hook: it may refuse outright, or allow on the user's behalf.
/// Returns the (possibly upgraded) approval to use for the gate, or a refusal reason.
pub(crate) async fn permission_hook(
    ctx: &ToolCtx,
    tool: &str,
    tool_input: Value,
) -> (Option<Arc<dyn ApprovalBroker>>, Option<String>) {
    let Some(hooks) = &ctx.hooks else {
        return (ctx.approval.clone(), None);
    };
    let outcome = hooks
        .fire(
            crate::hooks::HookEvent::PermissionRequest,
            json!({
                "session_id": ctx.session_id,
                "cwd": ctx.cwd,
                "tool_name": tool,
                "tool_input": tool_input,
            }),
        )
        .await;
    if let Some(why) = outcome.blocked {
        return (None, Some(why));
    }
    if outcome.allowed {
        eprintln!("(joy) a PermissionRequest hook allowed this {tool}");
        return (Some(Arc::new(AlreadyApproved)), None);
    }
    (ctx.approval.clone(), None)
}

/// Shared checks for content the model hands us: no NUL bytes (binary in a text tool),
/// no oversized payloads. `what` names the argument in the error text.
pub(crate) fn check_content(content: &str, what: &str) -> Result<()> {
    if content.contains('\0') {
        return Err(anyhow!(
            "{what} contains a NUL byte (probably binary content) — write_file / edit only write text; for binary use run_command (needs JOY_EXEC=1)."
        ));
    }
    if content.len() > MAX_WRITE_BYTES {
        return Err(anyhow!(
            "{what} is {} bytes, over the {MAX_WRITE_BYTES} per-write limit. Split large files, or use run_command (needs JOY_EXEC=1).",
            content.len()
        ));
    }
    Ok(())
}

/// Read an existing file as text for `edit`, with the same honesty rules as `read_file`.
pub(crate) fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            anyhow!("{path:?} is not UTF-8 text (probably a binary file) — edit only changes text files.")
        } else {
            anyhow!("Could not read {path:?}: {e}")
        }
    })
}

fn hunk_line(out: &mut String, marker: char, line: &str, rendered: &mut usize) {
    if *rendered < DIFF_MAX_LINES {
        out.push('\n');
        out.push(marker);
        out.push(' ');
        out.push_str(line);
    }
    *rendered += 1;
}

/// Render what changed as a compact unified-style hunk: common head/tail lines stay
/// shared, the differing middle shows as `-`/`+` with `DIFF_CONTEXT` lines of context.
/// Pure function — unit tests pin the format.
pub(crate) fn render_change(old: Option<&str>, new: &str) -> String {
    let Some(old) = old else {
        return format!("(new, {} lines)", new.lines().count().max(1));
    };
    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) =
        (old.lines().collect(), new.lines().collect());
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    if removed.is_empty() && added.is_empty() {
        return "(content unchanged)".to_string();
    }

    let ctx_start = prefix.saturating_sub(DIFF_CONTEXT);
    let ctx_end_old = (old_lines.len() - suffix + DIFF_CONTEXT).min(old_lines.len());
    let ctx_end_new = (new_lines.len() - suffix + DIFF_CONTEXT).min(new_lines.len());

    let mut out = format!(
        "@@ -{},{} +{},{} @@",
        ctx_start + 1,
        ctx_end_old - ctx_start,
        ctx_start + 1,
        ctx_end_new - ctx_start
    );
    let mut rendered = 0usize;
    for line in &old_lines[ctx_start..prefix] {
        hunk_line(&mut out, ' ', line, &mut rendered);
    }
    for line in removed {
        hunk_line(&mut out, '-', line, &mut rendered);
    }
    for line in added {
        hunk_line(&mut out, '+', line, &mut rendered);
    }
    for line in &new_lines[new_lines.len() - suffix..ctx_end_new] {
        hunk_line(&mut out, ' ', line, &mut rendered);
    }
    if rendered > DIFF_MAX_LINES {
        out.push_str(&format!(
            "\n[diff truncated: rendering the first {DIFF_MAX_LINES} lines only — use read_file for the whole thing.]"
        ));
    }
    out
}

/// The `write_file` registered into the tool table. Only registered when `JOY_WRITE=1`.
pub fn write_file(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "write_file".to_string(),
        description: "Write a whole piece of text into a file (overwriting it). It may only write inside the working directory and the directories listed in JOY_EXEC_WRITABLE_ROOTS; anything else is refused. To change a small stretch of an existing file, use edit (cheaper in tokens)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The target file path. ~ is expanded; a missing parent directory is created."},
                "content": {"type": "string", "description": "The full text to write (UTF-8, 1MB at most, replacing the file)."}
            },
            "required": ["path", "content"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let raw = require_str(&args, "path")?;
                let content = raw_str(&args, "content")?;
                check_content(&content, "content")?;
                let path = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&raw));

                let (approval, blocked) = permission_hook(
                    &ctx,
                    "write_file",
                    json!({ "path": raw, "content": content }),
                )
                .await;
                if let Some(why) = blocked {
                    return Ok(format!(
                        "Error: {why} (a PermissionRequest hook refused this write)"
                    ));
                }
                let roots = crate::roots::workspace_roots(&ctx, &policy.extra_roots).workspaces;
                if let Err(why) =
                    pass_write_gate("write_file", &path, &raw, &policy, &roots, approval).await
                {
                    return Ok(format!("Error: {why}"));
                }

                let old = if path.is_file() {
                    Some(read_text(&path)?)
                } else {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| anyhow!("Could not create the parent directory {parent:?}: {e}"))?;
                    }
                    None
                };
                if old.as_deref() == Some(content.as_str()) {
                    return Ok(format!(
                        "The content is unchanged, nothing was written: {path:?} ({} lines / {} bytes)",
                        content.lines().count().max(1),
                        content.len()
                    ));
                }

                std::fs::write(&path, &content).map_err(|e| anyhow!("Could not write {path:?}: {e}"))?;
                let was = old
                    .as_ref()
                    .map(|o| format!("(was {} lines)", o.lines().count().max(1)))
                    .unwrap_or_default();
                Ok(format!(
                    "Wrote {path:?}: {} lines / {} bytes{was}\n{}",
                    content.lines().count().max(1),
                    content.len(),
                    render_change(old.as_deref(), &content)
                ))
            })
        }),
    }
}

/// The `edit` registered into the tool table. Only registered when `JOY_WRITE=1`.
pub fn edit(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "edit".to_string(),
        description: "Replace one stretch of a file with another exactly (old_string must appear exactly once in the file: zero or several occurrences are both refused — include a few surrounding lines to make it unique). It may only change text files inside the working directory and the directories listed in JOY_EXEC_WRITABLE_ROOTS."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to edit (it must already exist). ~ is expanded."},
                "old_string": {"type": "string", "description": "The text to be replaced; it must match the file character for character and appear exactly once."},
                "new_string": {"type": "string", "description": "The replacement text (identical to old_string means no change, and is refused)."}
            },
            "required": ["path", "old_string", "new_string"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let raw = require_str(&args, "path")?;
                let old_string = raw_str(&args, "old_string")?;
                let new_string = raw_str(&args, "new_string")?;
                if old_string.is_empty() {
                    return Err(anyhow!("old_string must not be empty (use write_file to rewrite a whole file)."));
                }
                if old_string == new_string {
                    return Err(anyhow!("old_string and new_string are the same — there is nothing to change."));
                }
                let path = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&raw));
                if !path.is_file() {
                    return Err(anyhow!(
                        "File not found: {raw} (edit only changes files that exist; use write_file to create one)"
                    ));
                }
                check_content(&new_string, "new_string")?;

                let (approval, blocked) = permission_hook(
                    &ctx,
                    "edit",
                    json!({ "path": raw, "old_string": old_string, "new_string": new_string }),
                )
                .await;
                if let Some(why) = blocked {
                    return Ok(format!(
                        "Error: {why} (a PermissionRequest hook refused this edit)"
                    ));
                }
                let roots = crate::roots::workspace_roots(&ctx, &policy.extra_roots).workspaces;
                if let Err(why) =
                    pass_write_gate("edit", &path, &raw, &policy, &roots, approval).await
                {
                    return Ok(format!("Error: {why}"));
                }

                let old = read_text(&path)?;
                match old.matches(&old_string).count() {
                    0 => {
                        return Err(anyhow!(
                            "old_string was not found in {raw} (0 occurrences). read_file first to confirm the text — spaces and line endings must match character for character."
                        ))
                    }
                    1 => {}
                    n => {
                        return Err(anyhow!(
                            "old_string occurs {n} times in {raw} — edit replaces exactly one match. Include the surrounding lines so it becomes unique."
                        ))
                    }
                }

                let new_content = old.replacen(&old_string, &new_string, 1);
                std::fs::write(&path, &new_content)
                    .map_err(|e| anyhow!("Could not write {path:?}: {e}"))?;
                Ok(format!(
                    "Edited {path:?}: 1 replacement\n{}",
                    render_change(Some(&old), &new_content)
                ))
            })
        }),
    }
}
