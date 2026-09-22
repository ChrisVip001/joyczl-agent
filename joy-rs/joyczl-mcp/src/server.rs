//! `joy mcp serve` —— 反过来的一侧：把 Joy 的**记忆**暴露成 MCP 工具。
//!
//! 在客户端一侧，Joy 把别人的 MCP 服务器插成自己的工具；在这里，Joy 自己
//! 成为一台 MCP 服务器，让别的 agent（Claude Code、codex 或任何会说 MCP
//! 的东西）读写同一份记忆。语义很直接：Joy 是这台机器上的**个人记忆层**，
//! 谁来问都答同一份事实 —— 记忆在 state.db 里，不在某个会话的上下文里。
//!
//! 只暴露记忆，不暴露工具：让另一个 agent 隔着 MCP 指挥 Joy 去发消息、
//! 建日程，那是「代理的代理」，权限会绕开用户亲手按下的那些开关。记忆是
//! 只读为主的数据，边界清楚。
//!
//! 传输就是换行分隔的 JSON-RPC（与 app-server 同一套分帧，MCP 的 stdio
//! 传输本来就是这个形状）。日志只走 stderr —— stdout 是协议通道。

use anyhow::Result;
use joyczl_state::{Episodes, Facts};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 协议版本。服务器实现的是这一版；客户端报告别的版本时照常应答 ——
/// 版本协商的失败模式不该是「一句话都连不上」。
const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct MemoryServer {
    facts: Facts,
    episodes: Episodes,
}

impl MemoryServer {
    /// 只收记忆的两个句柄，不收连接池 —— 这层是协议翻译，不该认识 sqlx。
    pub fn new(facts: Facts, episodes: Episodes) -> Self {
        Self { facts, episodes }
    }

    /// 一行请求进去，一行应答出来。通知（没有 id）返回 None。
    pub async fn handle(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // 通知：MCP 的 initialized 走这条 —— 不该有应答。
        let id = id?;

        let result = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "joy-memory", "version": env!("CARGO_PKG_VERSION")},
            })),
            "tools/list" => Ok(json!({"tools": tool_declarations()})),
            "tools/call" => {
                self.call_tool(request.get("params").cloned().unwrap_or_default())
                    .await
            }
            "ping" => Ok(json!({})),
            other => Err((-32601, format!("未知方法 '{other}'"))),
        };

        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": code, "message": message},
            }),
        })
    }

    async fn call_tool(&self, params: Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call 少了 name".to_string()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        // 工具执行失败是**结果里的文本**（isError: true），不是 JSON-RPC
        // 错误 —— 与 Joy 自己的工具层同一条规矩：调用方读得到原因。
        let text = match name {
            "memory_search" => self.search(&args).await,
            "memory_remember" => self.remember(&args).await,
            "memory_forget" => self.forget(&args).await,
            "memory_list" => self.list(&args).await,
            "memory_episodes" => self.episodes(&args).await,
            other => {
                return Err((-32602, format!("没有叫 '{other}' 的工具")));
            }
        };
        match text {
            Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}]})),
            Err(why) => Ok(json!({
                "content": [{"type": "text", "text": why}],
                "isError": true,
            })),
        }
    }

    async fn search(&self, args: &Value) -> Result<String, String> {
        let query = require_str(args, "query")?;
        let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5) as u32;

        let facts = self
            .facts
            .search(&query, top_k)
            .await
            .map_err(|e| e.to_string())?;
        let episodes = self
            .episodes
            .search(&query, 3)
            .await
            .map_err(|e| e.to_string())?;
        if facts.is_empty() && episodes.is_empty() {
            return Ok(format!("没有找到和「{query}」相关的记忆。"));
        }
        let mut lines = Vec::new();
        for fact in &facts {
            lines.push(format!("- **{}**: {}", fact.subject, fact.content));
        }
        for episode in &episodes {
            lines.push(format!("- ({}) {}", episode.happened_at, episode.summary));
        }
        Ok(lines.join("\n"))
    }

    async fn remember(&self, args: &Value) -> Result<String, String> {
        let subject = require_str(args, "subject")?;
        let content = require_str(args, "content")?;
        let (fact, is_new) = self
            .facts
            .add(&subject, &content, "mcp", "fact")
            .await
            .map_err(|e| e.to_string())?;
        Ok(if is_new {
            format!("已记住（#{id}）：{subject} — {content}", id = fact.id)
        } else {
            format!(
                "这条已经记过了（没重复入库）：#{id} {subject} — {content}",
                id = fact.id
            )
        })
    }

    async fn forget(&self, args: &Value) -> Result<String, String> {
        let subject = require_str(args, "subject")?;
        let removed = self
            .facts
            .forget_subject(&subject)
            .await
            .map_err(|e| e.to_string())?;
        if removed == 0 {
            Ok(format!("没有关于「{subject}」的记忆可忘。"))
        } else {
            Ok(format!("已忘掉「{subject}」下的 {removed} 条事实。"))
        }
    }

    async fn list(&self, args: &Value) -> Result<String, String> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32;
        let facts = self
            .facts
            .recent(limit, 0)
            .await
            .map_err(|e| e.to_string())?;
        if facts.is_empty() {
            return Ok("记忆里还没有事实。".to_string());
        }
        Ok(facts
            .iter()
            .map(|f| format!("#{} **{}**: {}", f.id, f.subject, f.content))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// 名字与 `memory_episodes` 的工具名撞了，方法借用一下不同的词。
    async fn episodes(&self, args: &Value) -> Result<String, String> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as u32;
        let episodes = self
            .episodes
            .recent(limit)
            .await
            .map_err(|e| e.to_string())?;
        if episodes.is_empty() {
            return Ok("记忆里还没有情景。".to_string());
        }
        Ok(episodes
            .iter()
            .map(|e| format!("- ({}) {}", e.happened_at, e.summary))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// stdio 主循环：一行一个请求，一行一个应答。读完 stdin 就退。
    pub async fn run_stdio(self) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut out = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(line) {
                Ok(request) => request,
                Err(e) => {
                    let error = json!({
                        "jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": format!("解析失败：{e}")},
                    });
                    write_line(&mut out, &error).await?;
                    continue;
                }
            };
            if let Some(response) = self.handle(request).await {
                write_line(&mut out, &response).await?;
            }
        }
        Ok(())
    }
}

async fn write_line(out: &mut tokio::io::Stdout, value: &Value) -> Result<()> {
    out.write_all(value.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("参数 '{key}' 必须是非空字符串。"))
}

/// 暴露给外部的五个记忆工具。**只有记忆** —— 见模块头里为什么。
fn tool_declarations() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_search",
            "description": "Search Joy's long-term memory (facts and dated episodes) by keyword.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "keywords"},
                    "top_k": {"type": "integer", "description": "how many facts to return (default 5)"}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "memory_remember",
            "description": "Store a durable fact in Joy's memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "who or what it is about"},
                    "content": {"type": "string", "description": "the fact itself"}
                },
                "required": ["subject", "content"]
            }
        }),
        json!({
            "name": "memory_forget",
            "description": "Forget every fact under a subject.",
            "inputSchema": {
                "type": "object",
                "properties": {"subject": {"type": "string"}},
                "required": ["subject"]
            }
        }),
        json!({
            "name": "memory_list",
            "description": "List the most recent facts in Joy's memory.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "description": "default 10"}}
            }
        }),
        json!({
            "name": "memory_episodes",
            "description": "List recent dated episodes (what happened, when).",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "description": "default 5"}}
            }
        }),
    ]
}
