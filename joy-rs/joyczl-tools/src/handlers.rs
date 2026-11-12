//! Built-in tools.
//!
//! Every tool's output **states truthfully where the result landed** (local file, state.db table).
//! Not politeness: the model relays it to the user; clear text avoids a false "synced to the cloud".

use std::sync::Arc;

use chrono::Local;
use serde_json::{json, Value};

use crate::{opt_u32, require_str, Tool, ToolCtx};

/// The four memory tools: save, forget, search, list.
/// They are the part of Joy that "manages its own memory" — when the user says
/// "forget this one", it goes through forget_note, not some hidden admin command.
pub fn memory_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "save_note".to_string(),
            description: "把一条值得长期记住的事实存进记忆（关于人、项目、偏好）。\
                          适合「Alex 喜欢早上的会议」这种话。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "这条事实关于谁/什么，例如 alex 或 acme 项目"},
                    "content": {"type": "string", "description": "事实本身，一句话"}
                },
                "required": ["subject", "content"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let subject = require_str(&args, "subject")?;
                    let content = require_str(&args, "content")?;
                    // Category is optional: if the model omits it, it's `fact` (the write entry normalizes unknown values).
                    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("fact");
                    let (row, is_new) = ctx.facts.add(&subject, &content, "user", kind).await?;
                    if !is_new {
                        // Say "already remembered" outright: the model knows not to save it again,
                        // and the user knows their words weren't recorded twice.
                        return Ok(format!(
                            "这条已经记过了（没有重复入库）：#{} {} — {}",
                            row.id, row.subject, row.content
                        ));
                    }
                    Ok(format!(
                        "已记住：**{}** — {}（存在 {} 的 facts 表，可随时用 search_memory 找回）",
                        row.subject,
                        row.content,
                        ctx.home.join("state.db").display()
                    ))
                })
            }),
        },
        Tool {
            name: "forget_note".to_string(),
            description: "忘掉某个主题下的全部记忆。用户明确说「忘掉…」时才用。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "要忘记的主题"}
                },
                "required": ["subject"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let subject = require_str(&args, "subject")?;
                    let removed = ctx.facts.forget_subject(&subject).await?;
                    if removed == 0 {
                        Ok(format!("没有关于「{subject}」的记忆，无需删除。"))
                    } else {
                        Ok(format!("已忘掉 {removed} 条关于「{subject}」的记忆。"))
                    }
                })
            }),
        },
        Tool {
            name: "search_memory".to_string(),
            description: "在记忆里搜索。用户问起过去的事、某个人、某个偏好时用。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "搜索词"},
                    "top_k": {"type": "integer", "description": "最多返回几条，默认 4"}
                },
                "required": ["query"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let query = require_str(&args, "query")?;
                    let top_k = opt_u32(&args, "top_k", 4)?;
                    // Same hybrid path the turn's own retrieval uses (`search_hybrid`), so a
                    // keyword that never appears in the wording but is close in meaning still
                    // lands — and with no embedder configured it *is* the keyword search.
                    let hits = joyczl_memory::retrieval::search_hybrid(
                        &ctx.facts,
                        ctx.embedder.as_deref(),
                        &query,
                        top_k,
                    )
                    .await;
                    if hits.is_empty() {
                        return Ok(format!("记忆里没有关于「{query}」的东西。"));
                    }
                    let lines: Vec<String> = hits
                        .iter()
                        .map(|f| format!("- **{}**: {}（来源 {}）", f.subject, f.content, f.source))
                        .collect();
                    Ok(format!("找到 {} 条：\n{}", hits.len(), lines.join("\n")))
                })
            }),
        },
        Tool {
            name: "read_memory_page".to_string(),
            description: "读记忆库里的一页（系统提示里给的目录中，`[标题](页面名.md)` 那个名字）。\
                          目录只有一句话摘要；要细节、要出处、要核对原话时读页。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page": {"type": "string", "description": "页面名，例如 alex（不带 .md）"}
                },
                "required": ["page"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let page = require_str(&args, "page")?;
                    let bundle = joyczl_memory::bundle::Bundle::new(&ctx.home);
                    match bundle.read(&page)? {
                        Some(found) => {
                            // Provenance rides along: a page without its sources is a claim
                            // the model cannot check, and this project does not ship those.
                            let sources = if found.sources.is_empty() {
                                "（这页没有记出处）".to_string()
                            } else {
                                found.sources.join("、")
                            };
                            Ok(format!(
                                "# {}（{}）\n\n{}\n\n出处：{}",
                                found.title, found.kind, found.body, sources
                            ))
                        }
                        None => Ok(format!(
                            "记忆库里没有「{page}」这一页。可读的页面见系统提示里的目录。"
                        )),
                    }
                })
            }),
        },
        Tool {
            name: "read_skill".to_string(),
            description: "读一个技能的完整正文（系统提示里的技能目录给出名字）。\
                          目录只有一句话说明；正文里才是步骤、命令和注意事项。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "技能名，目录里 `- 名字 — 说明` 的那个名字"}
                },
                "required": ["name"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let name = require_str(&args, "name")?;
                    let mut loader = joyczl_memory::skills::SkillLoader::new(
                        joyczl_memory::skills::SkillLoader::dirs_for(&ctx.home),
                    );
                    // Deliberately uncapped: this text was asked for on purpose, one skill at
                    // a time — the cap in the prompt exists for text nobody asked for.
                    match loader.body_of(&name) {
                        Some(body) => Ok(format!("### {name}\n{body}")),
                        None => {
                            let known: Vec<String> = loader
                                .index_text(usize::MAX)
                                .lines()
                                .filter_map(|line| line.strip_prefix("- "))
                                .filter_map(|rest| rest.split(" — ").next())
                                .map(str::to_string)
                                .collect();
                            Ok(format!(
                                "没有叫「{name}」的技能。现有技能：{}",
                                if known.is_empty() {
                                    "（一个都没有）".to_string()
                                } else {
                                    known.join("、")
                                }
                            ))
                        }
                    }
                })
            }),
        },
        Tool {
            name: "list_memory".to_string(),
            description: "列出最近记住的几条事实。用户想知道 Joy 都记了什么时用。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "条数，默认 10"}
                },
                "required": []
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let limit = opt_u32(&args, "limit", 10)?;
                    let rows = ctx.facts.recent(limit, 0).await?;
                    if rows.is_empty() {
                        return Ok(
                            "记忆还是空的。告诉 Joy 一些关于你的事，它就会记住。".to_string()
                        );
                    }
                    let lines: Vec<String> = rows
                        .iter()
                        .map(|f| format!("- **{}**: {}", f.subject, f.content))
                        .collect();
                    Ok(format!(
                        "最近 {} 条记忆：\n{}",
                        rows.len(),
                        lines.join("\n")
                    ))
                })
            }),
        },
    ]
}

/// Widens the session's tool table (`JOY_TOOL_POLICY=deferred`).
///
/// The model sees `find_tools` plus the tools every turn needs; everything else is one
/// search away. It is a **token** knob: nothing becomes permitted here, and a switch-gated
/// tool never hides (`ALWAYS_ON`), so discovery cannot outrun consent.
pub fn find_tools() -> Tool {
    Tool {
        name: "find_tools".to_string(),
        description: "工具表里没有某个能力时，用这个工具按关键词找出来并打开（下一轮就能用）。\
                      例如「日历」「邮件」「表格」「截图」。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "要找什么能力，例如「日历 事件」"}
            },
            "required": ["query"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let query = require_str(&args, "query")?;
                let Some(activate) = ctx.activate else {
                    // Eager mode: the table is already complete. Saying "found nothing" here
                    // would send the model looking for another search tool that does not exist.
                    return Ok(
                        "工具表已经是完整的，没有隐藏的工具 —— 直接用现有的就好。".to_string()
                    );
                };
                let opened = activate(&query);
                if opened.is_empty() {
                    return Ok(format!(
                        "没有找到「{query}」相关的额外工具。要么名字不对，要么这个能力没装在这里。"
                    ));
                }
                Ok(format!(
                    "已打开 {} 个工具，下一轮就能用：{}",
                    opened.len(),
                    opened.join("、")
                ))
            })
        }),
    }
}

/// Lets the model know what time it is. Without it, the model can only guess "what day is today" —
/// and a wrong guess makes the schedule wrong too.
pub fn current_time() -> Tool {
    Tool {
        name: "current_time".to_string(),
        description: "获取用户电脑上的当前本地时间（含星期与时区）。\
                      安排日程、解释「今天」「明天」之前先调它。"
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|_ctx: ToolCtx, _args: Value| {
            Box::pin(async move {
                let now = Local::now();
                Ok(format!(
                    "现在是 {}（{}，UTC{}）",
                    now.format("%Y-%m-%d %H:%M:%S"),
                    now.format("%A"),
                    now.format("%:z")
                ))
            })
        }),
    }
}

/// The full P2 built-in tool set. More get added behind flags later — the registry is assembled
/// **explicitly**: no macros, no directory scanning, so "why is this tool here" is checkable at a glance.
pub fn build_default() -> crate::ToolRegistry {
    let mut registry = crate::ToolRegistry::new();
    for tool in memory_tools() {
        registry.register(tool);
    }
    registry.register(current_time());
    // Present in eager mode too: it costs one line of schema and answers "is there more"
    // without a round trip. In deferred mode it is the door to everything else.
    registry.register(find_tools());
    registry.register(crate::memory_admin::manage_memory());
    for tool in crate::schedules::schedule_tools() {
        registry.register(tool);
    }
    registry.register(crate::memory_admin::create_skill());
    registry.register(crate::calendar::create_event());
    registry.register(crate::calendar::list_events());
    registry.register(crate::messages::send_message());
    // search_web only when a backend is usable (a key configured, or DDG allowed):
    // a tool that cannot run must not appear in the model's view — it would only
    // invite calls that are guaranteed to fail. fetch_url is keyless, always on.
    if crate::web::search_ready() {
        registry.register(crate::web::search_web());
    }
    registry.register(crate::web::fetch_url());
    // Read-only file tools: no side effects, present by default (reads skip the execution gate — see fs.rs docs).
    registry.register(crate::fs::read_file());
    registry.register(crate::fs::list_dir());
    registry
}
