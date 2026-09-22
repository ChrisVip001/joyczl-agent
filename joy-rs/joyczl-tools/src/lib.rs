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

pub mod approval;
pub mod calendar;
pub mod exec;
pub mod handlers;
pub mod memory_admin;
pub mod messages;
pub mod report;
pub mod spill;
pub mod subagent;
pub mod web;

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tools_tests;

#[cfg(test)]
#[path = "exec_tests.rs"]
mod exec_tests;

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod subagent_tests;

#[cfg(test)]
#[path = "approval_tests.rs"]
mod approval_tests;

#[cfg(test)]
#[path = "spill_tests.rs"]
mod spill_tests;

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;

/// 工具执行时能拿到的东西。加字段要想清楚：每个工具都能看见全部。
/// 故意 Clone —— handler 的 Future 要拥有它，这样才能是 'static。
#[derive(Clone)]
pub struct ToolCtx {
    pub facts: Facts,
    pub episodes: Episodes,
    pub chat: Chat,
    pub calendar: Calendar,
    pub home: PathBuf,
    /// 这一轮属于哪个会话。日志与落盘用它归属（子代理给一个自己的名字，
    /// 于是「这条超长结果是谁弄出来的」查得到）。
    pub session_id: String,
    /// 有人可以问批准吗？`None` = 没有（子代理、`JOY_APPROVAL=never`、或没有
    /// 交互界面的调用方）—— 于是需要批准的动作直接拒绝，正如 `approval.rs` 的
    /// 「默认拒绝」。
    pub approval: Option<Arc<dyn crate::approval::ApprovalBroker>>,
}

pub type BoxFut = Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>;
pub type Handler = Arc<dyn Fn(ToolCtx, Value) -> BoxFut + Send + Sync>;

#[derive(Clone)]
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

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Tool>,
    /// 每个工具**预编译**好的参数校验器。注册时编一次，调用时零解析。
    ///
    /// 外层 `Option` 是「这个工具在不在表里」，内层是「它的 schema 编得出来吗」：
    /// MCP 服务器可能报上来一个坏 schema，那种情况跳过校验照常执行 ——
    /// 一个远端 schema 写坏了不该让工具直接不可用（但要在启动日志里说一声，
    /// 静默跳过校验会让人以为参数被查过）。
    validators: BTreeMap<String, Option<jsonschema::Validator>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 去掉某个工具的一份副本（子代理用：它不能再派生子代理）。
    ///
    /// 复制整个注册表而不是共享 —— 子代理的表与父的表从此互不影响，而工具
    /// 本身是 `Arc` 包着的，复制很便宜。
    pub fn without(&self, name: &str) -> Self {
        let mut copy = self.clone();
        copy.tools.remove(name);
        copy.validators.remove(name);
        copy
    }

    pub fn register(&mut self, tool: Tool) {
        let validator = match jsonschema::validator_for(&tool.input_schema) {
            Ok(validator) => Some(validator),
            Err(e) => {
                eprintln!(
                    "(joy) 工具 '{}' 的 input_schema 编译不了（{e}）—— 这个工具的参数不做校验",
                    tool.name
                );
                None
            }
        };
        self.validators.insert(tool.name.clone(), validator);
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

        // 参数先过一遍 schema，再交给 handler。三件事按这个顺序是有原因的：
        // 报错能具体到字段与期望（模型据此改得对），handler 里就不必再重复
        // 检查类型；而且**校验失败时 handler 根本没被调用**，不会留下半个副作用。
        if let Some(Some(validator)) = self.validators.get(name) {
            if let Err(why) = describe_violations(validator, &args) {
                return format!(
                    "Error: 参数不符合 {name} 的 schema —— {why}。请重写输入以满足 schema。"
                );
            }
        }

        match (tool.handler)(ctx, args).await {
            Ok(output) => output,
            Err(e) => format!("Error: 执行 {name} 失败：{e}"),
        }
    }
}

/// 用一份 schema 校验一个 JSON 值，报错就是给模型读的那句话（合规为 `Ok(())`）。
///
/// 工具参数的校验走同一条路（`describe_violations`），子代理的结构化结果也用它
/// —— 两处的措辞因此一致，模型不必学两套。
pub fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    let validator =
        jsonschema::validator_for(schema).map_err(|e| format!("这份 schema 本身编译不了：{e}"))?;
    describe_violations(&validator, value)
}

/// 把 schema 违规翻译成一句给模型读的话：最多列三处，每处带字段路径。
///
/// 不 dump 整段 schema —— 那会把上下文塞满，而模型要的只是「哪里不对」。
/// 全部合规时返回 `Ok(())`。
fn describe_violations(validator: &jsonschema::Validator, args: &Value) -> Result<(), String> {
    let all: Vec<String> = validator
        .iter_errors(args)
        .map(|error| {
            let path = error.instance_path();
            if path.is_empty() {
                format!("参数本身：{error}")
            } else {
                format!("{path}：{error}")
            }
        })
        .collect();
    if all.is_empty() {
        return Ok(());
    }
    let shown = all.len().min(3);
    let mut text = all[..shown].join("；");
    if all.len() > shown {
        text.push_str(&format!("（还有 {} 处）", all.len() - shown));
    }
    Err(text)
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
