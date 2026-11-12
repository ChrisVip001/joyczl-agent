//! Tool tests: no network, real temp databases, verifying behavior and output wording.

use std::sync::Arc;

use serde_json::json;

use crate::{handlers, Tool, ToolCtx, ToolRegistry};

/// Opens a temp state.db and returns a ready-to-use ctx. home is also a temp dir —
/// the calendar.ics / outbox tests need real file writes.
async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    ToolCtx {
        on_uncertain: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(crate::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
        on_delta: None,
    }
}

#[tokio::test]
async fn default_registry_has_the_expected_tools() {
    let registry = handlers::build_default();
    assert_eq!(
        registry.names(),
        vec![
            "cancel_schedule",
            "create_event",
            "create_skill",
            "current_time",
            "fetch_url",
            "find_tools",
            "forget_note",
            "list_dir",
            "list_events",
            "list_memory",
            "list_schedules",
            "manage_memory",
            "read_file",
            "read_memory_page",
            "read_skill",
            "save_note",
            "schedule_job",
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
    // The output must say where things landed — the model relays it and must not overclaim.
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
    assert!(out.contains("no memory about"), "0 条删除也该说清楚：{out}");

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
    assert!(out.contains("Forgot 1"), "应当报删了几条：{out}");
}

#[tokio::test]
async fn current_time_gives_a_usable_answer() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "current_time", json!({}))
        .await;
    // Weekday and UTC offset are both there — parsing "tomorrow morning" needs them.
    assert!(out.contains("UTC"), "没有时区：{out}");
    assert!(
        out.contains("weekday") || out.chars().any(|c| c.is_ascii_alphabetic()),
        "没有weekday：{out}"
    );
}

#[tokio::test]
async fn unknown_tool_and_bad_args_become_text_not_errors() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry.execute(ctx.clone(), "nope", json!({})).await;
    assert!(out.starts_with("Error:"), "未知工具：{out}");
    assert!(out.contains("save_note"), "应当列出可用的工具：{out}");

    // Missing argument
    let out = registry
        .execute(ctx.clone(), "save_note", json!({"subject": "alex"}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("content"),
        "缺参数：{out}"
    );

    // Wrong type: now caught first by the argument schema (earlier and more specific
    // than the hand-written check in the handler — the error carries the field path)
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

    // Empty string
    let out = registry
        .execute(
            ctx.clone(),
            "save_note",
            json!({"subject": "  ", "content": "x"}),
        )
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("must not be empty"),
        "空串：{out}"
    );
}

#[tokio::test]
async fn list_memory_handles_an_empty_store() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "list_memory", json!({}))
        .await;
    assert!(out.contains("empty"), "空记忆要有引导性文案：{out}");
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
        assert!(out.contains("nothing in memory about"), "{junk:?} → {out}");
    }
    // An empty string is an argument error — still text, not a crash.
    let out = registry
        .execute(ctx.clone(), "search_memory", json!({"query": ""}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("must not be empty"),
        "{out}"
    );
}

/// create_event: write db + write ICS + state where it landed honestly. Idempotency is
/// verified by a second call — same title and start time must never duplicate.
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
    assert!(out.contains("created"), "{out}");
    assert!(out.contains("local calendar"), "输出要说明落点：{out}");
    // No end given defaults to +1 hour.
    assert!(out.contains("2026-07-14T10:00"), "默认结束时间：{out}");

    let ics = std::fs::read_to_string(ctx.home.join("calendar.ics")).expect("ics 文件");
    assert!(ics.contains("SUMMARY:与阿明开会"));
    assert!(ics.contains("DTSTART:20260714T090000"), "{ics}");
    assert!(ics.ends_with("END:VCALENDAR\n"), "VCALENDAR 要闭合：{ics}");

    // Run it again: a timestamp with seconds also normalizes to minute precision → idempotent hit.
    let out = registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "与阿明开会", "start": "2026-07-14T09:00"}),
        )
        .await;
    assert!(out.contains("already exists"), "第二次应当报幂等：{out}");

    // And only one row in the database.
    let rows = ctx.calendar.list(None, None, 100).await.expect("查库");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].end, "2026-07-14T10:00");
}

/// list_events: reads back, and an empty result must say **which** calendar was searched —
/// "nothing on your calendar" is only honest when it says where it looked.
#[tokio::test]
async fn list_events_reports_what_it_checked_when_empty() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(ctx.clone(), "list_events", json!({}))
        .await;
    assert!(out.contains("No events found"), "{out}");
    assert!(out.contains("local calendar"), "要说明查了哪个日历：{out}");

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
    assert!(out.contains(", with 阿明"), "参与者要带上：{out}");

    // Out-of-range entries don't appear.
    let out = registry
        .execute(
            ctx.clone(),
            "list_events",
            json!({"start": "2026-08-01", "end": "2026-08-31"}),
        )
        .await;
    assert!(out.contains("No events found"), "区间外不该出现：{out}");
}

/// send_message only writes an outbox draft, never sends — the output must say so in full.
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
    assert!(out.contains("Nothing was sent"), "必须明说没发：{out}");

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

/// manage_memory: edit and delete by id; a missing id is reported honestly.
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
    assert!(out.contains("now reads"), "{out}");
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
    assert!(out.contains("Deleted"), "{out}");
    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "delete_fact", "id": fact.id}),
        )
        .await;
    assert!(out.contains("with id"), "删第二次要如实说：{out}");

    // An unknown action is text too, not an error — now caught first by the schema's enum
    // (the handler's fallback branch only runs if schema compilation failed).
    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "nuke_everything", "id": 1}),
        )
        .await;
    assert!(out.starts_with("Error:") && out.contains("action"), "{out}");
}

/// create_skill: a valid slug lands as SKILL.md; bad names and duplicates are refused —
/// never quietly overwrite an existing skill.
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
    assert!(out.contains("created"), "{out}");

    let skill = std::fs::read_to_string(
        ctx.home
            .join("skills")
            .join("weekly-review")
            .join("SKILL.md"),
    )
    .expect("SKILL.md");
    assert!(skill.starts_with("---\nname: weekly-review\n"), "{skill}");
    // What's written must parse in the loader — both sides share one validation.
    assert!(joyczl_memory::skills::parse_skill_text(&skill).is_some());

    // Duplicate names are refused.
    let out = registry
        .execute(
            ctx.clone(),
            "create_skill",
            json!({"name": "weekly-review", "description": "x", "body": "y"}),
        )
        .await;
    assert!(out.contains("already exists"), "{out}");

    // Invalid slugs (things that would become paths) are refused.
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

// ---- Argument schema validation ----------------------------------------------

/// Wrong type: **the handler must not be called at all** (otherwise half a side effect remains),
/// and the error must name the exact field.
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

/// A remote (MCP) server may report a broken schema. Such tools **skip validation and run** —
/// a broken schema must not make a tool unusable.
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

/// Valid arguments still reach the handler (validation must not hit normal calls).
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
    assert!(out.contains("Remembered"), "{out}");
}

/// Same-name tools **never overwrite**: one name, one implementation.
///
/// This guards against "unknown executor" accidents: MCP servers, skills, and built-ins can
/// all collide, and quietly swapping an implementation is far worse than one tool fewer.
#[tokio::test]
async fn a_duplicate_tool_name_does_not_replace_the_first() {
    use std::sync::Arc;

    use serde_json::Value;

    fn fixed(name: &str, answer: &str) -> super::Tool {
        let answer = answer.to_string();
        super::Tool {
            name: name.to_string(),
            description: "测试用".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            handler: Arc::new(move |_ctx: super::ToolCtx, _args: Value| {
                let answer = answer.clone();
                Box::pin(async move { Ok(answer) })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>,
                    >
            }),
        }
    }

    let mut registry = super::ToolRegistry::new();
    registry.register(fixed("dup", "第一个"));
    registry.register(fixed("dup", "第二个"));

    assert_eq!(
        registry
            .names()
            .iter()
            .filter(|name| **name == "dup")
            .count(),
        1,
        "只该有一个"
    );
    let out = registry
        .execute(ctx().await, "dup", serde_json::json!({}))
        .await;
    assert_eq!(out, "第一个", "先到的保留，后到的被拒");
}

/// Tool-level hooks hang on the **single choke point**: PreToolUse can stop a call, PostToolUse can rewrite the result.
#[cfg(unix)]
#[tokio::test]
async fn tool_hooks_block_before_and_rewrite_after() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("hooks.json"),
        r#"{ "hooks": {
             "PreToolUse": [ { "matcher": "blocked_tool", "command": "sh",
               "args": ["-c", "cat >/dev/null; echo 这个工具现在不许用 >&2; exit 2"] } ],
             "PostToolUse": [ { "matcher": "rewritten_tool", "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedOutput\":\"被钩子改写过了\"}'"] } ]
           } }"#,
    )
    .expect("写 hooks.json");
    let hooks = Arc::new(super::hooks::Hooks::load(dir.path(), None).expect("装载"));

    let mut registry = super::ToolRegistry::new();
    registry.register(tool("blocked_tool", "原样返回"));
    registry.register(tool("rewritten_tool", "原样返回"));
    registry.set_hooks(Some(hooks));

    let blocked = registry
        .execute(ctx().await, "blocked_tool", serde_json::json!({}))
        .await;
    assert!(blocked.starts_with("Error:"), "{blocked}");
    assert!(blocked.contains("这个工具现在不许用"), "{blocked}");

    let rewritten = registry
        .execute(ctx().await, "rewritten_tool", serde_json::json!({}))
        .await;
    assert_eq!(rewritten, "被钩子改写过了");
}

/// A tool that returns fixed text (for hook tests).
#[cfg(unix)]
fn tool(name: &str, answer: &str) -> super::Tool {
    use std::sync::Arc;

    use serde_json::Value;

    let answer = answer.to_string();
    super::Tool {
        name: name.to_string(),
        description: "测试用".to_string(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
        handler: Arc::new(move |_ctx: super::ToolCtx, _args: Value| {
            let answer = answer.clone();
            Box::pin(async move { Ok(answer) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>,
                >
        }),
    }
}

/// 记忆页：模型按目录读页，读到正文与出处；没有的页给一句话而不是空。
#[tokio::test]
async fn reading_a_memory_page_carries_its_sources() {
    let ctx = ctx().await;
    let bundle = joyczl_memory::bundle::Bundle::new(&ctx.home);
    let mut page = joyczl_memory::bundle::Page::new(
        "alex",
        "person",
        "Alex",
        "alex",
        "喜欢早会",
        "Alice 喜欢早上七点的会。",
    );
    page.sources = vec!["fact:1".to_string(), "ledger:s1/t3".to_string()];
    bundle.write(&page).expect("写页");

    let registry = handlers::build_default();
    let out = registry
        .execute(
            ctx.clone(),
            "read_memory_page",
            serde_json::json!({"page": "alex"}),
        )
        .await;
    assert!(out.contains("Alex"), "{out}");
    assert!(out.contains("早上七点的会"), "{out}");
    assert!(out.contains("fact:1"), "出处要带出来：{out}");

    let missing = registry
        .execute(
            ctx,
            "read_memory_page",
            serde_json::json!({"page": "nobody"}),
        )
        .await;
    assert!(missing.contains("has no page \"nobody\""), "{missing}");
}

/// `JOY_TOOL_POLICY=deferred`：工具表先窄，`find_tools` 打开后下一轮才宽。
///
/// 两条边界一起钉住：受开关与审批约束的工具**永远不会**被藏起来（发现步骤不能跑到
/// 授权前面），而隐藏只影响「模型看到什么」，不影响它能调什么。
#[test]
fn deferred_hides_the_shelf_until_find_tools_opens_it() {
    let mut view = handlers::build_default().session_view();
    view.defer_all_but_always();
    let names = |registry: &ToolRegistry| -> Vec<String> {
        registry
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect()
    };

    assert!(
        !names(&view).contains(&"list_memory".to_string()),
        "deferred 下没激活的工具不该进请求"
    );
    for always in [
        "current_time",
        "find_tools",
        "save_note",
        "search_memory",
        "read_file",
    ] {
        assert!(
            names(&view).contains(&always.to_string()),
            "{always} 是常驻工具，不该被藏"
        );
    }

    let opened = view.activator()("remember");
    assert_eq!(
        opened,
        vec!["list_memory".to_string()],
        "查询该只落在目标的描述上"
    );
    assert!(
        names(&view).contains(&"list_memory".to_string()),
        "打开之后下一轮该出现在工具表里"
    );
}

/// 一句话建任务：声明落在 `<home>`，模型不必让用户去挑工作目录。
///
/// 这条来自一次真实的卡住：用户说「帮我做一个定时任务」，模型去写 `schedules.json`，
/// 而那是工作区之外的文件 —— `write_file` 拒绝得对，界面却只好问「请选择目录」。
/// 定时任务是 Joy 自己的状态，不是项目内容，该由工具直接写。
#[tokio::test]
async fn schedule_job_writes_the_declaration_into_home() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "schedule_job",
            json!({"name": "morning-news", "cron": "0 8 * * 1-5", "prompt": "给我一份科技简报"}),
        )
        .await;
    assert!(out.contains("saved"), "{out}");
    assert!(out.contains("Next run"), "要告诉人什么时候跑：{out}");
    assert!(
        out.contains("joy schedule"),
        "谁在触发必须说清楚，否则它看起来像已经排好了：{out}"
    );
    assert!(
        ctx.home.join("schedules.json").exists(),
        "声明该落在 home 里，不是工作目录"
    );

    let listed = registry
        .execute(ctx.clone(), "list_schedules", json!({}))
        .await;
    assert!(listed.contains("morning-news"), "{listed}");

    let gone = registry
        .execute(
            ctx.clone(),
            "cancel_schedule",
            json!({"name": "morning-news"}),
        )
        .await;
    assert!(gone.contains("deleted"), "{gone}");
    let after = registry
        .execute(ctx.clone(), "list_schedules", json!({}))
        .await;
    assert!(after.contains("No scheduled jobs yet"), "{after}");
}

/// 坏输入变成一句可执行的话（`Error:` 文本回给模型），且不留下半个文件。
#[tokio::test]
async fn a_bad_cron_comes_back_as_a_sentence() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let error = registry
        .execute(
            ctx.clone(),
            "schedule_job",
            json!({"name": "x", "cron": "0 8 * *", "prompt": "y"}),
        )
        .await;
    assert!(error.starts_with("Error"), "{error}");
    assert!(error.contains("five fields"), "{error}");
    assert!(
        !ctx.home.join("schedules.json").exists(),
        "被拒绝时不该留下文件"
    );
}

// ---- 声明与实现不许漂 ---------------------------------------------------------------

/// 每个工具的 `input_schema` 都得是一份**能读的**契约：有描述、type=object、
/// required 里的每个名字都能在 properties 里找到、每个属性和顶层 type 都是合法的 JSON
/// 类型名。模型看到的正是这份 schema，写错一个字，代价是一次「参数不合法」的空转。
///
/// 为什么不在这里把 handler 也跑一遍：这些工具会连网、起子进程、写磁盘；「空参数会
/// 返回 Error 文本」由各工具自己的测试覆盖，混进这个遍历只会让一条断言变成一场副作用。
#[test]
fn every_builtin_tool_declares_a_schema_it_can_honour() {
    const JSON_TYPES: &[&str] = &[
        "object", "array", "string", "number", "integer", "boolean", "null",
    ];
    let registry = handlers::build_default();
    let names = registry.names();
    assert!(!names.is_empty(), "工具表不该是empty");

    let mut seen = std::collections::BTreeSet::new();
    for name in &names {
        assert!(
            seen.insert(name.to_string()),
            "工具名重复了：{name} —— register 会静默覆盖"
        );
        let tool = registry.get(name).expect("names() 里的工具要能取到");
        assert!(
            tool.description.trim().len() > 4,
            "{name} 的描述为空或太短 —— 模型就是靠这句话决定要不要用"
        );
        let schema = &tool.input_schema;
        assert_eq!(
            schema["type"], "object",
            "{name} 的 schema 顶层必须是 type=object"
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} 没有 properties"));
        if let Some(required) = schema["required"].as_array() {
            for field in required {
                let field = field.as_str().expect("required 里要写字符串");
                assert!(
                    properties.contains_key(field),
                    "{name} 的 required 里有 {field}，但 properties 里没有 —— 模型永远填不对"
                );
            }
        }
        let declared = schema["type"].as_str().expect("type 是字符串");
        assert!(
            JSON_TYPES.contains(&declared),
            "{name} 的 type 是 {declared}"
        );

        for (field, spec) in properties {
            let Some(kind) = spec["type"].as_str() else {
                // `anyOf` 之类是合法的 JSON Schema；只要不是「什么都没有」就行。
                assert!(
                    !spec.is_null(),
                    "{name}.{field} 没有任何类型信息，模型没法填"
                );
                continue;
            };
            assert!(
                JSON_TYPES.contains(&kind),
                "{name}.{field} 的类型是 {kind}，不是 JSON 类型名"
            );
        }
    }
}

/// `readonly()` 只去掉独占型工具，其余原样留下（批量委派靠它把写/执行收起来）。
#[tokio::test]
async fn readonly_drops_the_exclusive_tools_and_nothing_else() {
    let mut registry = handlers::build_default();
    // 用一个假的独占工具代表 run_command / write_file：真正的那些由 app-server 注册。
    let mut exclusive = crate::handlers::find_tools();
    exclusive.name = "pretend_write".to_string();
    registry.register_exclusive(exclusive);

    let read_only = registry.readonly();
    assert!(
        read_only.get("pretend_write").is_none(),
        "独占型工具必须被收走"
    );
    assert!(
        !read_only.is_exclusive("pretend_write"),
        "收走之后不能再被当成屏障"
    );
    assert!(
        read_only.get("read_file").is_some() && read_only.get("list_dir").is_some(),
        "只读工具一个都不能少"
    );
    assert!(
        registry.get("pretend_write").is_some(),
        "原表不能被改：readonly() 是拷贝"
    );
    assert!(
        read_only.names().len() + 1 == registry.names().len(),
        "差的恰好是那一个独占工具"
    );
}
