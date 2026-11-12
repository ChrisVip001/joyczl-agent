//! `joy mcp serve` — the reverse side: expose Joy's **memory** as MCP tools.
//!
//! On the client side Joy plugs other MCP servers in as its tools; here Joy itself becomes
//! an MCP server, letting other agents (Claude Code, codex, anything speaking MCP) read and
//! write the same memory. Direct semantics: Joy is the **personal memory layer** on this
//! machine; whoever asks gets the same facts — memory lives in state.db, not in a session.
//!
//! Memory only, no tools: letting another agent command Joy over MCP to send messages or
//! build schedules is "an agent's agent", and permissions would bypass the switches the
//! user pressed by hand. Memory is mostly read-only data with a clean boundary.
//!
//! The transport is newline-delimited JSON-RPC (the same framing as app-server; MCP's
//! stdio transport is exactly this shape). Logs go to stderr only — stdout is the protocol.

use anyhow::Result;
use joyczl_state::{Episodes, Facts};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Protocol version. The server implements this one; clients reporting other versions are
/// answered all the same — version negotiation must not fail as "not a word gets through".
const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct MemoryServer {
    facts: Facts,
    episodes: Episodes,
}

impl MemoryServer {
    /// Only the two memory handles, no connection pool — this layer is protocol translation, not sqlx.
    pub fn new(facts: Facts, episodes: Episodes) -> Self {
        Self { facts, episodes }
    }

    /// One request line in, one response line out. Notifications (no id) return None.
    pub async fn handle(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Notification: MCP's `initialized` goes through here — there must be no answer.
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
            other => Err((-32601, format!("unknown method '{other}'"))),
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
            .ok_or((-32602, "tools/call is missing name".to_string()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));

        // Tool execution failures are **text inside the result** (isError: true), not a
        // JSON-RPC error — same rule as Joy's own tool layer: the caller can read why.
        let text = match name {
            "memory_search" => self.search(&args).await,
            "memory_remember" => self.remember(&args).await,
            "memory_forget" => self.forget(&args).await,
            "memory_list" => self.list(&args).await,
            "memory_episodes" => self.episodes(&args).await,
            other => {
                return Err((-32602, format!("no tool named '{other}'")));
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
            return Ok(format!("Nothing in memory matches “{query}”."));
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
            format!("Remembered (#{id}): {subject} — {content}", id = fact.id)
        } else {
            format!(
                "Already remembered (not stored twice): #{id} {subject} — {content}",
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
            Ok(format!("There is no memory about “{subject}” to forget."))
        } else {
            Ok(format!("Forgot {removed} facts about “{subject}”."))
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
            return Ok("Memory has no facts yet.".to_string());
        }
        Ok(facts
            .iter()
            .map(|f| format!("#{} **{}**: {}", f.id, f.subject, f.content))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Name collides with the `memory_episodes` tool name; the method borrows a different word.
    async fn episodes(&self, args: &Value) -> Result<String, String> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as u32;
        let episodes = self
            .episodes
            .recent(limit)
            .await
            .map_err(|e| e.to_string())?;
        if episodes.is_empty() {
            return Ok("Memory has no episodes yet.".to_string());
        }
        Ok(episodes
            .iter()
            .map(|e| format!("- ({}) {}", e.happened_at, e.summary))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// stdio main loop: one request per line, one response per line. EOF on stdin exits.
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
                        "error": {"code": -32700, "message": format!("parse failed: {e}")},
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
        .ok_or_else(|| format!("Parameter '{key}' must be a non-empty string."))
}

/// The five memory tools exposed outside. **Memory only** — see the module header for why.
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
