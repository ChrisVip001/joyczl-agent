//! manage_memory — the maintenance surface for memory.
//!
//! Query (search_memory) and list (list_memory) are everyday actions; this tool handles
//! edit and delete: change a fact's body, or delete a fact/episode by id. Operations go
//! by **id** (from list_memory / search_memory output), not by content — deleting by
//! content would take out good memories on the same topic.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{require_str, Tool, ToolCtx};
// Name rule shared with joy skill install / update (it becomes the directory name).
use joyczl_memory::skills::is_slug;

fn opt_i64(args: &Value, key: &str) -> Result<Option<i64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("参数 '{key}' 应该是整数（记忆的编号），得到的是 {v}")),
    }
}

pub fn manage_memory() -> Tool {
    Tool {
        name: "manage_memory".to_string(),
        description: "维护记忆：修改一条事实的正文，或按编号删除一条事实/情景。\
                      编号来自 list_memory / search_memory 的输出。\
                      用户说「那条记错了，改一下」或「把某条删掉」时用。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["update_fact", "delete_fact", "delete_episode"],
                    "description": "update_fact 改事实正文；delete_fact 删事实；delete_episode 删情景"
                },
                "id": {"type": "integer", "description": "记忆的编号（来自 list/search 的输出）"},
                "content": {"type": "string", "description": "action=update_fact 时的改正文"}
            },
            "required": ["action", "id"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let action = require_str(&args, "action")?;
                let id = opt_i64(&args, "id")?
                    .ok_or_else(|| anyhow::anyhow!("缺少参数 'id'（记忆的编号）"))?;
                match action.as_str() {
                    "update_fact" => {
                        let content = require_str(&args, "content")?;
                        if ctx.facts.update(id, &content).await? {
                            Ok(format!("已把第 {id} 条事实改成：「{content}」。"))
                        } else {
                            Ok(format!("记忆里没有编号 {id} 的事实。先用 list_memory 确认编号。"))
                        }
                    }
                    "delete_fact" => {
                        if ctx.facts.delete(id).await? {
                            Ok(format!("已删掉第 {id} 条事实。"))
                        } else {
                            Ok(format!("记忆里没有编号 {id} 的事实。先用 list_memory 确认编号。"))
                        }
                    }
                    "delete_episode" => {
                        if ctx.episodes.delete(id).await? {
                            Ok(format!("已删掉第 {id} 条情景记录。"))
                        } else {
                            Ok(format!("没有编号 {id} 的情景记录。"))
                        }
                    }
                    other => Ok(format!(
                        "不认识的 action '{other}'。可选：update_fact / delete_fact / delete_episode。"
                    )),
                }
            })
        }),
    }
}

/// create_skill — turns a user-taught workflow into a SKILL.md (procedural memory's write entry).
///
/// Only call **after the user agrees** (this rule lives in SOUL.md): an agent approving
/// its own code of conduct is the same accident as an unguarded update_soul.
/// Never overwrite an existing skill — built-in or user-written alike.
pub fn create_skill() -> Tool {
    Tool {
        name: "create_skill".to_string(),
        description: "把一个可复用的工作流写成 SKILL.md（过程记忆），下次相关消息出现时\
                      自动载入。**只在用户同意后调用。** body 是分步指令；\
                      description 要写清什么时候用（含触发词）。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "小写 slug，如 weekly-review"},
                "description": {"type": "string", "description": "一句话：做什么、什么时候用（含触发词）"},
                "body": {"type": "string", "description": "分步指令（markdown）"}
            },
            "required": ["name", "description", "body"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let name = require_str(&args, "name")?.to_lowercase().replace(' ', "-");
                let description = require_str(&args, "description")?;
                let body = require_str(&args, "body")?;

                if !is_slug(&name) {
                    return Ok(
                        "技能名要是一个短 slug，如 'weekly-review'（小写字母、数字、连字符）。"
                            .to_string(),
                    );
                }
                let dir = ctx.home.join("skills").join(&name);
                let dest = dir.join("SKILL.md");
                if dest.exists() {
                    return Ok(format!("已有一个叫 '{name}' 的技能 —— 换个名字。"));
                }
                let text =
                    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
                if joyczl_memory::skills::parse_skill_text(&text).is_none() {
                    return Ok("这份内容没通过校验 —— name 和 description 都必须有。".to_string());
                }
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&dest, text)?;

                Ok(format!(
                    "技能 '{name}' 已创建（{}）。它会在提到「{description}」这类消息时自动生效。",
                    dest.display()
                ))
            })
        }),
    }
}
