//! 交互式批准：**默认关、默认拒绝、没人回答就是拒绝**。
//!
//! 这一层只有一个 trait —— 工具层不认识协议、不认识 Server，谁来问、怎么问
//! 是实现方的事（与 `subagent.rs` 同一条依赖纪律）。实现放在
//! `joyczl-app-server`：它才有通知出口与等待表。
//!
//! 三条不许松动的规矩：
//!
//! 1. **默认拒绝**。没有 broker（终端也没在问、驾驶舱没开）、超时、连接断了、
//!    回答得晚了 —— 一律不放行。放行只有一种来源：一个明确的「可以」。
//! 2. **批准只翻开放行表那一关**。硬拒名单与沙箱是不可协商的：`vet` 里那两道
//!    闸门在批准之前就返回了，批准根本没有机会碰到它们。
//! 3. **超时是实现方的责任**。`request` 必须自己兜住 `timeout_secs` 并返回
//!    `false` —— 一个永远不返回的批准请求会把一轮对话永久挂住。

use std::future::Future;
use std::pin::Pin;

/// 一次批准请求。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// 想执行什么（工具名）。
    pub tool: String,
    /// 给人看的预览：命令原文，或参数摘要。
    pub args_preview: String,
    /// 为什么没被放行规则直接放行。
    pub reason: String,
    /// 等人回答的秒数。实现方必须在它之后放弃并返回 `false`。
    pub timeout_secs: i64,
}

pub type ApprovalFut = Pin<Box<dyn Future<Output = bool> + Send>>;

/// 谁来回答「要不要执行」。实现方负责发问、等待与超时。
pub trait ApprovalBroker: Send + Sync {
    /// `true` = 批准；`false` = 拒绝（含超时、无人应答、出错）。
    fn request(&self, request: ApprovalRequest) -> ApprovalFut;
}
