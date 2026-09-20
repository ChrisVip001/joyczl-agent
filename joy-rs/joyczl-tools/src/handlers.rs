//! 内置工具。
//!
//! 每个工具的输出都**如实说明结果落在了哪儿**（本地文件、state.db 的哪张表）。
//! 这不是客套：模型会把这段话转述给用户，写清楚它才不会虚报「已同步到云端」。

use std::sync::Arc;

use chrono::Local;
use serde_json::{json, Value};

use crate::{opt_u32, require_str, Tool, ToolCtx};

/// 记忆相关的四个工具：存、忘、查、列。
/// 它们是 Joy「自己管自己的记忆」的那部分 —— 用户说「忘掉这条」时，
/// 走的就是 forget_note，而不是某个隐藏的管理命令。
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
                    let row = ctx.facts.add(&subject, &content, "user").await?;
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
                    let hits = ctx.facts.search(&query, top_k).await?;
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

/// 让模型知道现在几点。没有它，模型只能猜「今天星期几」——
/// 而猜错了日程就会跟着错。
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

/// P2 内置工具全集。以后按开关条件往里加 —— 注册表的组装是**显式的**，
/// 不用宏也不用扫目录，让「这个工具为什么在」一眼能查。
pub fn build_default() -> crate::ToolRegistry {
    let mut registry = crate::ToolRegistry::new();
    for tool in memory_tools() {
        registry.register(tool);
    }
    registry.register(current_time());
    registry.register(crate::memory_admin::manage_memory());
    registry.register(crate::memory_admin::create_skill());
    registry.register(crate::calendar::create_event());
    registry.register(crate::calendar::list_events());
    registry.register(crate::messages::send_message());
    registry.register(crate::web::search_web());
    registry
}
