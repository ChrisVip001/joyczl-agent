//! state's integration tests: run against a real temporary SQLite file; in-memory won't do —
//! every connection in sqlx's pool gets its own independent `:memory:` database.

use sqlx::sqlite::SqlitePool;

use crate::{Chat, Episodes, Facts, NewTask, TaskStatus, Tasks};

/// 任务卡的取值：开着的那条优先，但**完成后也要看得见** —— 验收结论与交付物正是
/// 那时候才值得读，用 `active` 会让它在变得有用的同一刻消失。
#[tokio::test]
async fn latest_task_prefers_open_but_never_hides_a_finished_one() {
    let pool = temp_db("task_latest").await;
    let tasks = Tasks::new(pool);

    tasks
        .create(bare_task("old-done", "s1", "做完了"))
        .await
        .expect("建任务");
    tasks
        .finish("old-done", TaskStatus::Done)
        .await
        .expect("收尾");
    tasks
        .create(bare_task("new-open", "s1", "还在跑"))
        .await
        .expect("建任务");

    let latest = tasks.latest("s1").await.expect("查").expect("该有一条");
    assert_eq!(latest.id, "new-open", "开着的那条优先");

    tasks
        .finish("new-open", TaskStatus::Done)
        .await
        .expect("收尾");
    let latest = tasks
        .latest("s1")
        .await
        .expect("查")
        .expect("完成态也要读得到");
    assert_eq!(latest.id, "new-open", "做完的任务不该从卡片上消失");
    assert_eq!(latest.status, TaskStatus::Done);

    assert!(
        tasks.latest("s2").await.expect("查").is_none(),
        "别的会话看不到"
    );
}

fn bare_task<'a>(id: &'a str, session: &'a str, title: &'a str) -> NewTask<'a> {
    NewTask {
        id,
        session_id: Some(session),
        title,
        goal_condition: None,
        acceptance_json: "[]",
        budget_json: "{}",
        cwd: None,
        max_rounds: 3,
        tier: None,
    }
}

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
    assert_eq!(sessions.len(), 2, "写消息就会把会话行建出来，不用先 new");
    let s2 = sessions.iter().find(|s| s.id == "s2").expect("s2 存在");
    assert_eq!(s2.messages, 4, "s2 有 2 轮 = 4 行");
    assert_eq!(s2.title, "second topic", "标题应当是该会话第一条用户消息");
}

/// 0018：空会话也是会话 —— 它必须出现在列表里，带着自己的名字和工作目录。
#[tokio::test]
async fn an_empty_session_is_listed_with_its_name_and_working_directory() {
    let pool = temp_db("sessions_empty.db").await;
    let chat = Chat::new(pool);
    chat.create_session("s1", Some("发版检查"), Some("/tmp/joy-work"))
        .await
        .expect("建会话");

    let all = chat.sessions().await.expect("列表");
    assert_eq!(all.len(), 1, "一条消息都没有，但它存在");
    assert_eq!(all[0].title, "发版检查");
    assert_eq!(all[0].messages, 0);
    assert_eq!(all[0].cwd.as_deref(), Some("/tmp/joy-work"));
}

/// 改名与清空：`Some("")` 是「清掉」，标题回退第一条用户消息，工作目录回退「没选」。
#[tokio::test]
async fn update_renames_and_clears_back_to_the_first_message() {
    let pool = temp_db("sessions_update.db").await;
    let chat = Chat::new(pool);
    chat.append_exchange("第一句", "答", "s1", "cli", None)
        .await
        .expect("写一轮");

    chat.update_session("s1", Some("重新起的名字"), Some("/tmp/w"))
        .await
        .expect("改名");
    let row = chat.session("s1").await.expect("查").expect("该有一条");
    assert_eq!(row.title, "重新起的名字");
    assert_eq!(row.cwd.as_deref(), Some("/tmp/w"));

    chat.update_session("s1", Some(""), Some(""))
        .await
        .expect("清空");
    let row = chat.session("s1").await.expect("查").expect("该有一条");
    assert_eq!(row.title, "第一句", "清空后回退首条用户消息");
    assert_eq!(row.cwd, None, "清空后就是没选目录");
}

/// `session/new` 是幂等的：同一个 id 再叫一次，不能把人起的名字改掉。
#[tokio::test]
async fn creating_twice_keeps_the_name_that_is_already_there() {
    let pool = temp_db("sessions_idem.db").await;
    let chat = Chat::new(pool);
    chat.create_session("s1", Some("原名"), None)
        .await
        .expect("建");
    chat.create_session("s1", Some("另一个名字"), Some("/tmp/x"))
        .await
        .expect("再建一次");

    let row = chat.session("s1").await.expect("查").expect("该有一条");
    assert_eq!(row.title, "原名");
    assert_eq!(row.cwd.as_deref(), Some("/tmp/x"), "空字段被补上");
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
/// Checkpoints: every boundary overwrites the previous one (only the newest working memory
/// is worth resuming from); reconciliation hands out exactly the still-`open` rows.
#[tokio::test]
async fn checkpoints_keep_the_newest_working_memory_open() {
    use crate::Checkpoints;

    let checkpoints = Checkpoints::new(temp_db("checkpoints.db").await);

    // First boundary lands, then a second one overwrites it.
    checkpoints
        .save(
            "t1",
            "s",
            "帮我排三天会",
            r#"[{"m":1}]"#,
            2,
            r#"{"input_tokens":100}"#,
            Some("turn-t1"),
        )
        .await
        .unwrap();
    checkpoints
        .save(
            "t1",
            "s",
            "帮我排三天会",
            r#"[{"m":2}]"#,
            5,
            r#"{"input_tokens":300}"#,
            Some("turn-t1"),
        )
        .await
        .unwrap();

    let rows = checkpoints.open_rows().await.unwrap();
    assert_eq!(rows.len(), 1, "同一个回合只有一行");
    assert_eq!(rows[0].iterations, 5, "只留最新的");
    assert!(
        rows[0].messages_json.contains(r#"{"m":2}"#),
        "工作记忆是新版"
    );
    assert_eq!(rows[0].status, "open");

    // Done: out of the reconciliation list, but the ledger row stays.
    checkpoints.finish("t1", "done").await.unwrap();
    assert!(checkpoints.open_rows().await.unwrap().is_empty());

    // An abandoned one (human interrupt) is not resumed either.
    checkpoints
        .save("t2", "s", "长任务", "[]", 1, "{}", None)
        .await
        .unwrap();
    checkpoints.finish("t2", "abandoned").await.unwrap();
    assert!(checkpoints.open_rows().await.unwrap().is_empty());

    // Two open checkpoints from two turns: reconciliation sees both, oldest first.
    checkpoints
        .save("t3", "s", "先来的", "[]", 1, "{}", None)
        .await
        .unwrap();
    checkpoints
        .save("t4", "s", "后来的", "[]", 1, "{}", None)
        .await
        .unwrap();
    let rows = checkpoints.open_rows().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].turn_id, "t3", "先来先续");
}

/// 0.16 follow-up：队列是**按会话**的先进先出，取走即消失（claim-then-run）。
#[tokio::test]
async fn the_follow_up_queue_is_fifo_per_session_and_pops_once() {
    let pool = temp_db("session_queue").await;
    let queue = crate::Queue::new(pool);

    assert_eq!(queue.push("s1", "第一句").await.expect("排"), 1);
    assert_eq!(queue.push("s1", "第二句").await.expect("排"), 2);
    assert_eq!(
        queue.push("s2", "别的会话").await.expect("排"),
        1,
        "按会话各算各的"
    );

    let first = queue.pop("s1").await.expect("取").expect("该有一条");
    assert_eq!(first.message, "第一句", "先进先出");
    let second = queue.pop("s1").await.expect("取").expect("该有一条");
    assert_eq!(second.message, "第二句");
    assert!(
        queue.pop("s1").await.expect("取").is_none(),
        "取走就不再来一次"
    );

    assert_eq!(queue.count("s2").await.expect("数"), 1, "别的会话不受影响");
    assert_eq!(
        queue.sessions_with_pending().await.expect("列"),
        vec!["s2".to_string()],
        "启动对账要知道还欠谁一条"
    );
}

// ---- 会话账本 ---------------------------------------------------------------

fn entry(kind: &'static str, text: &str) -> crate::NewEntry {
    crate::NewEntry::exact(kind, format!(r#"{{"text":"{text}"}}"#))
}

/// 一轮写进去、按原序读回来 —— 重建的全部前提。
#[tokio::test]
async fn a_turn_round_trips_in_append_order() {
    let ledger = crate::Ledger::new(temp_db("ledger_roundtrip").await);
    let entries = vec![
        entry("user", "帮我看看这个报错"),
        entry("assistant", "先读日志"),
        entry("tool_use", "read_file"),
        entry("tool_result", "no such file"),
    ];

    let digest = ledger
        .append("s1", "t1", &entries, "帮我看看这个报错")
        .await
        .expect("写账");
    assert_eq!(digest.len(), 64, "摘要是 sha256 的小写十六进制");

    let rows = ledger.entries("s1", "t1").await.expect("读账");
    assert_eq!(
        rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
        vec!["user", "assistant", "tool_use", "tool_result"],
        "顺序就是发生顺序"
    );
    assert_eq!(rows[3].payload, r#"{"text":"no such file"}"#);
    assert!(
        ledger.verify("s1", "t1").await.expect("验"),
        "刚写完就该验得过"
    );
}

/// 同一个 turn 重写一次是幂等的：崩溃重试不能让记录翻倍。
#[tokio::test]
async fn appending_the_same_turn_twice_writes_once() {
    let ledger = crate::Ledger::new(temp_db("ledger_idempotent").await);
    let entries = vec![entry("user", "只此一句")];

    let first = ledger
        .append("s1", "t1", &entries, "只此一句")
        .await
        .expect("第一次");
    let second = ledger
        .append("s1", "t1", &entries, "只此一句")
        .await
        .expect("第二次");
    assert_eq!(first, second, "第二次返回第一次的摘要，不再写");
    assert_eq!(
        ledger.entries("s1", "t1").await.expect("读").len(),
        1,
        "只有一行"
    );
    assert_eq!(ledger.count().await.expect("数"), 1, "一轮只记一次");
}

/// 内容被改一个字节，轮摘要就对不上 —— 这就是「可核验」的意思。
#[tokio::test]
async fn editing_a_row_breaks_the_turn_digest() {
    let ledger = crate::Ledger::new(temp_db("ledger_tamper").await);
    ledger
        .append("s1", "t1", &[entry("user", "原始内容")], "原始内容")
        .await
        .expect("写账");
    assert!(ledger.verify("s1", "t1").await.expect("验"));

    // 模拟篡改：直接改库里的一行（应用层没有这条路，这正是要证明的）。
    sqlx::query("UPDATE ledger SET payload = '{\"text\":\"改过了\"}' WHERE turn_id = 't1'")
        .execute(ledger.pool())
        .await
        .expect("篡改");

    assert!(
        !ledger.verify("s1", "t1").await.expect("验"),
        "内容变了，轮摘要必须对不上"
    );
}

/// `chat_log` 回填的行标 `folded`：那一轮的工具输出本来就只剩 200 字符。
#[tokio::test]
async fn a_folded_turn_is_declared_as_such() {
    let ledger = crate::Ledger::new(temp_db("ledger_folded").await);
    ledger
        .append(
            "s1",
            "t0",
            &[
                crate::NewEntry::folded("user", r#"{"text":"旧的一句话"}"#),
                crate::NewEntry::folded(
                    "assistant",
                    r#"{"text":"旧回复\n[tools used: read_file(…)]"}"#,
                ),
            ],
            "旧的一句话",
        )
        .await
        .expect("回填");

    let turns = ledger.turns("s1", 10).await.expect("列");
    assert_eq!(turns[0].fidelity, "folded", "保真度如实标注");
    assert_eq!(turns[0].events, 2);
    assert_eq!(turns[0].preview, "旧的一句话");
}

/// 待整理的一轮只会被一个赢家拿走（两个进程共享同一个库时的唯一保证）。
#[tokio::test]
async fn claiming_pending_turns_has_one_winner() {
    let ledger = crate::Ledger::new(temp_db("ledger_claim").await);
    for turn in ["t1", "t2"] {
        ledger
            .append("s1", turn, &[entry("user", "…")], "…")
            .await
            .expect("写账");
    }

    assert_eq!(ledger.pending(10).await.expect("待办").len(), 2);
    let claimed = ledger.claim_pending(10).await.expect("认领");
    assert_eq!(claimed.len(), 2, "一次认领全部");
    assert!(
        ledger.pending(10).await.expect("待办").is_empty(),
        "认领走了就不再待办"
    );
    assert!(
        ledger.claim_pending(10).await.expect("再认领").is_empty(),
        "别人拿不到"
    );

    // 失败交还，并按退避排到未来 —— 不会立刻被重试。
    ledger.mark_failed("s1", "t1").await.expect("交还");
    assert!(
        ledger.pending(10).await.expect("待办").is_empty(),
        "退避期内不该重新出现"
    );
}

/// 新事实自带新鲜度：写进去就有 `confirm_count=1`，重复写会加分而不是多一行。
#[tokio::test]
async fn a_fact_carries_its_freshness() {
    let facts = Facts::new(temp_db("facts_freshness").await);
    let (fact, is_new) = facts
        .add("alex", "喜欢早会", "user", "user")
        .await
        .expect("写");
    assert!(is_new);

    let meta = facts.meta(fact.id).await.expect("查").expect("有");
    assert_eq!(meta.confirm_count, 1);
    assert!(meta.last_confirmed_at.is_some(), "写入即确认一次");
    assert!(meta.page_path.is_none(), "还没镜像到明文库");

    let (again, is_new) = facts
        .add("alex", "喜欢早会", "user", "user")
        .await
        .expect("再写");
    assert!(!is_new, "重复不新增行");
    assert_eq!(again.id, fact.id);
    let meta = facts.meta(fact.id).await.expect("查").expect("有");
    assert_eq!(meta.confirm_count, 2, "再观察到一次就是更可信一点");

    facts
        .set_page(fact.id, "memory/alex.md", "abc123")
        .await
        .expect("记页");
    let meta = facts.meta(fact.id).await.expect("查").expect("有");
    assert_eq!(meta.page_path.as_deref(), Some("memory/alex.md"));
    assert_eq!(meta.digest.as_deref(), Some("abc123"));
}

/// 回填：把 chat_log 的折叠视图变成账本行，并如实标成 folded。
#[tokio::test]
async fn backfilling_marks_history_as_folded() {
    let pool = temp_db("ledger_backfill").await;
    let chat = Chat::new(pool.clone());
    chat.append_exchange_for("旧问题一", "旧回答一", "默认", "app-server", None, None)
        .await
        .expect("写一问一答");
    chat.append_exchange_for("旧问题二", "旧回答二", "默认", "app-server", None, None)
        .await
        .expect("写第二问");

    let report = crate::ledger::backfill_from_chat(&pool)
        .await
        .expect("回填");
    assert_eq!(report.sessions, 1);
    assert_eq!(report.turns, 2, "一问一答算一轮");
    assert_eq!(report.rows, 4, "两句用户 + 两句助手");

    let turns = crate::Ledger::new(pool.clone())
        .turns("默认", 10)
        .await
        .expect("列账");
    assert_eq!(turns.len(), 2);
    assert!(
        turns.iter().all(|turn| turn.fidelity == "folded"),
        "历史只能是折叠保真度"
    );

    // 再跑一次是幂等的：轮次翻倍就说明去重坏了。
    let again = crate::ledger::backfill_from_chat(&pool)
        .await
        .expect("再回填");
    assert_eq!(again.turns, 2);
    assert_eq!(
        crate::Ledger::new(pool).count().await.expect("数"),
        2,
        "回填不该写出第二轮"
    );
}
