//! 本地日历：Joy 自己创建的事件。
//!
//! 语义：
//!   * state.db 永远是权威落点（确定性评测断言的就是这里）；
//!   * `calendar.ics` 是可导入的文件副本；
//!   * **幂等**：同标题 + 同开始时间的事件只存一次 —— 模型犯糊涂或用户
//!     不耐烦，都不能造成三重预定。
//!
//! Apple / Google 日历的同步是工具层（joyczl-tools）的事：state 只管
//! 本地的这张表，联网同步失败也不能影响本地写入。

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CalendarRow {
    pub id: i64,
    pub title: String,
    /// ISO 8601，分钟精度。
    pub start: String,
    /// SQL 里是保留字 `"end"`；SELECT 出来的列名就是 `end`，字段名照它起。
    pub end: String,
    pub attendees: String,
    pub notes: String,
    pub created_at: Option<String>,
}

#[derive(Clone)]
pub struct Calendar {
    pool: SqlitePool,
}

impl Calendar {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 写一条事件。同 (title, start) 已存在时不重复写，返回 `Ok(None)` ——
    /// 调用方据此告诉模型「事件已经在了」，而不是假装又建了一遍。
    pub async fn add(
        &self,
        title: &str,
        start: &str,
        end: &str,
        attendees: &str,
        notes: &str,
    ) -> Result<Option<CalendarRow>> {
        let row = sqlx::query_as::<_, CalendarRow>(
            r#"INSERT OR IGNORE INTO calendar_events (title, start, "end", attendees, notes)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, title, start, "end", attendees, notes, created_at"#,
        )
        .bind(title)
        .bind(start)
        .bind(end)
        .bind(attendees)
        .bind(notes)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// 按开始日期的区间检索（两端都含当日；`start`/`end` 是 ISO 日期）。
    /// 都不给就是全部，按开始时间升序。
    pub async fn list(
        &self,
        start: Option<&str>,
        end: Option<&str>,
        limit: u32,
    ) -> Result<Vec<CalendarRow>> {
        let mut builder = sqlx::QueryBuilder::new(
            r#"SELECT id, title, start, "end", attendees, notes, created_at
               FROM calendar_events"#,
        );
        let mut has_where = false;
        if let Some(start) = start {
            // 只比日期部分（前 10 个字符）：ISO 格式下前缀相等即同一天，
            // 「7 月 14 日的安排」不会漏掉 09:00 那条，也不会吞进 15 日的。
            builder
                .push(" WHERE substr(start, 1, 10) >= ")
                .push_bind(start.to_string());
            has_where = true;
        }
        if let Some(end) = end {
            builder.push(if has_where { " AND " } else { " WHERE " });
            builder
                .push("substr(start, 1, 10) <= ")
                .push_bind(end.to_string());
        }
        builder.push(" ORDER BY start LIMIT ");
        builder.push_bind(limit as i64);
        builder
            .build_query_as::<CalendarRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(Into::into)
    }
}
