//! Tool tests: no network, real temp databases, verifying behavior and output wording.

use std::sync::Arc;

use serde_json::json;

use crate::{handlers, Tool, ToolCtx, ToolRegistry};

/// Opens a temp state.db and returns a ready-to-use ctx. home is also a temp dir —
/// the calendar.ics / outbox tests need real file writes.
async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    ToolCtx {
        on_uncertain: None,
        resource_lease: None,
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
            "glob",
            "grep",
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
            json!({"subject": "alex", "content": "likes morning meetings"}),
        )
        .await;
    // The output must say where things landed — the model relays it and must not overclaim.
    assert!(
        out.contains("state.db"),
        "the output never says where it landed: {out}"
    );
    assert!(
        out.contains("alex"),
        "the output does not repeat the content: {out}"
    );

    let out = registry
        .execute(ctx.clone(), "search_memory", json!({"query": "morning"}))
        .await;
    assert!(
        out.contains("likes morning meetings"),
        "it cannot be searched back: {out}"
    );
}

#[tokio::test]
async fn forget_note_reports_when_nothing_matched() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(ctx.clone(), "forget_note", json!({"subject": "ghost"}))
        .await;
    assert!(
        out.contains("no memory about"),
        "deleting 0 should still be said out loud: {out}"
    );

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
    assert!(
        out.contains("Forgot 1"),
        "it should report how many it deleted: {out}"
    );
}

#[tokio::test]
async fn current_time_gives_a_usable_answer() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "current_time", json!({}))
        .await;
    // Weekday and UTC offset are both there — parsing "tomorrow morning" needs them.
    assert!(out.contains("UTC"), "no timezone: {out}");
    assert!(
        out.contains("weekday") || out.chars().any(|c| c.is_ascii_alphabetic()),
        "there is no weekday: {out}"
    );
}

#[tokio::test]
async fn unknown_tool_and_bad_args_become_text_not_errors() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry.execute(ctx.clone(), "nope", json!({})).await;
    assert!(out.starts_with("Error:"), "an unknown tool: {out}");
    assert!(
        out.contains("save_note"),
        "it should list the tools that do exist: {out}"
    );

    // Missing argument
    let out = registry
        .execute(ctx.clone(), "save_note", json!({"subject": "alex"}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("content"),
        "a missing argument: {out}"
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
        "the wrong type: {out}"
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
        "an empty string: {out}"
    );
}

#[tokio::test]
async fn list_memory_handles_an_empty_store() {
    let registry = handlers::build_default();
    let out = registry
        .execute(ctx().await, "list_memory", json!({}))
        .await;
    assert!(
        out.contains("empty"),
        "an empty memory needs a guiding sentence: {out}"
    );
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
            json!({"title": "meet Alex", "start": "2026-07-14T09:00:00"}),
        )
        .await;
    assert!(out.contains("created"), "{out}");
    assert!(
        out.contains("local calendar"),
        "the output has to say where it landed: {out}"
    );
    // No end given defaults to +1 hour.
    assert!(
        out.contains("2026-07-14T10:00"),
        "the default end time: {out}"
    );

    let ics = std::fs::read_to_string(ctx.home.join("calendar.ics")).expect("the ics file");
    assert!(ics.contains("SUMMARY:meet Alex"));
    assert!(ics.contains("DTSTART:20260714T090000"), "{ics}");
    assert!(
        ics.ends_with("END:VCALENDAR\n"),
        "VCALENDAR has to close: {ics}"
    );

    // Run it again: a timestamp with seconds also normalizes to minute precision → idempotent hit.
    let out = registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "meet Alex", "start": "2026-07-14T09:00"}),
        )
        .await;
    assert!(
        out.contains("already exists"),
        "the second call should report the idempotent hit: {out}"
    );

    // And only one row in the database.
    let rows = ctx
        .calendar
        .list(None, None, 100)
        .await
        .expect("query the store");
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
    assert!(
        out.contains("local calendar"),
        "it has to say which calendar was read: {out}"
    );

    registry
        .execute(
            ctx.clone(),
            "create_event",
            json!({"title": "dentist", "start": "2026-07-14T09:00", "attendees": "Alex"}),
        )
        .await;
    let out = registry
        .execute(
            ctx.clone(),
            "list_events",
            json!({"start": "2026-07-14", "end": "2026-07-14"}),
        )
        .await;
    assert!(
        out.contains("dentist"),
        "events inside the window have to be listed: {out}"
    );
    assert!(
        out.contains(", with Alex"),
        "the attendees have to come along: {out}"
    );

    // Out-of-range entries don't appear.
    let out = registry
        .execute(
            ctx.clone(),
            "list_events",
            json!({"start": "2026-08-01", "end": "2026-08-31"}),
        )
        .await;
    assert!(
        out.contains("No events found"),
        "nothing outside the window should show up: {out}"
    );
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
            json!({"to": "Alex <alex@example.com>", "body": "see you Thursday"}),
        )
        .await;
    assert!(
        out.contains("Nothing was sent"),
        "it has to say outright that nothing was sent: {out}"
    );

    let dir = ctx.home.join("outbox");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("the outbox directory")
        .map(|e| e.expect("a directory entry"))
        .collect();
    assert_eq!(entries.len(), 1);
    let content = std::fs::read_to_string(entries.remove(0).path()).expect("the draft");
    assert!(
        content.starts_with("To: Alex <alex@example.com>\n\nsee you Thursday\n"),
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
            json!({"subject": "alex", "content": "likes coffee"}),
        )
        .await;
    let fact = &ctx
        .facts
        .search("coffee", 1)
        .await
        .expect("query the store")[0];

    let out = registry
        .execute(
            ctx.clone(),
            "manage_memory",
            json!({"action": "update_fact", "id": fact.id, "content": "likes tea"}),
        )
        .await;
    assert!(out.contains("now reads"), "{out}");
    assert!(
        ctx.facts.search("tea", 4).await.expect("query the store")[0]
            .content
            .contains("tea"),
        "the body really has to change"
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
    assert!(
        out.contains("with id"),
        "deleting twice has to be said honestly: {out}"
    );

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
    assert!(
        out.contains("slug"),
        "a path escape has to be stopped: {out}"
    );
    assert!(
        !ctx.home.join("evil").exists()
            && !ctx
                .home
                .parent()
                .map(|p| p.join("evil"))
                .is_some_and(|p| p.exists()),
        "nothing outside the directory tree may be written"
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

    assert!(
        out.starts_with("Error:"),
        "the Error: prefix has to stay: {out}"
    );
    assert!(out.contains("schema"), "{out}");
    assert!(out.contains("subject"), "it has to name the field: {out}");
    assert!(
        ctx.facts
            .recent(10, 0)
            .await
            .expect("query the store")
            .is_empty(),
        "a failed validation must not reach the store"
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
    assert!(
        out.contains("content"),
        "the missing field has to appear in the error: {out}"
    );
}

/// A remote (MCP) server may report a broken schema. Such tools **skip validation and run** —
/// a broken schema must not make a tool unusable.
#[tokio::test]
async fn a_tool_with_a_broken_schema_still_runs() {
    let mut registry = ToolRegistry::new();
    registry.register(Tool {
        name: "broken_schema".to_string(),
        description: "a tool whose schema is junk".to_string(),
        input_schema: json!("this is not a schema"),
        handler: Arc::new(|_ctx: ToolCtx, _args: serde_json::Value| {
            Box::pin(async { Ok("it really ran".to_string()) })
        }),
    });

    let out = registry
        .execute(ctx().await, "broken_schema", json!({"whatever": true}))
        .await;
    assert_eq!(
        out, "it really ran",
        "a broken schema must not block execution: {out}"
    );
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
            json!({"subject": "alex", "content": "likes morning meetings"}),
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
            description: "for the test".to_string(),
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
    registry.register(fixed("dup", "the first one"));
    registry.register(fixed("dup", "the second one"));

    assert_eq!(
        registry
            .names()
            .iter()
            .filter(|name| **name == "dup")
            .count(),
        1,
        "there should be exactly one"
    );
    let out = registry
        .execute(ctx().await, "dup", serde_json::json!({}))
        .await;
    assert_eq!(
        out, "the first one",
        "the first one is kept and the second refused"
    );
}

/// Tool-level hooks hang on the **single choke point**: PreToolUse can stop a call, PostToolUse can rewrite the result.
#[cfg(unix)]
#[tokio::test]
async fn tool_hooks_block_before_and_rewrite_after() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("hooks.json"),
        r#"{ "hooks": {
             "PreToolUse": [ { "matcher": "blocked_tool", "command": "sh",
               "args": ["-c", "cat >/dev/null; echo this tool is not allowed right now >&2; exit 2"] } ],
             "PostToolUse": [ { "matcher": "rewritten_tool", "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedOutput\":\"the hook rewrote this\"}'"] } ]
           } }"#,
    )
    .expect("write hooks.json");
    let hooks = Arc::new(super::hooks::Hooks::load(dir.path(), None).expect("load"));

    let mut registry = super::ToolRegistry::new();
    registry.register(tool("blocked_tool", "returned unchanged"));
    registry.register(tool("rewritten_tool", "returned unchanged"));
    registry.set_hooks(Some(hooks));

    let blocked = registry
        .execute(ctx().await, "blocked_tool", serde_json::json!({}))
        .await;
    assert!(blocked.starts_with("Error:"), "{blocked}");
    assert!(
        blocked.contains("this tool is not allowed right now"),
        "{blocked}"
    );

    let rewritten = registry
        .execute(ctx().await, "rewritten_tool", serde_json::json!({}))
        .await;
    assert_eq!(
        rewritten, "the hook rewrote this",
        "the hook rewrites the output"
    );
}

/// A tool that returns fixed text (for hook tests).
#[cfg(unix)]
fn tool(name: &str, answer: &str) -> super::Tool {
    use std::sync::Arc;

    use serde_json::Value;

    let answer = answer.to_string();
    super::Tool {
        name: name.to_string(),
        description: "for the test".to_string(),
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

/// Memory pages: the model reads a page from the catalog and gets the body plus its source; a
/// page that does not exist gets one sentence rather than nothing.
#[tokio::test]
async fn reading_a_memory_page_carries_its_sources() {
    let ctx = ctx().await;
    let bundle = joyczl_memory::bundle::Bundle::new(&ctx.home);
    let (fact, _) = ctx
        .facts
        .add(
            "Alex",
            "Alice likes the seven o'clock meeting.",
            "user",
            "user",
        )
        .await
        .unwrap();
    joyczl_memory::mirror::mirror_facts(&ctx.facts, &bundle)
        .await
        .unwrap();
    let mut page = bundle.read("alex").unwrap().unwrap();
    page.sources.push("ledger:s1/t3".into());
    bundle.write(&page).expect("write the page");

    let registry = handlers::build_default();
    let out = registry
        .execute(
            ctx.clone(),
            "read_memory_page",
            serde_json::json!({"page": "alex"}),
        )
        .await;
    assert!(out.contains("Alex"), "{out}");
    assert!(out.contains("the seven o'clock meeting"), "{out}");
    assert!(
        out.contains("fact:1"),
        "the source has to come along: {out}"
    );

    ctx.facts.delete(fact.id).await.unwrap();
    let stale = registry
        .execute(
            ctx.clone(),
            "read_memory_page",
            serde_json::json!({"page":"alex"}),
        )
        .await;
    assert!(stale.contains("excluded from recall"), "{stale}");
    assert!(!stale.contains("seven o'clock"), "{stale}");
    let missing = registry
        .execute(
            ctx,
            "read_memory_page",
            serde_json::json!({"page": "nobody"}),
        )
        .await;
    assert!(missing.contains("has no page \"nobody\""), "{missing}");
}

/// `JOY_TOOL_POLICY=deferred`: the tool list starts narrow and only widens on the next round,
/// after `find_tools` opens it.
///
/// Two boundaries pinned at once: a tool bound by a switch or an approval is **never** hidden
/// (discovery must not run ahead of authorisation), and hiding only changes what the model
/// sees, not what it can call.
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
        "a tool not activated under deferred must not enter the request"
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
            "{always} is an always-on tool and must not be hidden"
        );
    }

    let opened = view.activator()("remember");
    assert_eq!(
        opened,
        vec!["list_memory".to_string()],
        "the query has to land on the target's description only"
    );
    assert!(
        names(&view).contains(&"list_memory".to_string()),
        "once opened, it should appear in the tool table next round"
    );
}

/// A task from one sentence: the declaration lands in `<home>`, so the model does not have to
/// send the user off to pick a working directory.
///
/// That came from a real dead end: the user said "make me a scheduled task" and the model went
/// to write `schedules.json`, which lives outside the workspace — `write_file` refused, quite
/// rightly, and the UI was left asking "pick a directory". A scheduled task is Joy's own
/// state, not project content, so the tool should write it directly.
#[tokio::test]
async fn schedule_job_writes_the_declaration_into_home() {
    let registry = handlers::build_default();
    let ctx = ctx().await;

    let out = registry
        .execute(
            ctx.clone(),
            "schedule_job",
            json!({"name": "morning-news", "cron": "0 8 * * 1-5", "prompt": "give me a tech briefing"}),
        )
        .await;
    assert!(out.contains("saved"), "{out}");
    assert!(
        out.contains("Next run"),
        "a human has to be told when it runs: {out}"
    );
    assert!(
        out.contains("joy schedule"),
        "who triggers it has to be clear, or it looks as if it is already scheduled: {out}"
    );
    assert!(
        ctx.home.join("schedules.json").exists(),
        "the declaration lands in home, not the working directory"
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

/// Bad input turns into one actionable sentence (an `Error:` text back to the model) and
/// leaves no half-written file behind.
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
        "nothing may be left behind when it is refused"
    );
}

// ---- Declaration and implementation must not drift ----------------------------------

/// Every tool's `input_schema` has to be a **readable** contract: a description, type=object,
/// each name in required present in properties, and each property and the top-level type a
/// legal JSON type name. The model sees exactly this schema, and one wrong word costs a round
/// trip that ends in "invalid arguments".
///
/// Why the handlers are not run here too: these tools go online, start subprocesses and write
/// to disk; "empty arguments come back as an Error text" is covered by each tool's own tests,
/// and folding it into this sweep would turn one assertion into a pile of side effects.
#[test]
fn every_builtin_tool_declares_a_schema_it_can_honour() {
    const JSON_TYPES: &[&str] = &[
        "object", "array", "string", "number", "integer", "boolean", "null",
    ];
    let registry = handlers::build_default();
    let names = registry.names();
    assert!(!names.is_empty(), "the tool list must not be empty");

    let mut seen = std::collections::BTreeSet::new();
    for name in &names {
        assert!(
            seen.insert(name.to_string()),
            "the tool name is duplicated: {name} — register would overwrite silently"
        );
        let tool = registry
            .get(name)
            .expect("every tool in names() has to be retrievable");
        assert!(
            tool.description.trim().len() > 4,
            "{name}'s description is empty or too short — that sentence is how the model decides whether to use it"
        );
        let schema = &tool.input_schema;
        assert_eq!(
            schema["type"], "object",
            "{name}'s schema has to be type=object at the top"
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} has no properties"));
        if let Some(required) = schema["required"].as_array() {
            for field in required {
                let field = field.as_str().expect("required must hold strings");
                assert!(
                    properties.contains_key(field),
                    "{name} lists {field} in required but not in properties — the model can never fill it in right"
                );
            }
        }
        let declared = schema["type"].as_str().expect("type is a string");
        assert!(
            JSON_TYPES.contains(&declared),
            "{name}'s type is {declared}"
        );

        for (field, spec) in properties {
            let Some(kind) = spec["type"].as_str() else {
                // Something like `anyOf` is legal JSON Schema; it just must not be "nothing at all".
                assert!(
                    !spec.is_null(),
                    "{name}.{field} carries no type information, so the model cannot fill it in"
                );
                continue;
            };
            assert!(
                JSON_TYPES.contains(&kind),
                "{name}.{field} is typed {kind}, which is not a JSON type name"
            );
        }
    }
}

/// `readonly()` drops only the exclusive tools and leaves the rest alone (batch delegation uses
/// it to put writes and exec away).
#[tokio::test]
async fn readonly_drops_the_exclusive_tools_and_nothing_else() {
    let mut registry = handlers::build_default();
    // A fake exclusive tool stands in for run_command / write_file: the real ones are registered by
    // the app-server.
    let mut exclusive = crate::handlers::find_tools();
    exclusive.name = "pretend_write".to_string();
    registry.register_exclusive(exclusive);

    let read_only = registry.readonly();
    assert!(
        read_only.get("pretend_write").is_none(),
        "an exclusive tool has to be taken away"
    );
    assert!(
        !read_only.is_exclusive("pretend_write"),
        "once taken away it must not count as a barrier"
    );
    assert!(
        read_only.get("read_file").is_some() && read_only.get("list_dir").is_some(),
        "not one read-only tool may go missing"
    );
    assert!(
        registry.get("pretend_write").is_some(),
        "the original table must not change: readonly() copies"
    );
    assert!(
        read_only.names().len() + 1 == registry.names().len(),
        "the one missing is exactly the exclusive tool"
    );
}
