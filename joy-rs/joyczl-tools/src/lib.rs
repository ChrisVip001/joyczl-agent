//! `joyczl-tools` —— 工具注册表。
//!
//! 一个工具就是三样东西：模型读得到的名字 + 描述、参数的 JSON schema、
//! 以及一个真正执行它的 Rust 函数。就这些。
//!
//! **工具失败时把错误作为文本返回给模型，而不是让 loop 崩掉。**
//! 模型看到错误可以换条路走，甚至直接向用户解释 —— 这比一轮对话
//! 整个失败好得多。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use joyczl_provider::ToolSchema;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use serde_json::Value;

pub mod calendar;
pub mod handlers;
pub mod memory_admin;
pub mod messages;
pub mod web;

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tools_tests;

/// 工具执行时能拿到的东西。加字段要想清楚：每个工具都能看见全部。
/// 故意 Clone —— handler 的 Future 要拥有它，这样才能是 'static。
#[derive(Clone)]
pub struct ToolCtx {
    pub facts: Facts,
    pub episodes: Episodes,
    pub chat: Chat,
    pub calendar: Calendar,
    pub home: PathBuf,
}

pub type BoxFut = Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>;
pub type Handler = Arc<dyn Fn(ToolCtx, Value) -> BoxFut + Send + Sync>;

pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: Handler,
}

/// schema 的外形要跟 provider 说的一样，这里集中转换一次。
impl Tool {
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Tool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Tool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(Tool::schema).collect()
    }

    /// 执行一次调用。**永不返回 Err** —— 错误变成模型能读到的文本。
    /// 不认识的工具也一样，模型可以纠正自己。
    pub async fn execute(&self, ctx: ToolCtx, name: &str, args: Value) -> String {
        let Some(tool) = self.tools.get(name) else {
            return format!(
                "Error: 没有叫 '{name}' 的工具。可用：{}",
                self.names().join(", ")
            );
        };
        match (tool.handler)(ctx, args).await {
            Ok(output) => output,
            Err(e) => format!("Error: 执行 {name} 失败：{e}"),
        }
    }
}

// ---- handler 里反复用的小工具 ---------------------------------------------

/// 取一个必填的字符串参数。缺了/类型不对都报得清楚 —— 这段文本会进模型
/// 的上下文，它读得懂就知道该怎么改。
pub fn require_str(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("缺少参数 '{key}'"))?;
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("参数 '{key}' 应该是字符串，得到的是 {value}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("参数 '{key}' 不能是空的");
    }
    Ok(trimmed.to_string())
}

/// 取一个可选的整数参数（带默认值）。
pub fn opt_u32(args: &Value, key: &str, default: u32) -> Result<u32> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| n as u32)
            .ok_or_else(|| anyhow::anyhow!("参数 '{key}' 应该是非负整数，得到的是 {v}")),
    }
}
