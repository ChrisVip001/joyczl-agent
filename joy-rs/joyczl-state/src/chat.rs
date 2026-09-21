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

    /// 待提炼的行（按时间顺序）。consolidation 的原料。
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

    /// 提炼失败：记一笔，并按指数退避排到未来。
    ///
    /// 与「成功才 mark_consolidated」并行不冲突：成功走 `consolidated = 1`，
    /// 失败走这里。1 分钟起、每次翻倍、上限 1 小时 —— 一条永远提炼不出来的
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
                     consolidation_next_at = datetime('now', ?)
                 WHERE id = ?",
            )
            .bind(format!("+{backoff} seconds"))
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 把一批行标记为已提炼。**只在提炼成功后调用** ——
    /// 提炼失败就让它们留着，下次再来，日志绝不丢。
    pub async fn mark_consolidated(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("UPDATE chat_log SET consolidated = 1 WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

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

    /// 覆盖滚动摘要。同一会话一行，后来的盖掉先前的 —— 摘要只往前滚。
    pub async fn save_rollup(
        &self,
        session_id: &str,
        covered_turns: i32,
        summary: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO context_rollups (session_id, covered_turns, summary, updated_at)
             VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                covered_turns = excluded.covered_turns,
                summary = excluded.summary,
                updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(covered_turns as i64)
        .bind(summary)
        .execute(&self.pool)
        .await?;
        Ok(())
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
