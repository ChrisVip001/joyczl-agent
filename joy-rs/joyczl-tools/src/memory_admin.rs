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
        Some(v) => v.as_i64().map(Some).ok_or_else(|| {
            anyhow::anyhow!("Parameter '{key}' should be an integer (the memory id), got {v}")
        }),
    }
}

pub fn manage_memory() -> Tool {
    Tool {
        name: "manage_memory".to_string(),
        description: "Maintain memory: change the text of a fact, or delete a fact/episode by id. The ids come from list_memory / search_memory output. Use it when the user says \"that one is wrong, fix it\" or \"delete that\"."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["update_fact", "delete_fact", "delete_episode"],
                    "description": "update_fact rewrites a fact; delete_fact deletes a fact; delete_episode deletes an episode"
                },
                "id": {"type": "integer", "description": "the memory id (from list/search output)"},
                "content": {"type": "string", "description": "the new text when action=update_fact"}
            },
            "required": ["action", "id"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let action = require_str(&args, "action")?;
                let id = opt_i64(&args, "id")?
                    .ok_or_else(|| anyhow::anyhow!("Missing parameter 'id' (the memory id)"))?;
                match action.as_str() {
                    "update_fact" => {
                        let content = require_str(&args, "content")?;
                        if ctx.facts.update(id, &content).await? {
                            Ok(format!("Fact {id} now reads: \"{content}\"."))
                        } else {
                            Ok(format!("There is no fact with id {id} in memory. Check the id with list_memory first."))
                        }
                    }
                    "delete_fact" => {
                        if ctx.facts.delete(id).await? {
                            Ok(format!("Deleted fact {id}."))
                        } else {
                            Ok(format!("There is no fact with id {id} in memory. Check the id with list_memory first."))
                        }
                    }
                    "delete_episode" => {
                        if ctx.episodes.delete(id).await? {
                            Ok(format!("Deleted episode {id}."))
                        } else {
                            Ok(format!("There is no episode with id {id}."))
                        }
                    }
                    other => Ok(format!(
                        "Unknown action '{other}'. Options: update_fact / delete_fact / delete_episode."
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
        description: "Write a reusable workflow as a SKILL.md (procedural memory) that loads automatically the next time a related message appears. **Only call it after the user agrees.** body holds step-by-step instructions; description says when to use it (including trigger words)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "a lowercase slug, e.g. weekly-review"},
                "description": {"type": "string", "description": "one sentence: what it does and when to use it (including trigger words)"},
                "body": {"type": "string", "description": "step-by-step instructions (markdown)"}
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
                        "The skill name must be a short slug, e.g. 'weekly-review' (lowercase letters, digits, hyphens)."
                            .to_string(),
                    );
                }
                let dir = ctx.home.join("skills").join(&name);
                let dest = dir.join("SKILL.md");
                if dest.exists() {
                    return Ok(format!("A skill named '{name}' already exists — pick another name."));
                }
                let text =
                    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
                if joyczl_memory::skills::parse_skill_text(&text).is_none() {
                    return Ok("This content failed validation — both name and description are required.".to_string());
                }
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&dest, text)?;

                Ok(format!(
                    "Skill '{name}' created ({}). It takes effect automatically when a message like \"{description}\" comes up.",
                    dest.display()
                ))
            })
        }),
    }
}
