//! 待办清单：模型自己维护的「我现在做到哪了」。
//!
//! 设计上刻意**只加规划能力，不加执行能力**（这一条抄的是 hermes 的 todo 与
//! Claude Code 的 TodoWrite）：清单不影响任何工具的权限、不阻塞任何调用、不落库
//! —— 它只是把「打算做什么、做到哪一步」写在一个每轮都看得见的地方。真正需要
//! 「跨会话、能认领、有依赖」的东西是任务系统，那是另一件事，不是把这张表撑大。
//!
//! 三条硬边界（都有测试）：
//!
//! * **整表替换**：每次写都是全量替换，不做逐项编辑 —— 逐项编辑会让「谁删了第 3
//!   项」这类问题没有答案，而模型本来就擅长重写整张表。
//! * **同时只允许一个 `in_progress`**：两个并行推进的任务等于没有推进中的任务。
//!   （deepseek-harness 把这做成必填的部署选项，false 时直接 reject —— 我们照做。）
//! * **有上限**：20 项、单项 4000 字符。清单是给人看的工作记忆，不是日志。
//!
//! `revision` 每次写单调递增：驾驶舱据此丢弃过期更新（网络乱序时不该把新表
//! 覆盖成旧的）。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::Tool;

/// 最多几项。超过它就说明这不是「正在做的事」，而是「想做的事」。
pub const MAX_ITEMS: usize = 20;
/// 单项内容上限。
pub const MAX_CONTENT_CHARS: usize = 4_000;
/// 注入 system prompt 时的标题 —— 固定不变，便于压缩之后仍然认得出来。
pub const INJECTION_HEADER: &str = "## 待办清单（todo_write 维护 · 整表替换）";

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
    /// 单调递增的版本号。
    pub revision: u64,
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// 渲染成给模型看的那几行（注入与工具应答共用同一份渲染）。
    pub fn render(&self) -> String {
        let mut out = format!("{INJECTION_HEADER} · 第 {} 版\n", self.revision);
        for item in &self.items {
            out.push_str(&format!("- {}: {}\n", item.status.as_str(), item.content));
        }
        out.trim_end().to_string()
    }

    /// 「3 项：1 进行中、2 未开始」这种一句话概括。
    pub fn summary(&self) -> String {
        let count = |status: TodoStatus| self.items.iter().filter(|i| i.status == status).count();
        format!(
            "{} 项：{} 进行中、{} 未开始、{} 已完成、{} 已取消",
            self.items.len(),
            count(TodoStatus::InProgress),
            count(TodoStatus::Pending),
            count(TodoStatus::Completed),
            count(TodoStatus::Cancelled)
        )
    }
}

/// 会话 -> 清单。**进程内**：清单是这一趟的工作记忆，重启就重来（要留下来的
/// 东西走 `save_note` 与 schedule，不是这张表）。
#[derive(Debug, Default)]
pub struct TodoBoard {
    lists: Mutex<HashMap<String, TodoList>>,
}

impl TodoBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 整表替换。校验不过就一个字都不改（`Err` 里是给模型读的理由）。
    pub fn write(&self, session: &str, items: Vec<TodoItem>) -> Result<TodoList, String> {
        if items.len() > MAX_ITEMS {
            return Err(format!(
                "待办最多 {MAX_ITEMS} 项，收到 {} 项 —— 清单是「正在做的」，不是「想做的」",
                items.len()
            ));
        }
        for (index, item) in items.iter().enumerate() {
            let at = index + 1;
            if item.content.trim().is_empty() {
                return Err(format!("第 {at} 项的 content 是空的"));
            }
            let chars = item.content.chars().count();
            if chars > MAX_CONTENT_CHARS {
                return Err(format!(
                    "第 {at} 项的 content 有 {chars} 个字符，超过 {MAX_CONTENT_CHARS} —— 一句话说清要做什么"
                ));
            }
        }
        let running = items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .count();
        if running > 1 {
            return Err(format!(
                "同时只能有一项 in_progress，收到 {running} 项 —— 两个并行推进的任务等于没有推进中的任务"
            ));
        }

        let mut lists = self.lists.lock().expect("todo 锁不该中毒");
        let revision = lists.get(session).map(|list| list.revision).unwrap_or(0) + 1;
        let list = TodoList { revision, items };
        lists.insert(session.to_string(), list.clone());
        Ok(list)
    }

    pub fn read(&self, session: &str) -> Option<TodoList> {
        self.lists
            .lock()
            .expect("todo 锁不该中毒")
            .get(session)
            .cloned()
    }

    /// 注入 system prompt 的那一段。没有清单就返回 `None` —— 调用方因此不必
    /// 自己判断「空清单要不要加个空标题」。
    pub fn render(&self, session: &str) -> Option<String> {
        self.read(session).map(|list| list.render())
    }
}

/// `todo_write`：**传 `todos` 就是写，省略 `todos` 就是读**（一物两用，
/// 省一个工具名，也少一次「读要用哪个工具」的犹豫）。
pub fn todo_write(board: std::sync::Arc<TodoBoard>) -> Tool {
    Tool {
        name: "todo_write".to_string(),
        description: "维护你自己那张待办清单（整表替换）。省略 todos = 读回当前清单。\
                      清单会在每一轮重新注入上下文，所以长任务里它比「记住」可靠。\
                      同时只能有一项 in_progress。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "整张清单（最多 20 项）。省略它 = 只读回当前清单。",
                    "items": {
                        "type": "object",
                        "required": ["content", "status"],
                        "properties": {
                            "content": { "type": "string", "description": "一句话说清要做什么" },
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
                    // 读：没清单也如实说，而不是回一个空数组让人猜。
                    return Ok(match board.read(&ctx.session_id) {
                        Some(list) => list.render(),
                        None => "还没有待办清单。要建一张就把 todos 传进来。".to_string(),
                    });
                };
                let Some(array) = raw.as_array() else {
                    return Ok("Error: todos 要是一个数组（每项含 content 与 status）".to_string());
                };

                let mut items = Vec::with_capacity(array.len());
                for (index, raw) in array.iter().enumerate() {
                    let at = index + 1;
                    let Some(content) = raw.get("content").and_then(Value::as_str) else {
                        return Ok(format!("Error: 第 {at} 项缺少 content"));
                    };
                    let status = raw
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending");
                    let Some(status) = TodoStatus::parse(status) else {
                        return Ok(format!(
                            "Error: 第 {at} 项的 status 不认识：'{status}'（只能是 pending / \
                             in_progress / completed / cancelled）"
                        ));
                    };
                    items.push(TodoItem {
                        content: content.to_string(),
                        status,
                    });
                }

                match board.write(&ctx.session_id, items) {
                    // 把权威结果原样回给模型：它下一轮看到的就是这一份（包括版本号），
                    // 不必凭记忆猜自己刚才写了什么。
                    Ok(list) => Ok(format!("{}（{}）", list.render(), list.summary())),
                    Err(why) => Ok(format!("Error: {why}")),
                }
            })
        }),
    }
}
