//! state's integration tests: run against a real temporary SQLite file; in-memory won't do —
//! every connection in sqlx's pool gets its own independent `:memory:` database.

use sqlx::sqlite::SqlitePool;

use crate::{Chat, Episodes, Facts};

async fn temp_db(name: &str) -> SqlitePool {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join(name);
    let pool = crate::open(&path).await.expect("打开数据库");
    // TempDir deletes the directory when it leaves scope, but sqlite also creates -wal/-shm
    // files. The test process exits right away anyway; leaking beats a vanishing database.
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    pool
}

/// Duplicate facts don't get stored: return the existing row with "not new" handed over.
#[tokio::test]
async fn a_duplicate_fact_is_not_stored_twice() {
    let facts = Facts::new(temp_db("dedup.db").await);

    let (first, is_new) = facts.add("alex", "喜欢早会", "user", "user").await.unwrap();
    assert!(is_new, "第一次当然是新记的");

    // Exactly identical: not new, the existing row comes back.
    let (again, is_new) = facts.add("alex", "喜欢早会", "user", "user").await.unwrap();
    assert!(!is_new);
    assert_eq!(again.id, first.id);

    // Case and edge whitespace are not differences (model and consolidation formatting
    // drift); source and kind aren't either — it's still the same statement.
    let (spaced, is_new) = facts
        .add("alex", "  喜欢早会  ", "consolidation", "fact")
        .await
        .unwrap();
    assert!(!is_new, "首尾空白不该让它变成新的一条");
    assert_eq!(spaced.id, first.id);

    // Only genuinely different content counts as new.
    let (other, is_new) = facts
        .add("alex", "喜欢下午的会", "user", "user")
        .await
        .unwrap();
    assert!(is_new);
    assert!(other.id > first.id);

    assert_eq!(facts.recent(10, 0).await.unwrap().len(), 2, "库里就两条");
}

/// Measured calibrates estimated: ratio = measured / estimated, clamped to 0.5..=2.0.
#[tokio::test]
async fn context_observations_pair_measured_with_estimated() {
    let chat = Chat::new(temp_db("context.db").await);
    assert_eq!(
        chat.context_factor("s").await.unwrap(),
        None,
        "没有记录就不校准"
    );

    chat.observe_context("s", 1000, 800).await.unwrap();
    let factor = chat.context_factor("s").await.unwrap().unwrap();
    assert!(
        (factor - 1.25).abs() < 1e-9,
        "实测比估算大 → 1.25，实际 {factor}"
    );

    // Overwrite: only the latest matters (model/toolset changed, old ratio is void).
    chat.observe_context("s", 900, 1800).await.unwrap();
    let factor = chat.context_factor("s").await.unwrap().unwrap();
    assert!(factor < 1.0, "这次估多了：{factor}");

    // Zero values are not recorded (graph path skips loop; a ratio from 0 is absurd).
    chat.observe_context("zero", 0, 100).await.unwrap();
    assert_eq!(chat.context_factor("zero").await.unwrap(), None);

    // Extremes clamped: one anomalous request must not skew the budget.
    chat.observe_context("wild", 100_000, 10).await.unwrap();
    assert_eq!(chat.context_factor("wild").await.unwrap(), Some(2.0));
}

/// Kinds: recognized ones stored as-is; unknown always lands on `fact` (collapsed at the write entry).
#[tokio::test]
async fn fact_kinds_are_normalised_at_the_write_entry() {
    let facts = Facts::new(temp_db("kinds.db").await);
    facts
        .add("a", "写歪的类别", "user", "PREFERENCE")
        .await
        .unwrap();
    let rows = facts.recent(10, 0).await.unwrap();
    assert_eq!(rows[0].kind, "fact", "未知类别落兜底");

    facts
        .add("b", "正经类别", "user", "feedback")
        .await
        .unwrap();
    let rows = facts.recent(10, 0).await.unwrap();
    assert!(rows.iter().any(|row| row.kind == "feedback"));
    // Case-insensitive (the model may write "Feedback").
    facts
        .add("c", "大小写", "user", "  Project ")
        .await
        .unwrap();
    let rows = facts.recent(10, 0).await.unwrap();
    assert!(rows.iter().any(|row| row.kind == "project"));
}

/// Failed distillation **backs off**: the same bad rows must not be retried every turn.
#[tokio::test]
async fn failed_consolidation_rows_are_backed_off() {
    let chat = Chat::new(temp_db("backoff.db").await);
    chat.append_exchange("m", "r", "default", "cli", None)
        .await
        .unwrap();
    let rows = chat.unconsolidated().await.unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<i64> = rows.iter().map(|(id, _, _)| *id).collect();

    chat.mark_consolidation_failed(&ids).await.unwrap();
    assert_eq!(
        chat.unconsolidated().await.unwrap().len(),
        0,
        "退避期间捞不到"
    );

    // The rows remain (failure keeps logs) and are not marked consolidated.
    assert_eq!(chat.session_history("default").await.unwrap().len(), 1);
}

#[tokio::test]
async fn open_creates_the_schema() {
    let pool = temp_db("schema.db").await;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(&pool)
            .await
            .expect("查表名");
    for expected in ["facts", "facts_fts", "episodes", "episodes_fts", "chat_log"] {
        assert!(
            tables.contains(&expected.to_string()),
            "缺表 {expected}：{tables:?}"
        );
    }
}

#[tokio::test]
async fn facts_search_finds_by_keyword() {
    let pool = temp_db("facts.db").await;
    let facts = Facts::new(pool);
    facts
        .add("alex", "Alex prefers morning meetings", "user", "user")
        .await
        .unwrap();
    facts
        .add("project", "The Acme demo is on Friday", "user", "user")
        .await
        .unwrap();

    let hit = facts.search("morning", 4).await.unwrap();
    assert_eq!(hit.len(), 1, "应当只命中一条：{hit:?}");
    assert_eq!(hit[0].subject, "alex");

    let hit = facts.search("acme", 4).await.unwrap();
    assert_eq!(hit[0].content, "The Acme demo is on Friday");
}

#[tokio::test]
async fn search_never_errors_on_junk_input() {
    let pool = temp_db("junk.db").await;
    let facts = Facts::new(pool);
    facts
        .add("alex", "Alex prefers morning meetings", "user", "user")
        .await
        .unwrap();

    // The retrieval gate is fail-open: if any of these error the SQL, it degrades to "search every turn".
    for junk in ["", "???", "\"", "a:b", "(unclosed", "*", "alex OR", "---"] {
        let out = facts.search(junk, 4).await;
        assert!(out.is_ok(), "{junk:?} 让检索报错了：{:?}", out.err());
    }
}

#[tokio::test]
async fn deleting_a_fact_keeps_the_index_in_sync() {
    let pool = temp_db("delete.db").await;
    let facts = Facts::new(pool);
    facts
        .add("alex", "Alex prefers morning meetings", "user", "user")
        .await
        .unwrap();
    assert_eq!(facts.search("morning", 4).await.unwrap().len(), 1);

    let removed = facts.forget_subject("alex").await.unwrap();
    assert_eq!(removed, 1);
    // Without the trigger sync, this would fetch a ghost row pointing at the deleted line.
    assert!(
        facts.search("morning", 4).await.unwrap().is_empty(),
        "FTS 索引没跟着删"
    );
}

#[tokio::test]
async fn episodes_search_and_recent() {
    let pool = temp_db("episodes.db").await;
    let eps = Episodes::new(pool);
    eps.add("2026-09-01", "planned the Acme demo with Alex")
        .await
        .unwrap();
    eps.add("2026-09-10", "booked a catch-up on Friday")
        .await
        .unwrap();

    assert_eq!(eps.search("acme", 4).await.unwrap().len(), 1);

    let recent = eps.recent(10).await.unwrap();
    assert_eq!(recent[0].happened_at, "2026-09-10", "最近的应当排最前");
}

#[tokio::test]
async fn chat_log_round_trip_and_consolidation_marking() {
    let pool = temp_db("chat.db").await;
    let chat = Chat::new(pool);
    chat.append_exchange("hi", "hello", "default", "cli", None)
        .await
        .unwrap();
    chat.append_exchange(
        "remember alex likes mornings",
        "noted",
        "default",
        "cli",
        Some("{\"iterations\":1}"),
    )
    .await
    .unwrap();

    let pending = chat.unconsolidated().await.unwrap();
    assert_eq!(pending.len(), 4, "4 行（2 轮 × user/assistant）");

    // Claiming marks: after claiming there must be no pending rows left.
    let claimed = chat.claim_unconsolidated().await.unwrap();
    assert_eq!(claimed.len(), 4, "认领拿回自己认到的那 4 行");
    assert!(chat.unconsolidated().await.unwrap().is_empty());

    let history = chat.session_history("default").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].0, "hi");
}

#[tokio::test]
async fn sessions_are_just_labels_on_the_chat_log() {
    let pool = temp_db("sessions.db").await;
    let chat = Chat::new(pool);
    chat.append_exchange("first topic", "a", "s1", "cli", None)
        .await
        .unwrap();
    chat.append_exchange("second topic", "b", "s2", "cli", None)
        .await
        .unwrap();
    chat.append_exchange("more", "c", "s2", "cli", None)
        .await
        .unwrap();

    let sessions = chat.sessions().await.unwrap();
    assert_eq!(sessions.len(), 2);
    // s2 has more messages, sorts first by last_at (same-second inserts; messages breaks the tie)
    let s2 = sessions.iter().find(|s| s.id == "s2").expect("s2 存在");
    assert_eq!(s2.messages, 4, "s2 有 2 轮 = 4 行");
    assert_eq!(s2.title, "second topic", "标题应当是该会话第一条用户消息");
}

#[tokio::test]
async fn messages_page_backwards_from_the_newest() {
    let pool = temp_db("messages.db").await;
    let chat = Chat::new(pool);
    for round in 1..=2 {
        chat.append_exchange(
            &format!("问 {round}"),
            &format!("答 {round}"),
            "default",
            "cli",
            None,
        )
        .await
        .unwrap();
    }
    chat.append_exchange(
        "问 3",
        "答 3",
        "default",
        "cli",
        Some(r#"{"iterations":7}"#),
    )
    .await
    .unwrap();
    chat.append_exchange("别的会话", "不该混进来", "other", "cli", None)
        .await
        .unwrap();

    // The newest page, **newest first** — direction chosen by usage: latest page, then older.
    let page = chat.messages("default", None, 2).await.unwrap();
    let contents: Vec<&str> = page.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["答 3", "问 3"], "最新的那条该在最前");
    assert_eq!(page[0].role, "assistant");

    // Telemetry passed out as-is (JSON text in the column; parsing shape is the exit layer's
    // business); user rows don't have it.
    assert_eq!(page[0].meta.as_deref(), Some(r#"{"iterations":7}"#));
    assert!(page[1].meta.is_none());

    // Walk back from the oldest id: only earlier rows, not the two already seen.
    let older = chat
        .messages("default", Some(page[1].id), 10)
        .await
        .unwrap();
    let contents: Vec<&str> = older.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["答 2", "问 2", "答 1", "问 1"]);

    // Sessions are a hard boundary.
    let other = chat.messages("other", None, 10).await.unwrap();
    assert_eq!(other.len(), 2);
    assert_eq!(other[0].content, "不该混进来");
}

#[tokio::test]
async fn chinese_search_works_character_wise() {
    let pool = temp_db("cjk.db").await;
    let facts = Facts::new(pool);
    facts
        .add("阿明", "阿明喜欢早上的会议", "user", "user")
        .await
        .unwrap();

    let hit = facts.search("早上", 4).await.unwrap();
    assert_eq!(hit.len(), 1, "中文检索没命中：{hit:?}");
}

// ---- The rolling summary's monotonic guard ----------------------------------

/// The summary **only rolls forward**: a new one isn't overwritten by an old one, or vice versa.
///
/// Without the guard (`ON CONFLICT … DO UPDATE` without WHERE), concurrent compaction
/// decides the winner by **write order** — the user may get an older summary, no anomaly shown.
#[tokio::test]
async fn a_rollup_only_moves_forward() {
    let chat = Chat::new(temp_db("rollup.db").await);

    // First: covers 5 turns.
    assert!(chat.save_rollup("s", 5, "前五轮的摘要").await.unwrap());
    // The newer one (10 turns) lands.
    assert!(chat.save_rollup("s", 10, "前十轮的摘要").await.unwrap());
    let (covered, summary) = chat.load_rollup("s").await.unwrap().expect("有摘要");
    assert_eq!(covered, 10);
    assert_eq!(summary, "前十轮的摘要");

    // An older one (7 turns) is blocked: the DB's copy is still newer.
    assert!(
        !chat.save_rollup("s", 7, "第七轮的摘要").await.unwrap(),
        "回退该被挡下"
    );
    let (covered, summary) = chat.load_rollup("s").await.unwrap().expect("有摘要");
    assert_eq!(covered, 10, "没被旧的盖掉");
    assert_eq!(summary, "前十轮的摘要");

    // The same covered_turns is not an update (not strictly greater) — rewriting is pointless.
    assert!(!chat.save_rollup("s", 10, "又一版").await.unwrap());
}

/// The concurrency knobs are really configured: **10 seconds** lock wait + WAL.
///
/// These two PRAGMAs decide "multiple processes writing at once: queue or 'database is
/// locked'". They once silently stopped working (skipped in a refactor), so the values are pinned.
#[tokio::test]
async fn the_pool_waits_for_locks_and_uses_wal() {
    let pool = temp_db("pragmas.db").await;

    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(busy_timeout, 10_000, "等锁该是 10 秒（SQLite 官方建议值）");

    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(journal.to_lowercase(), "wal", "读不该被写堵住");
}
