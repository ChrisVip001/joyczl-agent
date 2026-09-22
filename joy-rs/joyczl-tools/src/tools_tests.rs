//! 工具的测试：不联网，真的开临时库，验证行为和输出文案。

use std::sync::Arc;

use serde_json::json;

use crate::{handlers, Tool, ToolCtx, ToolRegistry};

/// 开一个临时 state.db，返回可直接用的 ctx。home 也在临时目录里 ——
/// calendar.ics / outbox 的测试需要真的写文件。
async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    ToolCtx {
        approval: None,
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
    }
}

#[tokio::test]
async fn default_registry_has_the_expected_tools() {
    let registry = handlers::build_default();
    assert_eq!(
        registry.names(),
        vec![
            "create_event",
            "create_skill",
            "current_time",
            "forget_note",
            "list_events",
            "list_memory",
            "manage_memory",
            "save_note",
            "search_memory",
            "search_web",
            "send_message"
        ]
    );
}

#[tokio::test]
async fn save_note_then_search_memory_round_trips() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "alex", "content": "喜欢早上的会议"}),
        )
        .await;
    // 输出必须说明东西落在哪儿 —— 模型要照着转述，不能虚报。
    assert!(out.contains("state.db"), "输出没说落点：{out}");
    assert!(out.contains("alex"), "输出没复述内容：{out}");

    let out = registry
        .execute(ctx.clone(), "search_memory", json!({"query": "早上"}))
        .await;
    assert!(out.contains("喜欢早上的会议"), "搜不回来：{out}");
}

#[tokio::test]
async fn forget_note_reports_when_nothing_matched() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(ctx.clone(), "forget_note", json!({"subject": "ghost"}))
        .await;
    assert!(out.contains("没有"), "0 条删除也该说清楚：{out}");

    registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "a", "content": "b"}),
        )
        .await;
    let out = registry
        .execute(ctx.clone(), "forget_note", json!({"subject": "a"}))
        .await;
    assert!(out.contains("1 条"), "应当报删了几条：{out}");
}

#[tokio::test]
async fn current_time_gives_a_usable_answer() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "current_time", json!({}))
        .await;
    // 星期与 UTC 偏移都在 —— 模型解析「明天上午」需要它们。
    assert!(out.contains("UTC"), "没有时区：{out}");
    assert!(
        out.contains("星期") || out.chars().any(|c| c.is_ascii_alphabetic()),
        "没有星期：{out}"
    );
}

#[tokio::test]
async fn unknown_tool_and_bad_args_become_text_not_errors() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry.execute(ctx.clone(), "nope", json!({})).await;
    assert!(out.starts_with("Error:"), "未知工具：{out}");
    assert!(out.contains("save_note"), "应当列出可用的工具：{out}");

    // 缺参数
    let out = registry
        .execute(ctx.clone(), "save_note", json!({"subject": "alex"}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("content"),
        "缺参数：{out}"
    );

    // 类型不对：现在由参数 schema 先挡下（比 handler 里的手写检查更早、
    // 也更具体 —— 报错里带字段路径）
    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": 42, "content": "x"}),
        )
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("subject") && out.contains("schema"),
        "类型错：{out}"
    );

    // 空字符串
    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "  ", "content": "x"}),
        )
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("不能是空"),
        "空串：{out}"
    );
}

#[tokio::test]
async fn list_memory_handles_an_empty_store() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "list_memory", json!({}))
        .await;
    assert!(out.contains("空的"), "空记忆要有引导性文案：{out}");
}

#[tokio::test]
async fn search_memory_with_junk_query_does_not_error() {
    let registry = handlers::build_default();
    let ctx = ctx().await;
    registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "a", "content": "b"}),
        )
        .await;
    for junk in ["???", "a:b", "\"", "*", "??? ..."] {
        let out = registry
            .execute(ctx.clone(), "search_memory", json!({"query": junk}))
            .await;
        assert!(out.contains("没有关于"), "{junk:?} → {out}");
    }
    // 空串是参数错误 —— 同样是文本，不是崩溃。
    let out = registry
        .execute(ctx.clone(), "search_memory", json!({"query": ""}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("不能是空"),
        "{out}"
    );
}

/// create_event：写库 + 写 ICS + 如实交代落点。幂等靠第二次调用验证 ——
/// 同标题同开始时间的事件绝不重复。
#[tokio::test]
async fn create_event_lands_in_db_and_ics_and_is_idempotent() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "与阿明开会", "start": "2026-07-14T09:00:00"}),
        )
        .await;
    assert!(out.contains("已创建"), "{out}");
    assert!(out.contains("本地日历"), "输出要说明落点：{out}");
    // 没给 end 默认 +1 小时。
    assert!(out.contains("2026-07-14T10:00"), "默认结束时间：{out}");

    let ics = std::fs::read_to_string(ctx.home.join("calendar.ics")).expect("ics 文件");
    assert!(ics.contains("SUMMARY:与阿明开会"));
    assert!(ics.contains("DTSTART:20260714T090000"), "{ics}");
    assert!(ics.ends_with("END:VCALENDAR\n"), "VCALENDAR 要闭合：{ics}");

    // 再来一遍：带秒的时间戳也归一到分钟精度 → 幂等命中。
    let out = registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "与阿明开会", "start": "2026-07-14T09:00"}),
        )
        .await;
    assert!(out.contains("已经存在"), "第二次应当报幂等：{out}");

    // 数据库里也只有一条。
    let rows = ctx.calendar.list(None, None, 100).await.expect("查库");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].end, "2026-07-14T10:00");
}

/// list_events：读得回来，且空结果要说明查过**哪个**日历 ——
/// 「你日历上没安排」只有在说清楚查了哪儿时才是诚实的。
#[tokio::test]
async fn list_events_reports_what_it_checked_when_empty() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(ctx.clone(), "list_events", json!({}))
        .await;
    assert!(out.contains("没有找到事件"), "{out}");
    assert!(out.contains("本地日历"), "要说明查了哪个日历：{out}");

    registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "牙医", "start": "2026-07-14T09:00", "attendees": "阿明"}),
        )
        .await;
    let out = registry
        .execute(
            ctx.clone(),
            "list_events",
            json!({"start": "2026-07-14", "end": "2026-07-14"}),
        )
        .await;
    assert!(out.contains("牙医"), "区间内的事件要列出来：{out}");
    assert!(out.contains("与 阿明"), "参与者要带上：{out}");

    // 区间外的不出现。
    let out = registry
        .execute(
            ctx.clone(),
            "list_events",
            json!({"start": "2026-08-01", "end": "2026-08-31"}),
        )
        .await;
    assert!(out.contains("没有找到事件"), "区间外不该出现：{out}");
}

/// send_message 只写 outbox 草稿，绝不发送 —— 输出里必须把这句话说满。
#[tokio::test]
async fn send_message_writes_a_draft_and_never_sends() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "send_message",
            json!({"to": "阿明 <aming@example.com>", "body": "周四见"}),
        )
        .await;
    assert!(out.contains("没有真的发送"), "必须明说没发：{out}");

    let dir = ctx.home.join("outbox");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("outbox 目录")
        .map(|e| e.expect("目录项"))
        .collect();
    assert_eq!(entries.len(), 1);
    let content = std::fs::read_to_string(entries.remove(0).path()).expect("草稿");
    assert!(
        content.starts_with("To: 阿明 <aming@example.com>\n\n周四见\n"),
        "{content}"
    );
}

/// manage_memory：按编号改与删，编号不存在就如实说。
#[tokio::test]
async fn manage_memory_updates_and_deletes_by_id() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "alex", "content": "喜欢咖啡"}),
        )
        .await;
    let fact = &ctx.facts.search("咖啡", 1).await.expect("查库")[0];

    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "update_fact", "id": fact.id, "content": "喜欢茶"}),
        )
        .await;
    assert!(out.contains("已把"), "{out}");
    assert!(
        ctx.facts.search("茶", 4).await.expect("查库")[0]
            .content
            .contains("茶"),
        "正文要真的改掉"
    );

    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "delete_fact", "id": fact.id}),
        )
        .await;
    assert!(out.contains("已删掉"), "{out}");
    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "delete_fact", "id": fact.id}),
        )
        .await;
    assert!(out.contains("没有编号"), "删第二次要如实说：{out}");

    // 不认识的 action 也是文本，不是错误 —— 现在由 schema 的 enum 先挡下
    // （handler 里那条兜底分支只在 schema 编译失败时才会走到）。
    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "nuke_everything", "id": 1}),
        )
        .await;
    assert!(out.starts_with("Error:") && out.contains("action"), "{out}");
}

/// create_skill：合法 slug 落盘成 SKILL.md，非法名字和重名都被拒 ——
/// 绝不悄悄覆盖已有技能。
#[tokio::test]
async fn create_skill_writes_a_valid_skill_md_and_refuses_collisions() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "create_skill",
            json!({
                "name": "Weekly Review",
                "description": "summarize the week every Monday morning",
                "body": "1. Pull episodes.\n2. Draft three bullets."
            }),
        )
        .await;
    assert!(out.contains("已创建"), "{out}");

    let skill = std::fs::read_to_string(
        ctx.home
            .join("skills")
            .join("weekly-review")
            .join("SKILL.md"),
    )
    .expect("SKILL.md");
    assert!(skill.starts_with("---\nname: weekly-review\n"), "{skill}");
    // 写完必须能被 loader 解析 —— 两边共用同一套校验。
    assert!(joyczl_memory::skills::parse_skill_text(&skill).is_some());

    // 重名拒绝。
    let out = registry
        .execute(
            ctx.clone(),
            "create_skill",
            json!({"name": "weekly-review", "description": "x", "body": "y"}),
        )
        .await;
    assert!(out.contains("已有"), "{out}");

    // 非法 slug（会变成路径的东西）拒绝。
    let out = registry
        .execute(
            ctx.clone(),
            "create_skill",
            json!({"name": "../evil", "description": "x", "body": "y"}),
        )
        .await;
    assert!(out.contains("slug"), "路径穿越要被拦下：{out}");
    assert!(
        !ctx.home.join("evil").exists()
            && !ctx
                .home
                .parent()
                .map(|p| p.join("evil"))
                .is_some_and(|p| p.exists()),
        "不该写出目录树之外的东西"
    );
}

// ---- 参数 schema 校验 --------------------------------------------------------

/// 类型不对：**handler 根本不该被调用**（否则会留下半个副作用），
/// 报错要指名道姓说是哪个字段。
#[tokio::test]
async fn a_wrong_typed_argument_is_caught_before_the_handler_runs() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": 123, "content": "x"}),
        )
        .await;

    assert!(out.starts_with("Error:"), "要保持 Error: 前缀：{out}");
    assert!(out.contains("schema"), "{out}");
    assert!(out.contains("subject"), "要说清是哪个字段：{out}");
    assert!(
        ctx.facts.recent(10, 0).await.expect("查库").is_empty(),
        "校验没过就不该落库"
    );
}

#[tokio::test]
async fn a_missing_required_argument_is_caught_by_the_schema() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(ctx.clone(), "save_note", json!({"subject": "alex"}))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("content"), "缺的那个字段要出现在报错里：{out}");
}

/// 远端（MCP）可能报上来一个坏 schema。那种工具**跳过校验照常执行** ——
/// 一个 schema 写坏了不该让工具直接不可用。
#[tokio::test]
async fn a_tool_with_a_broken_schema_still_runs() {
    let mut registry = ToolRegistry::new();
    registry.register(Tool {
        name: "broken_schema".to_string(),
        description: "schema 是垃圾的工具".to_string(),
        input_schema: json!("这不是一个 schema"),
        handler: Arc::new(|_ctx: ToolCtx, _args: serde_json::Value| {
            Box::pin(async { Ok("真的跑了".to_string()) })
        }),
    });

    let out = registry
        .execute(ctx().await, "broken_schema", json!({"whatever": true}))
        .await;
    assert_eq!(out, "真的跑了", "坏 schema 不该拦住执行：{out}");
}

/// 合规的参数照样通到 handler（校验不能误伤正常调用）。
#[tokio::test]
async fn valid_arguments_pass_the_schema() {
    let registry = handlers::build_default();
    let ctx = ctx().await;
    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "alex", "content": "喜欢早会"}),
        )
        .await;
    assert!(out.contains("已记住"), "{out}");
}
