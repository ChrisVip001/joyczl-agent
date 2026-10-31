//! 调度状态（迁移 0012）：`joy schedule` 的去重、崩溃恢复与每作业投递
//! 策略，全部落在 state.db 一张表里 —— 原先的 `schedules-state.json`
//! 从此只做一次性导入的来源，不再是权威。
//!
//! 模型（dispatch 先推进 next_run 防重放，joy 版）：
//!
//! ```text
//! tick 发现该触发 ──► claim(name, minute)      ← 先占坑（崩溃前已持久化）
//!                    ──► fire() 执行
//!                    ──► confirm(name, minute) ← 落定：last_fired = minute
//!
//! 重启时发现 claimed_minute 非空 = 上次执行到一半进程没了：
//!   catch_up=1 → 重跑一次（至少一次；晨报类宁可重不可漏）
//!   catch_up=0 → 直接 confirm 放弃（至多一次；通知类宁可漏不可重）
//! ```

use sqlx::SqlitePool;

/// 一行调度状态。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleRow {
    pub name: String,
    pub last_fired_minute: Option<String>,
    pub claimed_minute: Option<String>,
    pub catch_up: bool,
    pub delivery: String,
}

/// 调度状态访问器。Clone 便宜（一个连接池句柄），与 Jobs 同一条理由。
#[derive(Clone)]
pub struct Schedules {
    pool: SqlitePool,
}

impl Schedules {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 登记一个声明过的任务：不存在就建，存在就按声明刷新策略字段
    /// （catch_up/delivery 以**当前声明**为准），last_fired/claimed 保留。
    pub async fn ensure(
        &self,
        name: &str,
        catch_up: bool,
        delivery: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO schedule_state (name, catch_up, delivery) VALUES (?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET catch_up = excluded.catch_up, \
             delivery = excluded.delivery, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        )
        .bind(name)
        .bind(catch_up)
        .bind(delivery)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// 上次落定的触发分钟（去重用）。
    pub async fn last_fired(&self, name: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT last_fired_minute FROM schedule_state WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(m,)| m))
    }

    /// 未落定的占坑（重启恢复用）。有值 = 上次执行到一半进程没了。
    pub async fn pending_claim(&self, name: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT claimed_minute FROM schedule_state WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(m,)| m))
    }

    /// 占坑：dispatch 之前先持久化「我要跑这一分钟」。此时崩溃，重启后
    /// pending_claim 非空 —— 重放被显式接管，而不是靠运气。
    pub async fn claim(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE schedule_state SET claimed_minute = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// 落定：last_fired = minute，坑清空。成功与失败都走这里 —— 失败的
    /// 作业也算「这一分钟已经处理过」，没有退避机制时不该每 20 秒重试。
    pub async fn confirm(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE schedule_state SET last_fired_minute = ?, claimed_minute = NULL, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// 全部行的快照（驾驶舱的定时面板用）。按名字排，读失败由调用方兜底。
    pub async fn list(&self) -> Result<Vec<ScheduleRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT name, last_fired_minute, claimed_minute, catch_up, delivery              FROM schedule_state ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 旧 JSON 的一次性导入：只搬 last_fired，不动策略字段。幂等 ——
    /// DB 已有值时不再覆盖（调度器只在 last_fired 为空时才调它）。
    pub async fn import_legacy(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE schedule_state SET last_fired_minute = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Schedules, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("打开库");
        (Schedules::new(pool), dir)
    }

    /// ensure 建行、刷新策略但保留历史；claim → pending → confirm 全链路。
    #[tokio::test]
    async fn ensure_claim_confirm_round_trip() {
        let (schedules, _dir) = db().await;
        assert!(schedules.last_fired("morning").await.expect("空").is_none());

        schedules
            .ensure("morning", true, "outbox")
            .await
            .expect("登记");
        schedules
            .claim("morning", "2026-10-30T08:00")
            .await
            .expect("占坑");
        assert_eq!(
            schedules
                .pending_claim("morning")
                .await
                .expect("读坑")
                .as_deref(),
            Some("2026-10-30T08:00")
        );
        schedules
            .confirm("morning", "2026-10-30T08:00")
            .await
            .expect("落定");
        assert!(schedules
            .pending_claim("morning")
            .await
            .expect("坑清了")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("morning")
                .await
                .expect("读回")
                .as_deref(),
            Some("2026-10-30T08:00")
        );

        // 再 ensure：策略刷新、历史保留。
        schedules
            .ensure("morning", false, "notify")
            .await
            .expect("再登记");
        let raw: (bool, String, Option<String>) = sqlx::query_as(
            "SELECT catch_up, delivery, last_fired_minute FROM schedule_state WHERE name = 'morning'",
        )
        .fetch_one(&schedules.pool)
        .await
        .expect("读行");
        assert!(!raw.0, "策略按最新声明");
        assert_eq!(raw.1, "notify");
        assert_eq!(raw.2.as_deref(), Some("2026-10-30T08:00"), "历史不丢");
    }

    /// confirm 同时也是「放弃」的出口：catch_up=0 的作业占坑后进程死了，
    /// 重启 confirm 掉 —— 至多一次。
    #[tokio::test]
    async fn confirm_doubles_as_the_abandon_exit() {
        let (schedules, _dir) = db().await;
        schedules
            .ensure("notify-job", false, "notify")
            .await
            .expect("登记");
        schedules
            .claim("notify-job", "2026-10-30T09:00")
            .await
            .expect("占坑");
        // 进程死了。重启：catch_up=0 → confirm 掉，不重跑。
        schedules
            .confirm("notify-job", "2026-10-30T09:00")
            .await
            .expect("放弃");
        assert!(schedules
            .pending_claim("notify-job")
            .await
            .expect("读")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("notify-job")
                .await
                .expect("读")
                .as_deref(),
            Some("2026-10-30T09:00")
        );
    }

    /// list 读回全部行，按名字有序。
    #[tokio::test]
    async fn list_returns_every_row_sorted() {
        let (schedules, _dir) = db().await;
        for (name, cu, dl) in [("b", true, "outbox"), ("a", false, "notify")] {
            schedules.ensure(name, cu, dl).await.expect("登记");
        }
        let rows = schedules.list().await.expect("读回");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a");
        assert!(!rows[0].catch_up);
        assert_eq!(rows[1].delivery, "outbox");
    }

    /// import_legacy 只搬历史；DB 已有值时（调用方保证不调，但语义上）
    /// 它就是覆盖 —— 所以调度器端必须先查再导。
    #[tokio::test]
    async fn legacy_import_moves_only_history() {
        let (schedules, _dir) = db().await;
        schedules.ensure("old", true, "outbox").await.expect("登记");
        schedules
            .import_legacy("old", "2026-10-29T08:00")
            .await
            .expect("导入");
        assert_eq!(
            schedules.last_fired("old").await.expect("读").as_deref(),
            Some("2026-10-29T08:00")
        );
    }
}
