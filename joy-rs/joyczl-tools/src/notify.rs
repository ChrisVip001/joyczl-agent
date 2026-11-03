//! Notifier (M3a): pushes "things a human should see" out of the process.
//!
//! Event kinds: **approval requests** (the human is off-screen — they can't click Slack
//! but should know to come back), **job completion** (a 7x24 background command finished
//! or died), **goal endings** (satisfied/impossible/round-limit/blocked — the loop
//! stopped, a human should know). All are **side channels**: a failed send only notes
//! to stderr, never blocks a turn — same failure posture as the usage ledger (M2d).
//!
//! Integration discipline is the same as `ApprovalBroker`/`JobRegistry`: the tool layer
//! sees only the trait; the implementation is wired in app-server (Slack webhook).
//!
//! Egress target: the URL comes from `<home>/notify.json` (admin-configured). **Deliberately
//! no** `fetch_url`-style SSRF guardrails — those guard against "the model points, we
//! fetch"; here the URL is hand-configured by the admin, and pointing at an intranet
//! self-hosted endpoint (ntfy/Gotify) is legitimate use, not an attack surface.
//! Switch: `JOY_NOTIFY=1` (off by default) — external sends are a user-pressed decision,
//! same philosophy as `JOY_HOOKS`/`JOY_DELEGATE`.

use std::path::Path;
use std::sync::Arc;

/// Things a human should see.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    ApprovalRequested {
        turn_id: String,
        tool: String,
        args_preview: String,
        reason: String,
    },
    JobFinished {
        id: String,
        command: String,
        status: String,
        exit_code: Option<i32>,
    },
    ScheduleRan {
        name: String,
        status: String,
        preview: String,
    },
    GoalFinished {
        session_id: String,
        status: String,
        reason: String,
    },
}

impl NotifyEvent {
    /// One human-readable sentence — the body of the Slack message.
    pub fn message(&self) -> String {
        match self {
            NotifyEvent::ApprovalRequested {
                tool,
                args_preview,
                reason,
                ..
            } => format!(
                "🔔 Joy 请求批准：`{tool}` {args_preview}\n{reason}\n（去驾驶舱或终端批准）"
            ),
            NotifyEvent::JobFinished {
                id,
                command,
                status,
                exit_code,
            } => match exit_code {
                Some(code) => {
                    format!("⚙️ Joy 后台作业 {id} 结束（{status}，exit {code}）：`{command}`")
                }
                None => format!("⚙️ Joy 后台作业 {id} 结束（{status}）：`{command}`"),
            },
            NotifyEvent::ScheduleRan {
                name,
                status,
                preview,
            } => format!("⏰ Joy 定时任务 '{name}' {status}：{preview}"),
            NotifyEvent::GoalFinished { status, reason, .. } => {
                format!("🎯 Joy 目标停了（{status}）：{reason}")
            }
        }
    }
}

/// Notifier. The implementation **must not** block the caller (fire-and-forget); on failure, note to stderr.
pub trait Notifier: Send + Sync {
    fn notify(&self, event: NotifyEvent);
}

/// Slack incoming webhook (also compatible with self-hosted endpoints taking `{"text": …}` JSON).
pub struct Slack {
    url: String,
    client: reqwest::Client,
}

impl Slack {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }
}

impl Notifier for Slack {
    fn notify(&self, event: NotifyEvent) {
        let text = event.message();
        let url = self.url.clone();
        let client = self.client.clone();
        // Spawn a standalone task: the caller (turn loop, job completion) doesn't wait on the network.
        tokio::spawn(async move {
            let result = client
                .post(&url)
                .json(&serde_json::json!({ "text": text }))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match result {
                Ok(resp) if !resp.status().is_success() => {
                    eprintln!("(joy) 通知没送出去：HTTP {}", resp.status());
                }
                Err(e) => eprintln!("(joy) 通知没送出去：{e}"),
                Ok(_) => {}
            }
        });
    }
}

/// `<home>/notify.json`: `{ "slack": "https://hooks.slack.com/services/…" }`.
/// Same convention as `mcp.json`/`hooks.json`/`schedules.json`: one file per home,
/// naturally per-user in hub mode. File missing, `JOY_NOTIFY` off, field empty — all
/// mean no notifier (return None, silently: "not configured" is the norm, not an error).
pub fn load(home: &Path, enabled: bool) -> Option<Arc<dyn Notifier>> {
    if !enabled {
        return None;
    }
    let path = home.join("notify.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| eprintln!("(joy) notify.json 读不懂，通知保持关闭：{e}"))
        .ok()?;
    let url = value.get("slack")?.as_str()?.trim().to_string();
    if url.is_empty() {
        return None;
    }
    eprintln!("(joy) 通知已启用：Slack webhook（来自 {}）", path.display());
    Some(Arc::new(Slack::new(url)))
}
