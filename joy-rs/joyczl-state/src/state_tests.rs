//! state 的集成测试：真的开一个临时 SQLite 文件跑，in-memory 不行 ——
//! sqlx 的连接池里每个连接会拿到各自独立的 `:memory:` 库。

use sqlx::sqlite::SqlitePool;

use crate::{Chat, Episodes, Facts};

async fn temp_db(name: &str) -> SqlitePool {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join(name);
    let pool = crate::open(&path).await.expect("打开数据库");
    // TempDir 一出作用域就删目录，而 sqlite 还会建 -wal/-shm 文件。
    // 测试进程反正马上就退出，让它泄漏比让数据库消失更安全。
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    pool
}

/// 类别：认得的按原样存，不认得的一律落 `fact`（收敛在写入口）。
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
    // 大小写不敏感（模型可能写成 Feedback）。
    facts
        .add("c", "大小写", "user", "  Project ")
        .await
        .unwrap();
    let rows = facts.recent(10, 0).await.unwrap();
    assert!(rows.iter().any(|row| row.kind == "project"));
}

/// 提炼失败会**退避**：同一批坏行不该每轮都被重试一遍。
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

    // 行还在（失败不丢日志），而且没有被标成已提炼。
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

    // 检索门失败开放：这些输入一旦让 SQL 报错，就会退化成「每轮都检索」。
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
    // 触发器没同步的话，这里会捞到一条指向已删除行的幽灵结果。
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

    let ids: Vec<i64> = pending.iter().map(|(id, _, _)| *id).collect();
    chat.mark_consolidated(&ids).await.unwrap();
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
    // s2 有更多消息，按 last_at 排序后应当在最前（同一秒内插入，靠 messages 兜底判断）
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

    // 最新的一页，**最新的在最前** —— 方向由用法决定：先拿最近一页，再往更早走。
    let page = chat.messages("default", None, 2).await.unwrap();
    let contents: Vec<&str> = page.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["答 3", "问 3"], "最新的那条该在最前");
    assert_eq!(page[0].role, "assistant");

    // 遥测原样带出来（列里的 JSON 文本，解析成什么形状是出口那一层的事）；
    // user 行上没有它。
    assert_eq!(page[0].meta.as_deref(), Some(r#"{"iterations":7}"#));
    assert!(page[1].meta.is_none());

    // 顺着最老那条的 id 往回：只拿到更早的，不含已经看过的那两行。
    let older = chat
        .messages("default", Some(page[1].id), 10)
        .await
        .unwrap();
    let contents: Vec<&str> = older.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["答 2", "问 2", "答 1", "问 1"]);

    // 会话是硬边界。
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
