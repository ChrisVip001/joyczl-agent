//! 记忆后端的契约。
//!
//! Joy 对「事实住在哪」保持中立：今天只有 SQLite，明天可以是 pgvector 或
//! 托管记忆 API。这种中立只有一个前提：**每个后端都会做同一组动作**。
//! 没有契约的时候，
//! 第二个后端少实现了两个方法，调用点的防御式兜底把一个响亮的错误
//! 变成了安静的谎言 —— agent 对用户说「没有这条记忆」，而记忆明明在库里。
//!
//! 所以：实现这两个 trait，然后跑 [`crate::conformance`] 里同一套验收 ——
//! 任何后端缺胳膊少腿，都在 CI 里现形，而不是在用户面前。
//!
//! 契约要点（实现者必读）：
//!   * **miss 绝不报错** —— 返回空列表 / `false`。会抛错的后端会把整轮带走。
//!   * `search` 返回空必须意味着「没命中」，绝不是「搜索没跑成」。
//!   * 本地存储天然同步（写即索引）；托管后端若最终一致，请在自己的
//!     实现里处理就绪等待，契约层面不设 `settle` —— 上游不该为某一家
//!     后端的特性付接口复杂度。

use anyhow::Result;

use crate::{EpisodeRow, Episodes, FactRow, Facts};

/// 语义记忆：关于用户、人、项目、偏好的长期事实。
///
/// `async_fn_in_trait` 的取舍与 mcp 的 `Rpc` trait 相同：接口面留给
/// 「自己仓库里、按 `impl Trait` 传」的实现，不为 dyn 兼容付 Send 装箱的
/// 复杂度。将来真有跨 crate 的远端后端，再升级成显式的 BoxFuture 版本。
#[allow(async_fn_in_trait)]
pub trait SemanticStore {
    /// 存一条事实。`source` 记录谁说的（"user" / "consolidation"），来源要可追溯。
    async fn add(&self, subject: &str, content: &str, source: &str, kind: &str) -> Result<FactRow>;
    /// 按关键词检索，相关性降序。没命中返回空 vec，绝不报错。
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<FactRow>>;
    /// 最近的事实（按 id 倒序），dashboard 与 list_memory 用。
    async fn recent(&self, limit: u32, offset: u32) -> Result<Vec<FactRow>>;
    /// 改一条事实的正文。id 不存在 → `false`，不报错。
    async fn update(&self, id: i64, content: &str) -> Result<bool>;
    /// 删一条事实。id 不存在 → `false`，不报错。
    async fn delete(&self, id: i64) -> Result<bool>;
    /// 忘掉某主题下的全部事实，返回实际删除条数。
    async fn forget_subject(&self, subject: &str) -> Result<u64>;
}

/// 情景记忆：发生过的事，带日期。
#[allow(async_fn_in_trait)]
pub trait EpisodicStore {
    async fn add(&self, happened_at: &str, summary: &str) -> Result<i64>;
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<EpisodeRow>>;
    async fn recent(&self, limit: u32) -> Result<Vec<EpisodeRow>>;
    async fn delete(&self, id: i64) -> Result<bool>;
}

impl SemanticStore for Facts {
    async fn add(&self, subject: &str, content: &str, source: &str, kind: &str) -> Result<FactRow> {
        // 存储抽象不需要「是不是新记的」这个细节，只把行交出去（去重仍然发生）。
        Ok(Facts::add(self, subject, content, source, kind).await?.0)
    }
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<FactRow>> {
        Facts::search(self, query, top_k).await
    }
    async fn recent(&self, limit: u32, offset: u32) -> Result<Vec<FactRow>> {
        Facts::recent(self, limit, offset).await
    }
    async fn update(&self, id: i64, content: &str) -> Result<bool> {
        Facts::update(self, id, content).await
    }
    async fn delete(&self, id: i64) -> Result<bool> {
        Facts::delete(self, id).await
    }
    async fn forget_subject(&self, subject: &str) -> Result<u64> {
        Facts::forget_subject(self, subject).await
    }
}

impl EpisodicStore for Episodes {
    async fn add(&self, happened_at: &str, summary: &str) -> Result<i64> {
        Episodes::add(self, happened_at, summary).await
    }
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<EpisodeRow>> {
        Episodes::search(self, query, top_k).await
    }
    async fn recent(&self, limit: u32) -> Result<Vec<EpisodeRow>> {
        Episodes::recent(self, limit).await
    }
    async fn delete(&self, id: i64) -> Result<bool> {
        Episodes::delete(self, id).await
    }
}
