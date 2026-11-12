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
            description: "Store a fact worth remembering for the long run (about a person, a project, a preference). Right for things like \"Alex prefers morning meetings\"."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "Who or what the fact is about, e.g. alex or the acme project"},
                    "content": {"type": "string", "description": "The fact itself, in one sentence"}
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
                            "Already recorded (not stored twice): #{} {} — {}",
                            row.id, row.subject, row.content
                        ));
                    }
                    Ok(format!(
                        "Remembered: **{}** — {} (in the facts table of {}, retrievable with search_memory at any time)",
                        row.subject,
                        row.content,
                        ctx.home.join("state.db").display()
                    ))
                })
            }),
        },
        Tool {
            name: "forget_note".to_string(),
            description: "Forget every memory about a subject. Use it only when the user explicitly says to forget something.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "The subject to forget"}
                },
                "required": ["subject"]
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let subject = require_str(&args, "subject")?;
                    let removed = ctx.facts.forget_subject(&subject).await?;
                    if removed == 0 {
                        Ok(format!("There is no memory about \"{subject}\", so nothing was deleted."))
                    } else {
                        Ok(format!("Forgot {removed} memories about \"{subject}\"."))
                    }
                })
            }),
        },
        Tool {
            name: "search_memory".to_string(),
            description: "Search the memories. Use it when the user asks about the past, a person or a preference.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search terms"},
                    "top_k": {"type": "integer", "description": "How many at most, 4 by default"}
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
                        return Ok(format!("There is nothing in memory about \"{query}\"."));
                    }
                    let lines: Vec<String> = hits
                        .iter()
                        .map(|f| format!("- **{}**: {} (source {})", f.subject, f.content, f.source))
                        .collect();
                    Ok(format!("Found {}:\n{}", hits.len(), lines.join("\n")))
                })
            }),
        },
        Tool {
            name: "read_memory_page".to_string(),
            description: "Read one page of the memory library (the `[title](page.md)` name from the catalog in the system prompt). The catalog carries one-line summaries only; read the page for detail, sources or the exact wording."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page": {"type": "string", "description": "The page name, e.g. alex (without .md)"}
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
                                "(this page records no sources)".to_string()
                            } else {
                                found.sources.join("、")
                            };
                            Ok(format!(
                                "# {} ({})\n\n{}\n\nSources: {}",
                                found.title, found.kind, found.body, sources
                            ))
                        }
                        None => Ok(format!(
                            "The memory library has no page \"{page}\". The readable pages are in the catalog in the system prompt."
                        )),
                    }
                })
            }),
        },
        Tool {
            name: "read_skill".to_string(),
            description: "Read a skill's full text (the skill catalog in the system prompt gives the names). The catalog carries one line each; the body is where the steps, commands and caveats live."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill name — the one in `- name — description` in the catalog"}
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
                                "No skill named \"{name}\". Known skills: {}",
                                if known.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    known.join(", ")
                                }
                            ))
                        }
                    }
                })
            }),
        },
        Tool {
            name: "list_memory".to_string(),
            description: "List the most recently remembered facts. Use it when the user wants to know what Joy has remembered.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many, 10 by default"}
                },
                "required": []
            }),
            handler: Arc::new(|ctx: ToolCtx, args: Value| {
                Box::pin(async move {
                    let limit = opt_u32(&args, "limit", 10)?;
                    let rows = ctx.facts.recent(limit, 0).await?;
                    if rows.is_empty() {
                        return Ok(
                            "Memory is still empty. Tell Joy something about yourself and it will remember.".to_string()
                        );
                    }
                    let lines: Vec<String> = rows
                        .iter()
                        .map(|f| format!("- **{}**: {}", f.subject, f.content))
                        .collect();
                    Ok(format!(
                        "The {} most recent memories:\n{}",
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
        description: "When a capability is missing from the tool table, use this to find it by keyword and open it (usable next turn). E.g. \"calendar\", \"email\", \"spreadsheet\", \"screenshot\"."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What capability to look for, e.g. \"calendar event\""}
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
                        "The tool table is already complete; there are no hidden tools — just use what you have.".to_string()
                    );
                };
                let opened = activate(&query);
                if opened.is_empty() {
                    return Ok(format!(
                        "No additional tool found for \"{query}\". Either the name is off, or that capability is not installed here."
                    ));
                }
                Ok(format!(
                    "Opened {} tools, usable next turn: {}",
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
        description: "Get the current local time on the user's machine (with the weekday and time zone). Call it before scheduling anything or explaining \"today\"/\"tomorrow\"."
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|_ctx: ToolCtx, _args: Value| {
            Box::pin(async move {
                let now = Local::now();
                Ok(format!(
                    "It is now {} ({}, UTC{})",
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
