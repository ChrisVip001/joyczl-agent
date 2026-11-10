//! Write tools: `write_file` (whole-file write) and `edit` (one exact-match replacement).
//!
//! **Off by default** (`JOY_WRITE=1` registers them; off, the model can't even see the names).
//! The trust boundary is the **writable roots** — the working directory, the Joy home, and
//! `JOY_EXEC_WRITABLE_ROOTS`: the same roots the exec sandbox may write. There is no process
//! sandbox here (the write happens in-process), so the containment check is the hard gate:
//! outside the roots is refused before approval is ever consulted — approval only ever unlocks
//! a write that is already inside the boundary, never one outside it.
//!
//! Like every tool: **refusals and failures are `Error: …` text, never Err** — the loop keeps going.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::approval::ApprovalBroker;
use crate::{require_str, Tool, ToolCtx};

/// Raw string argument — **no trimming**. `require_str` trims, which is right for
/// commands and paths but would silently corrupt file content: a trailing newline or
/// leading spaces are part of the file, and a write tool must not "tidy" its data.
fn raw_str(args: &Value, key: &str) -> Result<String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        None | Some(Value::Null) => Err(anyhow!("缺少参数 '{key}'")),
        Some(v) => Err(anyhow!("参数 '{key}' 应该是字符串，得到的是 {v}")),
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
}

/// "Someone already said yes" — same trick as exec.rs: a `PermissionRequest` hook that
/// explicitly allowed replaces the approval flow for this one call.
struct AlreadyApproved;

impl ApprovalBroker for AlreadyApproved {
    fn request(&self, _request: crate::approval::ApprovalRequest) -> crate::approval::ApprovalFut {
        Box::pin(async { true })
    }
}

/// Writable roots for this call: **the working directory the human picked**, plus the
/// policy's explicitly added roots. Nothing else.
///
/// `ctx.cwd` is `Some` only when the turn declared one (see `resolve_turn_cwd`) — that act is
/// the capability switch: no chosen workspace ⇒ no writable root ⇒ no write at all. The Joy
/// home and the process cwd are deliberately *not* roots: an assistant with a workspace must
/// not be able to wander into memory, skills or wherever the server happened to be started.
fn writable_roots(ctx: &ToolCtx, policy: &WritePolicy) -> Vec<PathBuf> {
    let Some(workspace) = &ctx.cwd else {
        return Vec::new();
    };
    let mut roots = vec![workspace.clone()];
    roots.extend(policy.extra_roots.iter().cloned());
    roots
}

/// Canonicalize the deepest existing ancestor of `path` — the file itself may not exist yet
/// (a create is not an escape), but every symlink in its real prefix must resolve inside.
fn existing_ancestor(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(real) => return real,
            Err(_) => {
                if !cur.pop() {
                    return path.to_path_buf(); // nowhere to anchor; containment below refuses
                }
            }
        }
    }
}

/// The hard gate: `path` (after symlink resolution) must land inside one of `roots`.
/// Not configurable, not negotiable — approval is never consulted for an outside path.
fn contained(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if roots.is_empty() {
        return Err(
            "还没有选择工作目录：先在驾驶舱选一个文件夹，工作目录就是唯一的可写范围。".to_string(),
        );
    }
    let real = existing_ancestor(path);
    for root in roots {
        if let Ok(root_real) = root.canonicalize() {
            if real.starts_with(&root_real) {
                return Ok(());
            }
        }
    }
    let listed = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "{path:?} 在可写范围之外（{listed}）。write_file / edit 只能写这些目录里的文件；\
         范围外的写入请走 run_command（需 JOY_EXEC=1）或让用户自己来。"
    ))
}

/// Ask once when `JOY_APPROVAL=on-request`. Silence, timeout, no broker — all refusals.
async fn pass_write_gate(
    tool: &str,
    path: &Path,
    preview: &str,
    policy: &WritePolicy,
    roots: &[PathBuf],
    approval: Option<Arc<dyn ApprovalBroker>>,
) -> Result<(), String> {
    contained(path, roots)?;
    if !policy.approval {
        return Ok(());
    }
    let Some(broker) = approval else {
        return Err(
            "当前是 JOY_APPROVAL=on-request，写入需要问你一次，但没人能回答 \
             （从终端或驾驶舱提问才有人应答）—— 这次写入没有执行。"
                .to_string(),
        );
    };
    let request = crate::approval::ApprovalRequest {
        tool: tool.to_string(),
        args_preview: preview.to_string(),
        reason: format!("写入 {path:?}（可写范围内；on-request 模式每次写入都会确认）"),
        timeout_secs: policy.approval_timeout_secs,
    };
    if broker.request(request).await {
        Ok(())
    } else {
        Err("这次写入没有被批准（或没人回答），已跳过。".to_string())
    }
}

/// Fire the `PermissionRequest` hook: it may refuse outright, or allow on the user's behalf.
/// Returns the (possibly upgraded) approval to use for the gate, or a refusal reason.
async fn permission_hook(
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
        eprintln!("(joy) PermissionRequest hook 放行了这次 {tool}");
        return (Some(Arc::new(AlreadyApproved)), None);
    }
    (ctx.approval.clone(), None)
}

/// Shared checks for content the model hands us: no NUL bytes (binary in a text tool),
/// no oversized payloads. `what` names the argument in the error text.
fn check_content(content: &str, what: &str) -> Result<()> {
    if content.contains('\0') {
        return Err(anyhow!(
            "{what} 含 NUL 字节（可能是二进制内容）—— write_file / edit 只写文本；\
             二进制请走 run_command（需 JOY_EXEC=1）。"
        ));
    }
    if content.len() > MAX_WRITE_BYTES {
        return Err(anyhow!(
            "{what} 有 {} 字节，超过单次写入上限 {MAX_WRITE_BYTES}。大文件请拆分、\
             或走 run_command（需 JOY_EXEC=1）。",
            content.len()
        ));
    }
    Ok(())
}

/// Read an existing file as text for `edit`, with the same honesty rules as `read_file`.
fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            anyhow!("{path:?} 不是 UTF-8 文本（可能是二进制文件）—— edit 只改文本文件。")
        } else {
            anyhow!("读 {path:?} 出错：{e}")
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
        return format!("（新建，{} 行）", new.lines().count().max(1));
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
        return "（内容未变）".to_string();
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
            "\n[diff 截断：只渲染前 {DIFF_MAX_LINES} 行 —— 用 read_file 看全文。]"
        ));
    }
    out
}

/// The `write_file` registered into the tool table. Only registered when `JOY_WRITE=1`.
pub fn write_file(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "write_file".to_string(),
        description: "把整段文本写进一个文件（覆盖原内容）。只能写工作目录、Joy 的 home、\
                      以及 JOY_EXEC_WRITABLE_ROOTS 列出的目录；范围外会被拒绝。\
                      要改现有文件的一小段，用 edit（更省 token）。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "目标文件路径。支持 ~ 展开；父目录不存在会自动创建。"},
                "content": {"type": "string", "description": "要写入的完整文本（UTF-8，上限 1MB，覆盖原文件）。"}
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
                        "Error: {why}（PermissionRequest hook 拒绝了这次写入）"
                    ));
                }
                if let Err(why) = pass_write_gate(
                    "write_file",
                    &path,
                    &raw,
                    &policy,
                    &writable_roots(&ctx, &policy),
                    approval,
                )
                .await
                {
                    return Ok(format!("Error: {why}"));
                }

                let old = if path.is_file() {
                    Some(read_text(&path)?)
                } else {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| anyhow!("创建父目录 {parent:?} 失败：{e}"))?;
                    }
                    None
                };
                if old.as_deref() == Some(content.as_str()) {
                    return Ok(format!(
                        "内容未变，没有写盘：{path:?}（{} 行 / {} 字节）",
                        content.lines().count().max(1),
                        content.len()
                    ));
                }

                std::fs::write(&path, &content).map_err(|e| anyhow!("写 {path:?} 失败：{e}"))?;
                let was = old
                    .as_ref()
                    .map(|o| format!("（原 {} 行）", o.lines().count().max(1)))
                    .unwrap_or_default();
                Ok(format!(
                    "已写入 {path:?}：{} 行 / {} 字节{was}\n{}",
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
        description: "把文件里的一段文本精确替换成另一段（old_string 必须在文件里恰好出现 \
                      一次：0 次或多次都会被拒绝，多带上前后几行让它唯一）。只能改工作目录、\
                      Joy 的 home、以及 JOY_EXEC_WRITABLE_ROOTS 列出的目录下的文本文件。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "要编辑的文件路径（必须已存在）。支持 ~ 展开。"},
                "old_string": {"type": "string", "description": "要被替换的原文，必须与文件内容逐字符一致，且恰好出现一次。"},
                "new_string": {"type": "string", "description": "替换后的文本（与 old_string 相同等于没改，会被拒绝）。"}
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
                    return Err(anyhow!("old_string 不能为空（整文件重写用 write_file）。"));
                }
                if old_string == new_string {
                    return Err(anyhow!("old_string 与 new_string 相同 —— 没有要做的修改。"));
                }
                let path = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&raw));
                if !path.is_file() {
                    return Err(anyhow!(
                        "找不到文件：{raw}（edit 只改已存在的文件；新建用 write_file）"
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
                        "Error: {why}（PermissionRequest hook 拒绝了这次编辑）"
                    ));
                }
                if let Err(why) = pass_write_gate(
                    "edit",
                    &path,
                    &raw,
                    &policy,
                    &writable_roots(&ctx, &policy),
                    approval,
                )
                .await
                {
                    return Ok(format!("Error: {why}"));
                }

                let old = read_text(&path)?;
                match old.matches(&old_string).count() {
                    0 => {
                        return Err(anyhow!(
                            "old_string 在 {raw} 里没有找到（0 处）。先 read_file 确认原文——\
                             空格、行尾都要逐字符一致。"
                        ))
                    }
                    1 => {}
                    n => {
                        return Err(anyhow!(
                            "old_string 在 {raw} 里出现了 {n} 次——edit 只替换恰好一次的匹配。\
                             把前后几行一并放进 old_string，让它变成唯一匹配。"
                        ))
                    }
                }

                let new_content = old.replacen(&old_string, &new_string, 1);
                std::fs::write(&path, &new_content)
                    .map_err(|e| anyhow!("写 {path:?} 失败：{e}"))?;
                Ok(format!(
                    "已编辑 {path:?}：替换 1 处\n{}",
                    render_change(Some(&old), &new_content)
                ))
            })
        }),
    }
}
