//! Tests for the loop.
//!
//! A mock provider scripts exactly what the model says each turn, verifying the guardrails
//! one by one: natural end, tool round trip, errors not fatal, iteration cap.
//! Scripted model: what the model says is entirely decided by the test.

use std::sync::{Arc, Mutex};

use joyczl_provider::mock::Mock;
use joyczl_provider::{ContentBlock, Message, Role, StopReason, Usage};
use joyczl_tools::handlers;
use serde_json::json;

use crate::{run, LoopEvent, Turn};

/// A real state.db + the default tool set.
async fn ctx() -> joyczl_tools::ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here
    joyczl_tools::ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
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
        on_delta: None,
    }
}

async fn turn<'a>(
    client: &'a Mock,
    tools: &'a joyczl_tools::ToolRegistry,
    ctx: joyczl_tools::ToolCtx,
    observer: Option<crate::Observer>,
) -> crate::LoopResult {
    run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "remember that alex likes morning meetings".to_string(),
        tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop should finish")
}

/// 0.16: the sentence added mid-run goes into the **next** model call, and tools that already
/// ran are not run again.
///
/// That is the line between steering and "cancel and start over": it does not replay the
/// turn, it appends one sentence to the end of working memory.
#[tokio::test]
async fn words_added_mid_turn_join_the_next_model_call_and_do_not_rerun_tools() {
    let steering = crate::Steering::new();
    let mock = Mock::new(vec![
        Mock::tool_use("call-1", "list_dir", json!({ "path": "." })),
        Mock::text("read it all."),
    ]);
    let tools = handlers::build_default();

    // Read off the observer: the real ordering of turn/steer — accepted after the first model
    // call, visible to the second.
    let observer: Option<crate::Observer> = {
        let steering = steering.clone();
        Some(Arc::new(move |event: LoopEvent| {
            if let LoopEvent::Llm { iteration: 1, .. } = event {
                steering
                    .lock()
                    .expect("lock")
                    .push("have a look at the README too");
            }
        }))
    };

    let result = run(Turn {
        system_prefix: None,
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "have a look at the directory".to_string(),
        user_images: vec![],
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: Some(steering.clone()),
        resume: None,
    })
    .await
    .expect("the loop finishes");

    assert_eq!(
        result.tool_calls.len(),
        1,
        "the tool should run exactly once"
    );
    let received = mock.received.lock().expect("lock");
    assert_eq!(received.len(), 2, "two sentences = two model calls");
    let last = received[1]
        .messages
        .last()
        .expect("the second call is not empty");
    assert!(matches!(last.role, Role::User));
    assert!(
        last.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "have a look at the README too")),
        "{:?}",
        last.content
    );
    // Every record stays: the transcript has to show the addition, even if it was accepted a
    // moment before the ending.
    assert_eq!(
        steering.lock().expect("lock").all(),
        vec!["have a look at the README too".to_string()]
    );
}

#[tokio::test]
async fn no_tool_call_ends_the_turn_immediately() {
    let mock = Mock::new(vec![Mock::text("fine, noted.")]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;

    assert_eq!(result.reply, "fine, noted.");
    assert_eq!(result.iterations, 1);
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.usage.input_tokens, 10);
    // The user message made it into the prompt, and so did the system.
    let sent = mock.received.lock().unwrap()[0].clone();
    assert!(sent
        .messages
        .iter()
        .any(|m| m.text().contains("alex likes morning meetings")));
    assert_eq!(sent.system.as_deref(), Some("You are Joy"));
}

#[tokio::test]
async fn tool_call_round_trip_feeds_result_back() {
    let mock = Mock::new(vec![
        Mock::tool_use(
            "tu_1",
            "save_note",
            json!({"subject": "alex", "content": "likes morning meetings"}),
        ),
        Mock::text("already noted."),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;
    let result = turn(&mock, &tools, ctx.clone(), None).await;

    assert_eq!(result.iterations, 2);
    assert_eq!(result.reply, "already noted.");
    assert_eq!(result.tool_calls.len(), 1);
    assert!(
        result.tool_calls[0].ok(),
        "save_note must not fail: {}",
        result.tool_calls[0].output
    );

    // The tool result really was fed back to the model.
    let second = mock.received.lock().unwrap()[1].clone();
    let has_tool_result = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, joyczl_provider::ContentBlock::ToolResult { .. }))
    });
    assert!(
        has_tool_result,
        "the second round's messages should carry a tool_result"
    );
    // And the fact really was written to the store.
    assert_eq!(
        ctx.facts.search("morning meeting", 4).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn tool_error_becomes_text_and_the_loop_continues() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": 42})), // type error → the tool returns an "Error:" text
        Mock::text("the arguments look wrong, could you say that again?"),
    ]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;

    assert!(!result.tool_calls[0].ok());
    assert_eq!(
        result.iterations, 2,
        "after an error the loop should carry on, not fall over"
    );
    assert_eq!(
        result.reply,
        "the arguments look wrong, could you say that again?"
    );
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
        Mock::text("I do not have that tool."),
    ]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;
    assert!(result.tool_calls[0]
        .output
        .contains("no tool named 'hack_the_planet'"));
    assert_eq!(result.iterations, 2);
}

#[tokio::test]
async fn the_last_iteration_is_a_wrap_up_not_a_cliff() {
    // Running out of budget no longer stops hard: the last call puts the tools away and lets the
    // model close in three sentences.
    //
    // That rule comes from a real waste: after 10 steps and 210k input tokens the user got
    // "I hit the iteration limit before finishing — try splitting the request into smaller
    // pieces". Claude Code's loop has no interactive limit at all (the model stops by not calling
    // a tool), so the "limit" here changed from a failure into a closing.
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("the time was read; what is unfinished is the cleanup; I need you to confirm what to delete."),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "what time is it".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 2,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the closing round should not error");

    assert_eq!(result.iterations, 2);
    assert_eq!(
        result.reply, "the time was read; what is unfinished is the cleanup; I need you to confirm what to delete.",
        "the wrapping-up round's words go to the user as they are, not swapped for a template"
    );

    // The last request must have **no tools** and carry the closing instruction (otherwise the
    // model keeps calling tools).
    let requests = mock.received.lock().expect("lock");
    let last = requests.last().expect("at least one request has to go out");
    assert!(
        last.tools.is_empty(),
        "the wrapping-up round must get no more tools: {} of them",
        last.tools.len()
    );
    let tail = serde_json::to_string(&last.messages).expect("serialise");
    assert!(
        tail.contains("Budget note"),
        "the wrapping-up instruction has to reach the context: {tail:.200}"
    );
}

#[tokio::test]
async fn the_token_budget_also_turns_the_next_round_into_a_wrap_up() {
    // The step limit governs how many rounds, the token budget how expensive. Reaching the budget
    // takes the same closing path —
    // and it has to say the wallet ran out, not that the work is done.
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("the budget is up, so I am wrapping up: the time was read, the rest of the cleanup is not done."),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "what time is it".to_string(),
        tools: &tools,
        ctx,
        // Steps to spare (10 rounds), but the wallet covers one and a half.
        max_iterations: 10,
        max_tokens: 1024,
        token_budget: Some(20),
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the budget closing should not error");

    assert_eq!(
        result.iterations, 2,
        "the first round as usual, the second goes straight to closing"
    );
    let requests = mock.received.lock().expect("lock");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].tools.is_empty(),
        "the budget wrapping-up round must get no more tools: {} of them",
        requests[1].tools.len()
    );
    let tail = serde_json::to_string(&requests[1].messages).expect("serialise");
    assert!(
        tail.contains("token budget is spent"),
        "it has to say plainly that the budget stopped it: {tail:.200}"
    );
}

#[tokio::test]
async fn without_a_token_budget_the_loop_keeps_its_tools() {
    // Off by default: with no budget the second round still carries tools (otherwise "unchanged by
    // default" would not be true).
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("done"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let _ = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "what time is it".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 10,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("two rounds and it should be over");

    let requests = mock.received.lock().expect("lock");
    assert!(
        !requests[1].tools.is_empty(),
        "with no budget the second round carries tools as usual"
    );
    let tail = serde_json::to_string(&requests[1].messages).expect("serialise");
    assert!(
        !tail.contains("Budget note"),
        "no budget means no closing instruction"
    );
}

#[tokio::test]
async fn observer_sees_llm_and_tool_events_in_order() {
    #[derive(Default)]
    struct Log(Vec<String>);
    let log = Arc::new(Mutex::new(Log::default()));
    let sink = log.clone();

    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("noted"),
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
            LoopEvent::ToolDelta { chunk, .. } => format!("tool_delta:{chunk}"),
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
    let mock = Mock::streaming(vec![Mock::text("all right, noted.")]);
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: Some(observer),
        on_text: Some(on_text),
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop finishes");

    assert!(
        mock.streamed.lock().unwrap().iter().all(|x| *x),
        "it really did go through streaming"
    );
    let collected = deltas.lock().unwrap().join("");
    assert_eq!(
        collected, "all right, noted.",
        "the joined deltas should be the whole text"
    );
    assert!(
        deltas.lock().unwrap().len() > 1,
        "there should be several deltas, not one whole paragraph"
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
    assert_eq!(text_events, "all right, noted.");
    assert_eq!(
        result.reply, "all right, noted.",
        "the final reply should still be whole"
    );
}

#[tokio::test]
async fn streaming_works_across_tool_rounds() {
    // First round streaming + tool call, second round plain text — the two paths must not interfere.
    let mock = Mock::streaming(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("noted."),
    ]);
    let tools = handlers::build_default();

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = deltas.clone();
    let on_text: joyczl_provider::TextSink = Arc::new(move |d: &str| {
        sink.lock().unwrap().push(d.to_string());
    });

    let ctx = ctx().await;
    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: Some(on_text),
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop finishes");

    assert_eq!(result.iterations, 2);
    assert_eq!(
        deltas.lock().unwrap().join(""),
        "noted.",
        "the tool round has no text, so no delta should be produced"
    );
    assert!(result.tool_calls[0].ok());
}

/// Cancel the token up front: the loop stands down on entry, sends no model call,
/// and the reply honestly says this turn was interrupted.
#[tokio::test]
async fn a_cancelled_interrupt_stops_the_loop_before_any_model_call() {
    let mock = Mock::new(vec![Mock::text(
        "this sentence never gets its turn to be said.",
    )]);
    let tools = handlers::build_default();

    let interrupt = crate::Interrupt::new();
    interrupt.cancel();

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: Some(interrupt),
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("an interrupt is not an error");

    assert!(result.interrupted);
    assert_eq!(result.iterations, 0, "not one model call should happen");
    assert_eq!(result.reply, "(this turn was interrupted.)");
    assert!(
        mock.received.lock().unwrap().is_empty(),
        "calling the model after the cancel token was flipped means it did not listen"
    );
}

/// The interrupt lands between tool rounds: save_note really ran, but the second model call
/// never happened, and the results already obtained come back with the interrupted flag.
#[tokio::test]
async fn an_interrupt_between_tool_rounds_keeps_what_already_happened() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("this sentence should not appear."),
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "hi".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: Some(observer),
        on_text: None,
        interrupt: Some(interrupt),
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("an interrupt is not an error");

    assert!(first_tool_fired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(result.interrupted);
    assert_eq!(
        result.tool_calls.len(),
        1,
        "the first round's tool really did run"
    );
    assert_eq!(result.iterations, 1, "the second model call never happened");
    assert_eq!(result.reply, "(this turn was interrupted.)");
}

/// The loop guardrail wired into the loop: when the same call repeats to the 3rd time, the
/// fed-back text gains a reminder, but `tool_calls` records the **real result** (trace sees the truth).
#[tokio::test]
async fn the_stall_guard_warns_the_model_but_keeps_real_tool_output() {
    // Use `search_memory` instead of `save_note`: the latter dedupes now, so the first reply
    // ("remembered #1") differs from later ones ("already noted"), never forming "3 identical".
    // The guardrail wants the **same call, same result**, so pick a read-only tool with stable output.
    let args = json!({"query": "something that cannot be found"});
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "search_memory", args.clone()),
        Mock::tool_use("tu_2", "search_memory", args.clone()),
        Mock::tool_use("tu_3", "search_memory", args.clone()),
        Mock::text("done."),
    ]);
    let tools = handlers::build_default();

    let result = turn(&mock, &tools, ctx().await, None).await;

    assert!(
        result.guard.hits >= 1,
        "the third identical call must trip the guard"
    );
    assert!(
        result.guard.note.is_some(),
        "the note about the trip has to be kept"
    );

    // The ToolResult fed back to the model carries the reminder...
    let warned = result.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, joyczl_provider::ContentBlock::ToolResult { content, .. }
                     if content.contains("[guard]"))
        })
    });
    assert!(warned, "the warning has to land where the model can see it");

    // ...while the recorded tool output is still clean.
    assert!(
        result
            .tool_calls
            .iter()
            .all(|call| !call.output.contains("[guard]")),
        "tool_calls should hold the real result"
    );
    assert!(
        result.tool_calls.iter().all(|call| call.ok()),
        "all three calls were themselves successful"
    );
}

/// The intra-turn budget really runs: by the second request the 100k-char result is stubbed,
/// and the file the stub points to holds the full original text.
#[tokio::test]
async fn an_oversized_tool_result_becomes_a_stub_before_the_next_request() {
    use joyczl_tools::{BoxFut, Tool, ToolRegistry};
    use serde_json::Value;

    let home = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .expect("open the store");
    let ctx = joyczl_tools::ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
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
        on_delta: None,
    };

    // An "MCP-style" tool: nobody watches its output; it returns 100k characters.
    let big: Tool = Tool {
        name: "mcp__demo__fetch".to_string(),
        description: "answer with one big block of text".to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        handler: Arc::new(|_ctx: joyczl_tools::ToolCtx, _args: Value| {
            Box::pin(async move { Ok("z".repeat(100_000)) }) as BoxFut
        }),
    };
    let mut tools = ToolRegistry::new();
    tools.register(big);

    let mock = Mock::new(vec![
        Mock::tool_use("call-1", "mcp__demo__fetch", json!({})),
        Mock::text("read it all."),
    ]);

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "let us have a look at that text".to_string(),
        tools: &tools,
        ctx,
        max_iterations: 4,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: crate::budget::ToolResultBudget {
            total_chars: 10_000,
            per_result_chars: 5_000,
            keep_recent: 0,
        },
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop finishes");

    assert_eq!(result.reply, "read it all.");
    let requests = mock.received.lock().unwrap();
    assert_eq!(requests.len(), 2, "one tool round trip = two model calls");

    let second = &requests[1];
    let stub = second
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            joyczl_provider::ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("the second request should carry the tool result");
    assert!(
        stub.contains("truncated") && stub.chars().count() < 5_000,
        "an over-limit result should be swapped for a stub: {} characters",
        stub.chars().count()
    );
    assert!(stub.contains("full output at spill/"), "{stub}");

    // The file the stub points to holds the full original (100k chars).
    let relative = stub
        .split("full output at ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the path");
    let saved = std::fs::read_to_string(home.path().join(relative)).expect("read the spill back");
    assert_eq!(saved.chars().count(), 100_000);

    // The spilled file lives under the session directory (`spill/date/…`).
    assert!(relative.starts_with("spill/"), "{relative}");
}
/// Checkpoints land at tool-results boundaries: one per completed iteration, carrying the
/// full working memory — a crash there is resumable from exactly that point.
#[tokio::test]
async fn a_checkpoint_lands_at_every_tool_results_boundary() {
    let mock = Mock::new(vec![
        Mock::tool_use(
            "tu_1",
            "save_note",
            json!({"subject": "alex", "content": "likes morning meetings"}),
        ),
        Mock::text("noted."),
    ]);
    let tools = handlers::build_default();
    let checkpoints: Arc<Mutex<Vec<crate::Checkpoint>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let checkpoints = checkpoints.clone();
        Arc::new(move |cp: crate::Checkpoint| {
            let checkpoints = checkpoints.clone();
            Box::pin(async move {
                checkpoints.lock().unwrap().push(cp);
                Ok(())
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        }) as crate::CheckpointSink
    };

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "remember that alex likes morning meetings".to_string(),
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: Some(sink),
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop finishes");

    assert_eq!(result.iterations, 2);
    let cps = checkpoints.lock().unwrap();
    assert!(cps.iter().all(|cp| cp.round_id == result.round_id));
    assert!(!result.round_id.is_empty());
    assert_eq!(cps.len(), 3, "initial input, execution intent, tool result");
    assert_eq!(cps[0].iterations, 0);
    assert!(cps[0].operations.is_empty());
    assert_eq!(cps[1].operations.len(), 1);
    assert_eq!(cps[1].operations[0].tool_use_id, "tu_1");
    assert!(cps[1].operations[0].output.is_none());
    assert!(cps[2].operations[0].output.is_some());
    // The checkpoint's working memory holds the tool round: assistant's tool request + the result.
    let last = cps[2].messages.last().expect("there is a result block");
    assert!(matches!(
        last.content.first(),
        Some(joyczl_provider::ContentBlock::ToolResult { .. })
    ));
}

/// Resume: the loop continues the checkpointed working memory — no user message is re-added,
/// no finished tool is re-run, and the pre-crash usage joins the final account.
#[tokio::test]
async fn a_resumed_turn_continues_without_rerunning_finished_tools() {
    let mock = Mock::new(vec![Mock::text(
        "carrying on from the last round, it is done.",
    )]);
    let tools = handlers::build_default();

    // The checkpoint's working memory: user asked, assistant requested a tool, result came back.
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "remember that alex likes morning meetings".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "save_note".to_string(),
                input: json!({"subject": "alex", "content": "likes morning meetings"}),
                extra: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "saved".to_string(),
            }],
        },
    ];

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: String::new(), // resume ignores it entirely
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: Some(crate::Resume {
            round_id: "test-round".into(),
            messages,
            completed_iterations: 3,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
            max_iterations: None,
        }),
    })
    .await
    .expect("the resume should finish");

    assert_eq!(result.round_id, "test-round");
    assert_eq!(result.reply, "carrying on from the last round, it is done.");
    assert_eq!(result.iterations, 4, "3 finished iterations plus 1 new one");
    assert!(
        result.tool_calls.is_empty(),
        "a tool that already ran never runs again"
    );
    assert_eq!(
        result.usage.input_tokens, 110,
        "the two ledgers added together (100 + 10)"
    );
    // The request carried the checkpointed memory verbatim — no second user message appended.
    let sent = mock.received.lock().unwrap()[0].clone();
    assert_eq!(sent.messages.len(), 3);
}

// ---- One round, several calls: read-only together, writes alone ------------------------

/// A tool that answers after `ms` — long enough that concurrency is measurable.
fn slow_tool(name: &str, ms: u64, answer: &str) -> joyczl_tools::Tool {
    let answer = answer.to_string();
    joyczl_tools::Tool {
        name: name.to_string(),
        description: "for the test: sleep a while, then answer".to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(move |_ctx: joyczl_tools::ToolCtx, _args| {
            let answer = answer.clone();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(answer)
            })
        }),
    }
}

/// A model response that asks for two calls in the same round.
fn two_calls(first: (&str, &str), second: (&str, &str)) -> joyczl_provider::CreateResponse {
    use joyczl_provider::{ContentBlock, CreateResponse, StopReason, Usage};
    CreateResponse {
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        content: vec![
            ContentBlock::ToolUse {
                id: first.0.to_string(),
                name: first.1.to_string(),
                input: json!({}),
                extra: None,
            },
            ContentBlock::ToolUse {
                id: second.0.to_string(),
                name: second.1.to_string(),
                input: json!({}),
                extra: None,
            },
        ],
    }
}

async fn run_one_round(
    mock: &Mock,
    tools: &joyczl_tools::ToolRegistry,
) -> (crate::LoopResult, std::time::Duration) {
    run_with(mock, tools, None).await
}

async fn run_with(
    mock: &Mock,
    tools: &joyczl_tools::ToolRegistry,
    observer: Option<crate::Observer>,
) -> (crate::LoopResult, std::time::Duration) {
    let ctx = ctx().await; // building the store is not timed: the tool batch is what is measured
                           // tiktoken builds its BPE table on the first estimate (~180ms, once) — warm it up here,
                           // otherwise the cold start lands inside the "tool batch" timing and the measured concurrency
                           // is fiction.
    joyczl_provider::tokens::estimate_text("warm");
    let started = std::time::Instant::now();
    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: mock,
        model: "test-model",
        system: "You are Joy".to_string(),
        history: vec![],
        user_message: "do two things at once".to_string(),
        tools,
        ctx,
        max_iterations: 5,
        max_tokens: 1024,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
    .expect("the loop finishes");
    (result, started.elapsed())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_tools_in_one_round_run_together() {
    let mock = Mock::new(vec![
        two_calls(("call-1", "slow_a"), ("call-2", "slow_b")),
        Mock::text("everything is done."),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(slow_tool("slow_a", 200, "A"));
    tools.register(slow_tool("slow_b", 200, "B"));

    let (result, elapsed) = run_one_round(&mock, &tools).await;

    assert_eq!(result.tool_calls.len(), 2);
    assert!(
        elapsed < std::time::Duration::from_millis(360),
        "two 200 ms read-only tools have to run concurrently (serial would be 400 ms+): {elapsed:?}"
    );
}

#[tokio::test]
async fn results_are_reported_in_request_order_not_completion_order() {
    let mock = Mock::new(vec![
        // The slow one is asked for first and finishes last.
        two_calls(("call-slow", "slow_a"), ("call-fast", "slow_b")),
        Mock::text("everything is done."),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(slow_tool("slow_a", 120, "A"));
    tools.register(slow_tool("slow_b", 5, "B"));

    let (result, _) = run_one_round(&mock, &tools).await;

    let names: Vec<&str> = result.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["slow_a", "slow_b"],
        "the tool ledger keeps request order"
    );
    let sent = mock.received.lock().unwrap().pop().unwrap();
    let blocks = &sent
        .messages
        .last()
        .expect("the last message is the tool result")
        .content;
    let ids: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["call-slow", "call-fast"],
        "what goes back to the model keeps order too"
    );
}

#[tokio::test]
async fn an_exclusive_tool_is_a_barrier() {
    let mock = Mock::new(vec![
        two_calls(("call-1", "reader"), ("call-2", "writer")),
        two_calls(("call-3", "reader2"), ("call-4", "reader3")),
        Mock::text("everything is done."),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(slow_tool("reader", 120, "R"));
    tools.register_exclusive(slow_tool("writer", 120, "W"));
    tools.register(slow_tool("reader2", 120, "R2"));
    tools.register(slow_tool("reader3", 120, "R3"));

    let (result, elapsed) = run_one_round(&mock, &tools).await;

    assert_eq!(result.tool_calls.len(), 4);
    // reader (120) → writer barrier (120) → reader2+reader3 together (120) ≈ 360ms.
    // If the barrier were gone the two reader batches would overlap the writer: ~240ms.
    assert!(
        elapsed >= std::time::Duration::from_millis(330),
        "an exclusive tool has to be a barrier between what comes before and after: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_streaming_tool_emits_deltas_and_still_returns_the_real_result() {
    use std::sync::Mutex;

    let events: Arc<Mutex<Vec<crate::LoopEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let observer: crate::Observer = Arc::new(move |event| {
        collected.lock().unwrap().push(event);
    });

    let mock = Mock::new(vec![
        Mock::tool_use("call-1", "streamer", json!({})),
        Mock::text("received."),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(joyczl_tools::Tool {
        name: "streamer".to_string(),
        description: "report as it goes".to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|ctx: joyczl_tools::ToolCtx, _args| {
            Box::pin(async move {
                // A real tool (run_command) writes stdout here; the fake just proves the
                // wiring: the sink reaches the loop, the return value stays the answer.
                if let Some(sink) = &ctx.on_delta {
                    sink("the first paragraph");
                    sink("the second paragraph");
                }
                Ok("the full result".to_string())
            })
        }),
    });

    let (result, _) = run_with(&mock, &tools, Some(observer)).await;

    let events = events.lock().unwrap();
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            crate::LoopEvent::ToolDelta { chunk, .. } => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec!["the first paragraph", "the second paragraph"],
        "deltas go out in arrival order"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            crate::LoopEvent::ToolDelta { tool_use_id, .. } if tool_use_id == "call-1"
        )),
        "a delta has to carry the call id, or the front end cannot tell which row to update when parallel"
    );
    let finished: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            crate::LoopEvent::Tool { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec!["the full result"],
        "the Tool event still carries only the final result"
    );
    assert_eq!(result.tool_calls[0].output, "the full result");
}

#[tokio::test]
async fn uncertain_effect_stops_remaining_tools_and_keeps_the_intent_checkpoint() {
    let mock = Mock::new(vec![
        two_calls(("first", "uncertain"), ("second", "must_not_run")),
        Mock::text("must not be called"),
    ]);
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register_exclusive(joyczl_tools::Tool {
        name: "uncertain".into(),
        description: "unknown remote effect".into(),
        input_schema: json!({"type":"object"}),
        handler: Arc::new(|ctx, _| {
            Box::pin(async move {
                ctx.on_uncertain.as_ref().unwrap()("remote outcome unknown");
                Ok("Error: remote outcome unknown".into())
            })
        }),
    });
    let capture = ran.clone();
    tools.register_exclusive(joyczl_tools::Tool {
        name: "must_not_run".into(),
        description: "later effect".into(),
        input_schema: json!({"type":"object"}),
        handler: Arc::new(move |_, _| {
            let capture = capture.clone();
            Box::pin(async move {
                capture.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok("ran".into())
            })
        }),
    });
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let captured = checkpoints.clone();
    let sink: crate::CheckpointSink = Arc::new(move |checkpoint| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().unwrap().push(checkpoint);
            Ok(())
        })
    });
    let result = run(Turn {
        system_prefix: None,
        user_images: vec![],
        client: &mock,
        model: "test",
        system: "test".into(),
        history: vec![],
        user_message: "work".into(),
        tools: &tools,
        ctx: ctx().await,
        max_iterations: 5,
        max_tokens: 600,
        token_budget: None,
        tool_result_budget: Default::default(),
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: Some(sink),
        steering: None,
        resume: None,
    })
    .await
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::UncertainEffect);
    assert_eq!(mock.received.lock().unwrap().len(), 1);
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(result.tool_calls.len(), 1);
    let saved = checkpoints.lock().unwrap();
    assert_eq!(
        saved.len(),
        2,
        "initial input and intent only; unknown results must not become resumable success"
    );
    assert!(matches!(
        saved
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .content
            .first(),
        Some(ContentBlock::ToolUse { .. })
    ));
}
