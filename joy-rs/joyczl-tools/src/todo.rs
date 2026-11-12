//! Todo list: the model's self-maintained "where am I right now".
//!
//! The design deliberately **adds planning capability only, no execution power** (the
//! list grants no tool permissions, blocks no calls, persists nowhere
//! — it just writes "what I plan to do, how far along" somewhere visible every turn. What
//! really needs "cross-session, claimable, dependency-aware" is a task system, not this table.
//!
//! Three hard boundaries (all tested):
//!
//! * **Whole-table replace**: every write replaces the full table, no per-item edits —
//!   per-item edits leave "who deleted item 3" unanswered, and the model rewrites tables well.
//! * **Only one `in_progress` at a time**: two tasks pushed in parallel mean none is in progress.
//!   (Rejected outright when false.)
//! * **Bounded**: 20 items, 4000 chars each. The list is working memory for humans, not a log.
//!
//! `revision` increases monotonically on every write: the cockpit drops stale updates by it
//! (an out-of-order network must not overwrite a new table with an old one).

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::Tool;

/// How many items max. Beyond it this isn't "things being done", but "things wished for".
pub const MAX_ITEMS: usize = 20;
/// Per-item content cap.
pub const MAX_CONTENT_CHARS: usize = 4_000;
/// Header when injected into the system prompt — fixed, recognizable even after compaction.
pub const INJECTION_HEADER: &str =
    "## To-do list (kept by todo_write · the whole table is replaced)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            "cancelled" => Some(TodoStatus::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone)]
pub struct TodoList {
    /// Monotonically increasing version number.
    pub revision: u64,
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// Render into the lines shown to the model (injection and tool replies share one rendering).
    pub fn render(&self) -> String {
        let mut out = format!("{INJECTION_HEADER} · revision {}\n", self.revision);
        for item in &self.items {
            out.push_str(&format!("- {}: {}\n", item.status.as_str(), item.content));
        }
        out.trim_end().to_string()
    }

    /// A one-line summary like "3 items: 1 in progress, 2 pending".
    pub fn summary(&self) -> String {
        let count = |status: TodoStatus| self.items.iter().filter(|i| i.status == status).count();
        format!(
            "{} items: {} in progress, {} pending, {} completed, {} cancelled",
            self.items.len(),
            count(TodoStatus::InProgress),
            count(TodoStatus::Pending),
            count(TodoStatus::Completed),
            count(TodoStatus::Cancelled)
        )
    }
}

/// Session -> list. **In-process**: the list is this run's working memory; a restart resets it
/// (things that must survive go through `save_note` and schedules, not this table).
#[derive(Debug, Default)]
pub struct TodoBoard {
    lists: Mutex<HashMap<String, TodoList>>,
}

impl TodoBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whole-table replace. If validation fails, nothing changes (`Err` carries a model-readable reason).
    pub fn write(&self, session: &str, items: Vec<TodoItem>) -> Result<TodoList, String> {
        if items.len() > MAX_ITEMS {
            return Err(format!(
                "The to-do list takes at most {MAX_ITEMS} items, got {} — the list is \"what is being done\", not \"what might be done\"",
                items.len()
            ));
        }
        for (index, item) in items.iter().enumerate() {
            let at = index + 1;
            if item.content.trim().is_empty() {
                return Err(format!("item {at} has empty content"));
            }
            let chars = item.content.chars().count();
            if chars > MAX_CONTENT_CHARS {
                return Err(format!(
                    "item {at} has {chars} characters, over {MAX_CONTENT_CHARS} — say what to do in one sentence"
                ));
            }
        }
        let running = items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .count();
        if running > 1 {
            return Err(format!(
                "Only one item may be in_progress at a time, got {running} — two things in progress means nothing is in progress"
            ));
        }

        let mut lists = self
            .lists
            .lock()
            .expect("the todo lock should not be poisoned");
        let revision = lists.get(session).map(|list| list.revision).unwrap_or(0) + 1;
        let list = TodoList { revision, items };
        lists.insert(session.to_string(), list.clone());
        Ok(list)
    }

    pub fn read(&self, session: &str) -> Option<TodoList> {
        self.lists
            .lock()
            .expect("the todo lock should not be poisoned")
            .get(session)
            .cloned()
    }

    /// The block injected into the system prompt. Returns `None` when there's no list — so the
    /// caller needn't decide itself "should an empty list get an empty header".
    pub fn render(&self, session: &str) -> Option<String> {
        self.read(session).map(|list| list.render())
    }
}

/// `todo_write`: **passing `todos` writes, omitting `todos` reads** (dual-purpose: saves a
/// tool name and one round of "which tool do I read with").
pub fn todo_write(board: std::sync::Arc<TodoBoard>) -> Tool {
    Tool {
        name: "todo_write".to_string(),
        description: "Maintain your own to-do list (whole-table replacement). Omit todos = read the current list back. The list is re-injected into the context every round, so on a long task it is more reliable than \"remembering\". Only one item may be in_progress."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The whole list (20 items at most). Omit it to just read the current list back.",
                    "items": {
                        "type": "object",
                        "required": ["content", "status"],
                        "properties": {
                            "content": { "type": "string", "description": "what to do, in one sentence" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        }
                    }
                }
            }
        }),
        handler: std::sync::Arc::new(move |ctx: crate::ToolCtx, args: Value| {
            let board = board.clone();
            Box::pin(async move {
                let Some(raw) = args.get("todos") else {
                    // Read: say honestly when there's no list, rather than an empty array to guess at.
                    return Ok(match board.read(&ctx.session_id) {
                        Some(list) => list.render(),
                        None => "There is no to-do list yet. Pass todos to create one.".to_string(),
                    });
                };
                let Some(array) = raw.as_array() else {
                    return Ok("Error: todos must be an array (each item with content and status)".to_string());
                };

                let mut items = Vec::with_capacity(array.len());
                for (index, raw) in array.iter().enumerate() {
                    let at = index + 1;
                    let Some(content) = raw.get("content").and_then(Value::as_str) else {
                        return Ok(format!("Error: item {at} has no content"));
                    };
                    let status = raw
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending");
                    let Some(status) = TodoStatus::parse(status) else {
                        return Ok(format!(
                            "Error: unknown status for item {at}: '{status}' (only pending / in_progress / completed / cancelled)"
                        ));
                    };
                    items.push(TodoItem {
                        content: content.to_string(),
                        status,
                    });
                }

                match board.write(&ctx.session_id, items) {
                    // Return the authoritative result as-is: what the model sees next turn is
                    // this copy (revision included), so it needn't guess what it just wrote.
                    Ok(list) => Ok(format!("{} ({})", list.render(), list.summary())),
                    Err(why) => Ok(format!("Error: {why}")),
                }
            })
        }),
    }
}
