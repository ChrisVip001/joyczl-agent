//! 通知器（M3a）：把「需要人看的事」推出进程。
//!
//! 四类事件：**审批请求**（人在屏幕外，Slack 上点不了但能知道该回来看了）、
//! **作业收尾**（7x24 的后台命令跑完了/死了）、**目标终局**（satisfied/
//! impossible/round-limit/blocked 都算 —— 自主循环停了就该让人知道）。
//! 全部是**旁路**：发送失败只往 stderr 说一声，绝不拦 turn —— 与用量台账
//! （M2d）同一条失败姿态。
//!
//! 接入纪律与 `ApprovalBroker`/`JobRegistry` 相同：工具层只见 trait，实现
//! 在 app-server 装配（Slack webhook）。
//!
//! 出站目标：URL 来自 `<home>/notify.json`（管理员配置）。**刻意不做**
//! `fetch_url` 那套 SSRF 护栏 —— 那是防「模型指哪打哪」的；这里的 URL 是
//! 管理员亲手配的，指向内网自建端点（ntfy/Gotify）是合法用法，不是攻击面。
//!
//! 开关：`JOY_NOTIFY=1`（默认关）—— 往外部服务发数据是用户亲手按下的决定，
//! 与 `JOY_HOOKS`/`JOY_DELEGATE` 同一条哲学。

use std::path::Path;
use std::sync::Arc;

/// 需要人看的事。
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
    GoalFinished {
        session_id: String,
        status: String,
        reason: String,
    },
}

impl NotifyEvent {
    /// 一句人话 —— Slack 消息的正文。
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
            NotifyEvent::GoalFinished { status, reason, .. } => {
                format!("🎯 Joy 目标停了（{status}）：{reason}")
            }
        }
    }
}

/// 通知器。实现**必须**不阻塞调用方（fire-and-forget）、失败只往 stderr 说。
pub trait Notifier: Send + Sync {
    fn notify(&self, event: NotifyEvent);
}

/// Slack incoming webhook（也兼容收 `{"text": …}` JSON 的自建端点）。
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
        // 起一个独立 task：调用方（turn 循环、作业收尾）不等网络。
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

/// `<home>/notify.json`：`{ "slack": "https://hooks.slack.com/services/…" }`。
/// 与 `mcp.json`/`hooks.json`/`schedules.json` 同一约定：一个 home 一份，
/// hub 模式下天然按用户隔离。文件不在、`JOY_NOTIFY` 没开、字段空 —— 都等于
/// 没有通知器（返回 None，静默，因为「没配」是常态不是错误）。
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
