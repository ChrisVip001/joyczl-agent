//! manage_memory —— 记忆的维护面。
//!
//! 查（search_memory）和列（list_memory）是日常动作；这个工具管的是
//! 修与删：改一条事实的正文、按编号删一条事实或情景。
//! 按**编号**操作（编号来自 list_memory / search_memory 的输出），
//! 而不是按内容 —— 按内容删会误伤同主题的好记忆。

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{require_str, Tool, ToolCtx};

/// create_skill 的名字只能是小写 slug（weekly-review 这种）。
/// 它要变成目录名，放行别的字符就是在给路径穿越递刀子。
fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

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

/// create_skill —— 把用户教的工作流固化成 SKILL.md（过程记忆的写入口）。
///
/// 只在**用户同意之后**才调用（这条规矩写在 SOUL.md 里）：agent 自己
/// 批准自己写行为准则，和 update_soul 不设防是同一种事故。
/// 从不覆盖已有技能 —— 内置的或用户写的都不行。
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
