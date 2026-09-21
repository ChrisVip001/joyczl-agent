//! 批准问话的服务端一端。
//!
//! `joyczl-tools` 只有 `ApprovalBroker` 这个 trait（工具层不认识协议与 Server）；
//! 这里是它的实现：发一条 `approvalRequested` 通知、把回答通道登记进等待表、
//! 然后**等**。等不到（超时、连接断了、太晚）就是拒绝 —— 默认拒绝是地基。
//!
//! 等待表按 turn_id 分桶，与 `turns`（取消令牌表）同一副形状：一轮结束就整桶
//! 摘掉，不会有「上一轮的问题还在等一个永远不会来的回答」。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use joyczl_protocol::{ApprovalRequestedNotification, ServerNotification};
use joyczl_tools::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest};
use tokio::sync::oneshot;

use crate::{narrow, EventSink};

/// 一个正在等人回答的问题。
pub(crate) struct Waiting {
    /// 回答送回这里。
    pub(crate) tx: oneshot::Sender<bool>,
    /// 被问的那条命令 —— `remember` 要把它写进放行表，所以得留着。
    pub(crate) command: String,
}

/// turn_id → request_id → 在等的问题。
pub(crate) type Pending = Arc<Mutex<HashMap<String, HashMap<String, Waiting>>>>;

pub(crate) struct Bridge {
    pub(crate) pending: Pending,
    pub(crate) sink: EventSink,
    pub(crate) turn_id: String,
    pub(crate) timeout_secs: i64,
}

impl ApprovalBroker for Bridge {
    fn request(&self, request: ApprovalRequest) -> ApprovalFut {
        let request_id = format!("ap-{}", next_request_id());
        let (tx, rx) = oneshot::channel();

        if let Ok(mut table) = self.pending.lock() {
            table.entry(self.turn_id.clone()).or_default().insert(
                request_id.clone(),
                Waiting {
                    tx,
                    command: request.args_preview.clone(),
                },
            );
        }

        self.sink
            .notification(ServerNotification::ApprovalRequested(
                ApprovalRequestedNotification {
                    turn_id: self.turn_id.clone(),
                    request_id: request_id.clone(),
                    tool: request.tool,
                    args_preview: request.args_preview,
                    reason: request.reason,
                    expires_in_ms: narrow(self.timeout_secs.saturating_mul(1000)),
                },
            ));

        let pending = self.pending.clone();
        let turn_id = self.turn_id.clone();
        let timeout = Duration::from_secs(self.timeout_secs.max(1) as u64);

        Box::pin(async move {
            // 超时按拒绝算。这里**必须**有超时：一个永远等下去的批准请求会把
            // 一轮对话永久挂住，而工具层的超时兜不住「服务端在等」这件事。
            let approved = matches!(tokio::time::timeout(timeout, rx).await, Ok(Ok(true)));
            // 无论结果如何都撤掉登记 —— 不然表会一直长。
            if let Ok(mut table) = pending.lock() {
                if let Some(per_turn) = table.get_mut(&turn_id) {
                    per_turn.remove(&request_id);
                    if per_turn.is_empty() {
                        table.remove(&turn_id);
                    }
                }
            }
            if !approved {
                eprintln!("(joy) 批准请求 {request_id} 没有在时限内得到批准，按拒绝处理");
            }
            approved
        })
    }
}

/// 请求号：进程内单调就够（一轮里的问题本来就不多）。
fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
