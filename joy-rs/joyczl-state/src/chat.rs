//! 原始对话日志，以及「会话」这个轻量概念。
//!
//! 会话不是一张表，只是 chat_log 上的一个 `session_id` 标签。所以「新建会话」
//! 不建任何东西，换一个标签就行；而 consolidation 仍然能跨会话读到所有未提炼的行。

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

/// chat_log 的一行。会话历史的本体。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MessageRow {
    /// 行号。**也是翻页的游标** —— 比时间戳可靠：同一轮的两行是同一秒写进去的。
    pub id: i64,
    pub role: String,
    pub content: String,
    /// `created_at` 的原样：SQLite 的 `datetime('now')`，UTC，空格分隔。
    /// 转成协议要的 ISO 8601 是出口那边的事（见 app-server 里的 `iso`）——
    /// 这一层只管把库里存的东西原样交出来。
    pub at: String,
    /// 这一轮的遥测 JSON，只有 assistant 行上有。也原样带出去：
    /// 它该被解析成什么形状，是协议层的事。
    pub meta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    /// 标题 = 该会话第一条用户消息。
    pub title: String,
    pub messages: i64,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
}

#[derive(Clone)]
pub struct Chat {
    pool: SqlitePool,
}

impl Chat {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 记一轮对话。`meta` 是这一轮的遥测 JSON（gate/graph/iterations/…），
    /// 只挂在 assistant 行上 —— 重开一个旧会话时还能看到当时是哪个脑子答的。
    pub async fn append_exchange(
        &self,
        user: &str,
        assistant: &str,
        session_id: &str,
        source: &str,
        meta: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO chat_log (role, content, session_id, source) VALUES ('user', ?, ?, ?)",
        )
        .bind(user)
        .bind(session_id)
        .bind(source)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO chat_log (role, content, session_id, source, meta)
             VALUES ('assistant', ?, ?, ?, ?)",
        )
        .bind(assistant)
        .bind(session_id)
        .bind(source)
        .bind(meta)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 待提炼的行（按时间顺序）。consolidation 的原料与阈值判断用。
    pub async fn unconsolidated(&self) -> Result<Vec<(i64, String, String)>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            // 只取**到点了**的行：提炼失败的那批会带着 `consolidation_next_at`
            // 排到未来，时间没到就不该再被捞出来（否则每轮都白烧一次模型调用）。
            "SELECT id, role, content FROM chat_log
             WHERE consolidated = 0
               AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// **认领**一批待提炼的行：一条 SQL 完成「选中 + 标记」，返回自己认到的那批。
    ///
    /// 为什么不是「读一批 → 提炼 → 再标记」：那个顺序里，两个并发 turn（REPL
    /// 一轮 + dashboard 一轮、或 schedule 与用户同时触发）会**同时**看到「到期」，
    /// 于是同一批日志被提炼两遍 —— 白烧一次模型调用、`consolidation_tries`
    /// 多加一次。事实倒是不会重复（写入口有去重闸，那是另一道、更慢的防线）。
    ///
    /// 认领是原子的：输的那个拿到空列表，于是它**连模型都不调**。代价是
    /// 「先标记后处理」，所以每条失败路径都必须 `mark_consolidation_failed`
    /// 把认领还回去。
    pub async fn claim_unconsolidated(&self) -> Result<Vec<(i64, String, String)>> {
        // 与 `unconsolidated` 同一个 WHERE（到点才捞），只是把 SELECT 换成
        // UPDATE … RETURNING：标记与取回之间没有空档。
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "UPDATE chat_log SET consolidated = 1
             WHERE id IN (
                 SELECT id FROM chat_log
                 WHERE consolidated = 0
                   AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
                 ORDER BY id
             )
             RETURNING id, role, content",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 提炼失败：**把认领还回去**，并按指数退避排到未来。
    ///
    /// 认领（`claim_unconsolidated`）是先标记后处理的，所以失败时必须归零
    /// `consolidated` —— 否则这批日志被标成「已提炼」却什么都没写进去，那是
    /// 真的丢数据。1 分钟起、每次翻倍、上限 1 小时：一条永远提炼不出来的
    /// 记录不该让每一轮都重试它，但也不该被彻底放弃（模型可能只是这会儿抖动）。
    pub async fn mark_consolidation_failed(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            let tries: i64 =
                sqlx::query_scalar("SELECT consolidation_tries FROM chat_log WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?
                    .unwrap_or(0);
            let backoff = (60i64 * 2i64.pow(tries.min(6) as u32)).min(3_600);
            sqlx::query(
                "UPDATE chat_log
                 SET consolidation_tries = consolidation_tries + 1,
                     consolidation_next_at = datetime('now', ?),
                     consolidated = 0
                 WHERE id = ?",
            )
            .bind(format!("+{backoff} seconds"))
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    // （「标记为已提炼」不再是单独一步：`claim_unconsolidated` 在认领的那一刻
    //  就已经标记了。从前那个 `mark_consolidated` 随之删除 —— 留着它就是给
    //  「已经标过了再标一遍」留一扇门。）

    /// 一个旧会话的 (用户, 助手) 轮次，按顺序 —— 切回旧会话时用来重建工作记忆。
    pub async fn session_history(&self, session_id: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT role, content FROM chat_log WHERE session_id = ? ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut pairs = Vec::new();
        let mut pending: Option<String> = None;
        for (role, content) in rows {
            match role.as_str() {
                "user" => pending = Some(content),
                "assistant" => {
                    if let Some(u) = pending.take() {
                        pairs.push((u, content));
                    }
                }
                _ => {}
            }
        }
        Ok(pairs)
    }

    /// 记一次「实测 × 估算」的配对（用 provider 回报的 usage 校准本地估算）。
    ///
    /// 两个数都不为正时什么都不记 —— 图路径（不走 loop）就是这种情况，
    /// 拿 0 去算比值会得出一个荒唐的倍率。
    pub async fn observe_context(
        &self,
        session_id: &str,
        observed: i64,
        estimated: i64,
    ) -> Result<()> {
        if observed <= 0 || estimated <= 0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO session_context (session_id, observed_input_tokens, estimated_input_tokens, updated_at)
             VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                observed_input_tokens = excluded.observed_input_tokens,
                estimated_input_tokens = excluded.estimated_input_tokens,
                updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(observed)
        .bind(estimated)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 最近一次的实测/估算比值，夹在 `0.5..=2.0`。
    ///
    /// 夹住是刻意的：一次异常请求（比如 1 token 的探针）能把比值推到天上，
    /// 而预算被那样一个数带偏，比不校准更糟。没有记录时返回 `None`（不校准）。
    pub async fn context_factor(&self, session_id: &str) -> Result<Option<f64>> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT observed_input_tokens, estimated_input_tokens
             FROM session_context WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(observed, estimated)| {
            if observed <= 0 || estimated <= 0 {
                return None;
            }
            Some((observed as f64 / estimated as f64).clamp(0.5, 2.0))
        }))
    }

    /// 这个会话的滚动摘要：(覆盖了多少轮, 摘要)。
    ///
    /// 没有摘要、或表里那行读不出来，都当「还没有」—— 摘要只是省 token 的
    /// 手段，坏了顶多让 prompt 多塞几轮原文，不该让一轮对话起不来。
    pub async fn load_rollup(&self, session_id: &str) -> Result<Option<(i32, String)>> {
        let row = sqlx::query_as::<_, (i64, String)>(
            "SELECT covered_turns, summary FROM context_rollups WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(covered, summary)| (covered as i32, summary)))
    }

    /// 覆盖滚动摘要。同一会话一行，**只往前滚**。
    ///
    /// 返回「这次写进去了吗」。`false` = 被单调守卫挡下：库里已经有一份
    /// `covered_turns` 更大（也就是更新）的摘要，于是这一份被丢弃。
    ///
    /// 为什么需要守卫：压缩在两个并发 turn 里可能同时发生（同一会话没有串行
    /// 保护，见 docs/limitations.md），后写的那个不一定更新 —— 没有守卫时，
    /// 用户会拿到一份**更旧**的摘要，而且界面毫无异常。
    pub async fn save_rollup(
        &self,
        session_id: &str,
        covered_turns: i32,
        summary: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            // `WHERE excluded.covered_turns > context_rollups.covered_turns`：
            // 只接受更新的。冲突行上这个条件不成立时，DO UPDATE 什么都不做
            // （SQLite 的既定语义），于是 rows_affected() = 0。
            "INSERT INTO context_rollups (session_id, covered_turns, summary, updated_at)
             VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                covered_turns = excluded.covered_turns,
                summary = excluded.summary,
                updated_at = excluded.updated_at
             WHERE excluded.covered_turns > context_rollups.covered_turns",
        )
        .bind(session_id)
        .bind(covered_turns as i64)
        .bind(summary)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 一个会话的消息，**最新的在最前**。
    ///
    /// 方向跟对话本身相反是有原因的：这个方法要么拿「最近的一页」，
    /// 要么拿「比某条更早的一页」—— 两种用法都是从新往旧走。
    /// 排给人在屏幕上看的那一份（自下而上）是展示层的事。
    ///
    /// `before` 是「只取比这条 id 更早的」，不给就是最新的那一页。
    /// 默认给 `i64::MAX` 而不是拼两句 SQL：`id < ?` 一个表达式就够，
    /// 也不用引入 `(? IS NULL OR ...)` 那种要 bind 两遍的写法。
    pub async fn messages(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        sqlx::query_as::<_, MessageRow>(
            "SELECT id, role, content, created_at AS at, meta
               FROM chat_log
              WHERE session_id = ? AND id < ?
              ORDER BY id DESC
              LIMIT ?",
        )
        .bind(session_id)
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 所有会话，最近有消息的排在最前。
    pub async fn sessions(&self) -> Result<Vec<SessionRow>> {
        #[derive(sqlx::FromRow)]
        struct Raw {
            session_id: String,
            title: Option<String>,
            messages: i64,
            started_at: Option<String>,
            last_at: Option<String>,
        }

        let rows = sqlx::query_as::<_, Raw>(
            "SELECT session_id,
                    COUNT(*) AS messages,
                    MIN(created_at) AS started_at,
                    MAX(created_at) AS last_at,
                    (SELECT c.content FROM chat_log c
                      WHERE c.session_id = s.session_id AND c.role = 'user'
                      ORDER BY c.id LIMIT 1) AS title
             FROM chat_log s
             GROUP BY session_id
             ORDER BY last_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SessionRow {
                id: r.session_id,
                title: r.title.unwrap_or_else(|| "(空会话)".to_string()),
                messages: r.messages,
                started_at: r.started_at,
                last_at: r.last_at,
            })
            .collect())
    }
}
