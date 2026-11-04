//! Tests for the loop.
//!
//! A mock provider scripts exactly what the model says each turn, verifying the guardrails
//! one by one: natural end, tool round trip, errors not fatal, iteration cap.
//! Scripted model: what the model says is entirely decided by the test.

use std::sync::{Arc, Mutex};

use joyczl_provider::mock::Mock;
use joyczl_provider::StopReason;
use joyczl_tools::handlers;
use serde_json::json;

use crate::{run, LoopEvent, Turn};

/// A real state.db + the default tool set.
async fn ctx() -> joyczl_tools::ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here
    joyczl_tools::ToolCtx {
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(joyczl_tools::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home: std::path::PathBuf::from(".joy"),
    }
}

async fn turn<'a>(
    client: &'a Mock,
    tools: &'a joyczl_tools::ToolRegistry,
    ctx: joyczl_tools::ToolCtx,
    observer: Option<crate::Observer>,
) -> crate::LoopResult {
    run(Turn {
        client,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "记住 alex 喜欢早会".to_string(),
        tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer,
        on_text: None,
        interrupt: None,
    })
    .await
    .expect("loop 跑完")
}

#[tokio::test]
async fn no_tool_call_ends_the_turn_immediately() {
    let mock = Mock::new(vec![Mock::text("好，我记住了。")]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;

    assert_eq!(result.reply, "好，我记住了。");
    assert_eq!(result.iterations, 1);
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.usage.input_tokens, 10);
    // The user message made it into the prompt, and so did the system.
    let sent = mock.received.lock().unwrap()[0].clone();
    assert!(sent
        .messages
        .iter()
        .any(|m| m.text().contains("alex 喜欢早会")));
    assert_eq!(sent.system.as_deref(), Some("你是 Joy"));
}

#[tokio::test]
async fn tool_call_round_trip_feeds_result_back() {
    let mock = Mock::new(vec![
        Mock::tool_use(
            "tu_1",
            "save_note",
            json!({"subject": "alex", "content": "喜欢早会"}),
        ),
        Mock::text("已经记下了。"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;
    let result = turn(&mock, &tools, ctx.clone(), None).await;

    assert_eq!(result.iterations, 2);
    assert_eq!(result.reply, "已经记下了。");
    assert_eq!(result.tool_calls.len(), 1);
    assert!(
        result.tool_calls[0].ok(),
        "save_note 不该失败：{}",
        result.tool_calls[0].output
    );

    // The tool result really was fed back to the model.
    let second = mock.received.lock().unwrap()[1].clone();
    let has_tool_result = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, joyczl_provider::ContentBlock::ToolResult { .. }))
    });
    assert!(has_tool_result, "第二轮的 messages 里应当有 tool_result");
    // And the fact really was written to the store.
    assert_eq!(ctx.facts.search("早会", 4).await.unwrap().len(), 1);
}

#[tokio::test]
async fn tool_error_becomes_text_and_the_loop_continues() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": 42})), // type error → the tool returns an "Error:" text
        Mock::text("参数好像不对，你能再说一遍吗？"),
    ]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;

    assert!(!result.tool_calls[0].ok());
    assert_eq!(result.iterations, 2, "出错后 loop 应当继续，而不是崩掉");
    assert_eq!(result.reply, "参数好像不对，你能再说一遍吗？");
    // The error text went back to the model as a tool_result.
    let second = mock.received.lock().unwrap()[1].clone();
    let text = second
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            joyczl_provider::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.starts_with("Error:"), "{text}");
}

#[tokio::test]
async fn unknown_tool_name_is_surfaced_not_fatal() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "hack_the_planet", json!({})),
        Mock::text("我没有那个工具。"),
    ]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;
    assert!(result.tool_calls[0]
        .output
        .contains("没有叫 'hack_the_planet' 的工具"));
    assert_eq!(result.iterations, 2);
}

#[tokio::test]
async fn iteration_limit_stops_the_loop() {
    // The model keeps calling tools and never speaks — the guardrail must hard-stop, not spin forever.
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::tool_use("tu_2", "current_time", json!({})),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "现在几点".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 2, // deliberately stuck on two tool replies
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
    })
    .await
    .expect("护栏应当接住，而不是报错");

    assert_eq!(result.iterations, 2);
    assert!(
        result.reply.contains("迭代上限"),
        "护栏消息要说清楚发生了什么：{}",
        result.reply
    );
    assert_eq!(result.tool_calls.len(), 2);
}

#[tokio::test]
async fn observer_sees_llm_and_tool_events_in_order() {
    #[derive(Default)]
    struct Log(Vec<String>);
    let log = Arc::new(Mutex::new(Log::default()));
    let sink = log.clone();

    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("记好了"),
    ]);
    let tools = handlers::build_default();
    let observer = move |event: LoopEvent| {
        let name = match event {
            LoopEvent::Llm {
                iteration,
                stop_reason,
                ..
            } => {
                format!(
                    "llm#{iteration}={}",
                    match stop_reason {
                        StopReason::ToolUse => "tool_use",
                        StopReason::EndTurn => "end_turn",
                        _ => "other",
                    }
                )
            }
            LoopEvent::Text { .. } => "text".to_string(),
            LoopEvent::ToolStart { name, .. } => format!("tool_start:{name}"),
            LoopEvent::Tool { name, .. } => format!("tool:{name}"),
        };
        sink.lock().unwrap().0.push(name);
    };

    let result = turn(&mock, &tools, ctx().await, Some(Arc::new(observer))).await;
    assert_eq!(result.iterations, 2);

    let events = &log.lock().unwrap().0;
    assert_eq!(
        events,
        &vec![
            "llm#1=tool_use".to_string(),
            "tool_start:save_note".to_string(),
            "tool:save_note".to_string(),
            "llm#2=end_turn".to_string()
        ]
    );
}

#[tokio::test]
async fn streaming_emits_text_deltas_in_order() {
    // The mock splits into two-char chunks — they must arrive in order, and the final reply must be complete.
    let mock = Mock::streaming(vec![Mock::text("好的，我记住了。")]);
    let tools = handlers::build_default();

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = deltas.clone();
    let on_text: joyczl_provider::TextSink = Arc::new(move |delta: &str| {
        sink.lock().unwrap().push(delta.to_string());
    });

    let events: Arc<Mutex<Vec<LoopEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let observer_sink = events.clone();
    let observer: crate::Observer = Arc::new(move |event: LoopEvent| {
        observer_sink.lock().unwrap().push(event);
    });

    let ctx = ctx().await;
    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer: Some(observer),
        on_text: Some(on_text),
        interrupt: None,
    })
    .await
    .expect("loop 跑完");

    assert!(
        mock.streamed.lock().unwrap().iter().all(|x| *x),
        "应当真的走了流式"
    );
    let collected = deltas.lock().unwrap().join("");
    assert_eq!(collected, "好的，我记住了。", "增量拼起来应当是完整文本");
    assert!(
        deltas.lock().unwrap().len() > 1,
        "应当有多个增量，而不是一整段"
    );

    // The observer also received the same Text events.
    let text_events = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            LoopEvent::Text { delta } => Some(delta.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text_events, "好的，我记住了。");
    assert_eq!(result.reply, "好的，我记住了。", "最终应答应当还是完整的");
}

#[tokio::test]
async fn streaming_works_across_tool_rounds() {
    // First round streaming + tool call, second round plain text — the two paths must not interfere.
    let mock = Mock::streaming(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("记好了。"),
    ]);
    let tools = handlers::build_default();

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = deltas.clone();
    let on_text: joyczl_provider::TextSink = Arc::new(move |d: &str| {
        sink.lock().unwrap().push(d.to_string());
    });

    let ctx = ctx().await;
    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: Some(on_text),
        interrupt: None,
    })
    .await
    .expect("loop 跑完");

    assert_eq!(result.iterations, 2);
    assert_eq!(
        deltas.lock().unwrap().join(""),
        "记好了。",
        "工具那一轮没有文本，不该产生增量"
    );
    assert!(result.tool_calls[0].ok());
}

/// Cancel the token up front: the loop stands down on entry, sends no model call,
/// and the reply honestly says this turn was interrupted.
#[tokio::test]
async fn a_cancelled_interrupt_stops_the_loop_before_any_model_call() {
    let mock = Mock::new(vec![Mock::text("这句话永远轮不到被说出。")]);
    let tools = handlers::build_default();

    let interrupt = crate::Interrupt::new();
    interrupt.cancel();

    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: Some(interrupt),
    })
    .await
    .expect("打断不该是错误");

    assert!(result.interrupted);
    assert_eq!(result.iterations, 0, "一次模型调用都不该发生");
    assert_eq!(result.reply, "（这轮被打断了。）");
    assert!(
        mock.received.lock().unwrap().is_empty(),
        "取消令牌拨下了还去调模型，就是没听话"
    );
}

/// The interrupt lands between tool rounds: save_note really ran, but the second model call
/// never happened, and the results already obtained come back with the interrupted flag.
#[tokio::test]
async fn an_interrupt_between_tool_rounds_keeps_what_already_happened() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("这句话不该出现。"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let interrupt = crate::Interrupt::new();
    // First round streaming (the mock does not chunk, so no deltas to listen for) — use the
    // observer to flip the token when the first tool event arrives: round 1 tools are done, round 2 not started.
    let first_tool_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = first_tool_fired.clone();
    let interrupt_for_observer = interrupt.clone();
    let observer: crate::Observer = Arc::new(move |event| {
        if matches!(event, crate::LoopEvent::Tool { .. }) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            interrupt_for_observer.cancel();
        }
    });

    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        tool_result_budget: Default::default(),
        observer: Some(observer),
        on_text: None,
        interrupt: Some(interrupt),
    })
    .await
    .expect("打断不该是错误");

    assert!(first_tool_fired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(result.interrupted);
    assert_eq!(result.tool_calls.len(), 1, "第一轮工具确实做完了");
    assert_eq!(result.iterations, 1, "第二轮模型调用没有发生");
    assert_eq!(result.reply, "（这轮被打断了。）");
}

/// The loop guardrail wired into the loop: when the same call repeats to the 3rd time, the
/// fed-back text gains a reminder, but `tool_calls` records the **real result** (trace sees the truth).
#[tokio::test]
async fn the_stall_guard_warns_the_model_but_keeps_real_tool_output() {
    // Use `search_memory` instead of `save_note`: the latter dedupes now, so the first reply
    // ("remembered #1") differs from later ones ("already noted"), never forming "3 identical".
    // The guardrail wants the **same call, same result**, so pick a read-only tool with stable output.
    let args = json!({"query": "查不到的东西"});
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "search_memory", args.clone()),
        Mock::tool_use("tu_2", "search_memory", args.clone()),
        Mock::tool_use("tu_3", "search_memory", args.clone()),
        Mock::text("好了。"),
    ]);
    let tools = handlers::build_default();

    let result = turn(&mock, &tools, ctx().await, None).await;

    assert!(result.guard.hits >= 1, "第 3 次相同调用必须触发护栏");
    assert!(result.guard.note.is_some(), "命中的说明要留下来");

    // The ToolResult fed back to the model carries the reminder...
    let warned = result.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, joyczl_provider::ContentBlock::ToolResult { content, .. }
                     if content.contains("[guard]"))
        })
    });
    assert!(warned, "提醒必须进到模型看得见的地方");

    // ...while the recorded tool output is still clean.
    assert!(
        result
            .tool_calls
            .iter()
            .all(|call| !call.output.contains("[guard]")),
        "tool_calls 里该是真结果"
    );
    assert!(
        result.tool_calls.iter().all(|call| call.ok()),
        "这三次调用本身都是成功的"
    );
}

/// The intra-turn budget really runs: by the second request the 100k-char result is stubbed,
/// and the file the stub points to holds the full original text.
#[tokio::test]
async fn an_oversized_tool_result_becomes_a_stub_before_the_next_request() {
    use joyczl_tools::{BoxFut, Tool, ToolRegistry};
    use serde_json::Value;

    let home = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .expect("打开库");
    let ctx = joyczl_tools::ToolCtx {
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(joyczl_tools::web::TurnLimits::default()),
        session_id: "budget".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home: home.path().to_path_buf(),
    };

    // An "MCP-style" tool: nobody watches its output; it returns 100k characters.
    let big: Tool = Tool {
        name: "mcp__demo__fetch".to_string(),
        description: "回一大段文本".to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        handler: Arc::new(|_ctx: joyczl_tools::ToolCtx, _args: Value| {
            Box::pin(async move { Ok("z".repeat(100_000)) }) as BoxFut
        }),
    };
    let mut tools = ToolRegistry::new();
    tools.register(big);

    let mock = Mock::new(vec![
        Mock::tool_use("call-1", "mcp__demo__fetch", json!({})),
        Mock::text("看完了。"),
    ]);

    let result = run(Turn {
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "把那段文本拿来看看".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 4,
        max_tokens: 1024,
        tool_result_budget: crate::budget::ToolResultBudget {
            total_chars: 10_000,
            per_result_chars: 5_000,
        },
        observer: None,
        on_text: None,
        interrupt: None,
    })
    .await
    .expect("loop 跑完");

    assert_eq!(result.reply, "看完了。");
    let requests = mock.received.lock().unwrap();
    assert_eq!(requests.len(), 2, "一次工具往返 = 两次模型调用");

    let second = &requests[1];
    let stub = second
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            joyczl_provider::ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("第二次请求里该有工具结果");
    assert!(
        stub.contains("已截断") && stub.chars().count() < 5_000,
        "超限结果该换成桩：{} 字符",
        stub.chars().count()
    );
    assert!(stub.contains("完整输出在 spill/"), "{stub}");

    // The file the stub points to holds the full original (100k chars).
    let relative = stub
        .split("完整输出在 ")
        .nth(1)
        .and_then(|rest| rest.split('）').next())
        .expect("路径");
    let saved = std::fs::read_to_string(home.path().join(relative)).expect("读回落盘");
    assert_eq!(saved.chars().count(), 100_000);

    // The spilled file lives under the session directory (`spill/date/…`).
    assert!(relative.starts_with("spill/"), "{relative}");
}
