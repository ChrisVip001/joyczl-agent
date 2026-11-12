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
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here
    joyczl_tools::ToolCtx {
        on_uncertain: None,
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
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "记住 alex 喜欢早会".to_string(),
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
    .expect("loop 跑完")
}

/// 0.16：运行中补的那一句，进的是**下一次**模型调用，已经跑完的工具不重跑。
///
/// 这就是 steering 与 "取消重来" 的分界线：不是重放这一轮，而是往工作记忆末尾接一句话。
#[tokio::test]
async fn words_added_mid_turn_join_the_next_model_call_and_do_not_rerun_tools() {
    let steering = crate::Steering::new();
    let mock = Mock::new(vec![
        Mock::tool_use("call-1", "list_dir", json!({ "path": "." })),
        Mock::text("看完了。"),
    ]);
    let tools = handlers::build_default();

    // 从 observer 里推：正是 turn/steer 的真实时序 —— 第一次模型调用之后被接受、第二次看得见。
    let observer: Option<crate::Observer> = {
        let steering = steering.clone();
        Some(Arc::new(move |event: LoopEvent| {
            if let LoopEvent::Llm { iteration: 1, .. } = event {
                steering.lock().expect("锁").push("顺便看下 README");
            }
        }))
    };

    let result = run(Turn {
        system_prefix: None,
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "看看目录".to_string(),
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
    .expect("loop 跑完");

    assert_eq!(result.tool_calls.len(), 1, "工具只该跑一次");
    let received = mock.received.lock().expect("锁");
    assert_eq!(received.len(), 2, "两句话 = 两次模型调用");
    let last = received[1].messages.last().expect("第二次调用非空");
    assert!(matches!(last.role, Role::User));
    assert!(
        last.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "顺便看下 README")),
        "{:?}",
        last.content
    );
    // 全部记录都留着 —— 转录要能看见那句补充（哪怕它是在收尾前一刻被接受的）。
    assert_eq!(
        steering.lock().expect("锁").all(),
        vec!["顺便看下 README".to_string()]
    );
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
        .contains("no tool named 'hack_the_planet'"));
    assert_eq!(result.iterations, 2);
}

#[tokio::test]
async fn the_last_iteration_is_a_wrap_up_not_a_cliff() {
    // 预算用尽时不再硬停：最后一次调用把工具收起，让模型用三句话收尾。
    //
    // 这条规则来自一次真实的浪费：10 步、21 万输入 token 之后，用户只拿到
    // 「我还没做完就到了迭代上限——试试把请求拆小一点」。Claude Code 的循环根本没有
    // 交互式上限（模型不调工具就结束），所以我们也把「上限」从失败改成收尾。
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("读到了时间；没做完的是清理；需要你确认删哪些。"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "现在几点".to_string(),
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
    .expect("收尾轮不该报错");

    assert_eq!(result.iterations, 2);
    assert_eq!(
        result.reply, "读到了时间；没做完的是清理；需要你确认删哪些。",
        "收尾轮的话要原样交给用户，而不是被换成一句模板"
    );

    // 最后一次请求必须**没有工具**，并且带上收尾指令（否则模型会继续调工具）。
    let requests = mock.received.lock().expect("锁");
    let last = requests.last().expect("至少要请求一次");
    assert!(
        last.tools.is_empty(),
        "收尾轮不该再给工具：{} 个",
        last.tools.len()
    );
    let tail = serde_json::to_string(&last.messages).expect("序列化");
    assert!(
        tail.contains("Budget note"),
        "收尾指令要进上下文：{tail:.200}"
    );
}

#[tokio::test]
async fn the_token_budget_also_turns_the_next_round_into_a_wrap_up() {
    // 步数上限管「跑几轮」，token 预算管「跑多贵」。到了预算就走同一条收尾路径 ——
    // 并且要说清是钱包到了，不是活儿干完了。
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("预算到了，我先收尾：时间读到了，剩下的清理没做。"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "现在几点".to_string(),
        tools: &tools,
        ctx,
        // 步数还够（10 轮），钱包只够一轮半。
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
    .expect("预算收尾不该报错");

    assert_eq!(result.iterations, 2, "第一轮照常，第二轮直接收尾");
    let requests = mock.received.lock().expect("锁");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].tools.is_empty(),
        "预算收尾那一轮不该再给工具：{} 个",
        requests[1].tools.len()
    );
    let tail = serde_json::to_string(&requests[1].messages).expect("序列化");
    assert!(
        tail.contains("token budget is spent"),
        "要明说是预算停的：{tail:.200}"
    );
}

#[tokio::test]
async fn without_a_token_budget_the_loop_keeps_its_tools() {
    // 默认关掉：不设预算时第二轮仍然带着工具（否则「默认不变」这句话就不成立）。
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "current_time", json!({})),
        Mock::text("好了"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let _ = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "现在几点".to_string(),
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
    .expect("两轮就该结束");

    let requests = mock.received.lock().expect("锁");
    assert!(!requests[1].tools.is_empty(), "没有预算时第二轮照常带工具");
    let tail = serde_json::to_string(&requests[1].messages).expect("序列化");
    assert!(!tail.contains("Budget note"), "没预算就不该有收尾指令");
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
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
        on_uncertain: None,
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
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "把那段文本拿来看看".to_string(),
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
        stub.contains("truncated") && stub.chars().count() < 5_000,
        "超限结果该换成桩：{} 字符",
        stub.chars().count()
    );
    assert!(stub.contains("full output at spill/"), "{stub}");

    // The file the stub points to holds the full original (100k chars).
    let relative = stub
        .split("full output at ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("路径");
    let saved = std::fs::read_to_string(home.path().join(relative)).expect("读回落盘");
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
            json!({"subject": "alex", "content": "喜欢早会"}),
        ),
        Mock::text("记好了。"),
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
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "记住 alex 喜欢早会".to_string(),
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
    .expect("loop 跑完");

    assert_eq!(result.iterations, 2);
    let cps = checkpoints.lock().unwrap();
    assert!(cps.iter().all(|cp| cp.round_id == result.round_id));
    assert!(!result.round_id.is_empty());
    assert_eq!(cps.len(), 3, "initial input, execution intent, tool result");
    assert_eq!(cps[0].iterations, 0);
    // The checkpoint's working memory holds the tool round: assistant's tool request + the result.
    let last = cps[2].messages.last().expect("有结果块");
    assert!(matches!(
        last.content.first(),
        Some(joyczl_provider::ContentBlock::ToolResult { .. })
    ));
}

/// Resume: the loop continues the checkpointed working memory — no user message is re-added,
/// no finished tool is re-run, and the pre-crash usage joins the final account.
#[tokio::test]
async fn a_resumed_turn_continues_without_rerunning_finished_tools() {
    let mock = Mock::new(vec![Mock::text("接着上一轮，已经办完了。")]);
    let tools = handlers::build_default();

    // The checkpoint's working memory: user asked, assistant requested a tool, result came back.
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "记住 alex 喜欢早会".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "save_note".to_string(),
                input: json!({"subject": "alex", "content": "喜欢早会"}),
                extra: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "已保存".to_string(),
            }],
        },
    ];

    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: &mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
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
    .expect("续跑完成");

    assert_eq!(result.round_id, "test-round");
    assert_eq!(result.reply, "接着上一轮，已经办完了。");
    assert_eq!(result.iterations, 4, "3 个已完成迭代 + 1 个新迭代");
    assert!(result.tool_calls.is_empty(), "已完成的工具绝不重跑");
    assert_eq!(result.usage.input_tokens, 110, "两段账加在一起（100 + 10）");
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
        description: "测试用：睡一会儿再回答".to_string(),
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
    let ctx = ctx().await; // 建库不算进计时：测的是工具批次
                           // tiktoken 的 BPE 词表是首次估算时才构造的（约 180ms，一次性）——这里预热掉，
                           // 否则冷启动会被算进"工具批次"的耗时里，测出来的并发是假的。
    joyczl_provider::tokens::estimate_text("warm");
    let started = std::time::Instant::now();
    let result = run(Turn {
        system_prefix: None,
        user_images: Vec::new(),
        client: mock,
        model: "test-model",
        system: "你是 Joy".to_string(),
        history: vec![],
        user_message: "同时做两件事".to_string(),
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
    .expect("loop 跑完");
    (result, started.elapsed())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_tools_in_one_round_run_together() {
    let mock = Mock::new(vec![
        two_calls(("call-1", "slow_a"), ("call-2", "slow_b")),
        Mock::text("都做完了。"),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(slow_tool("slow_a", 200, "A"));
    tools.register(slow_tool("slow_b", 200, "B"));

    let (result, elapsed) = run_one_round(&mock, &tools).await;

    assert_eq!(result.tool_calls.len(), 2);
    assert!(
        elapsed < std::time::Duration::from_millis(360),
        "两个 200ms 的只读工具必须并发（串行要 400ms+）：{elapsed:?}"
    );
}

#[tokio::test]
async fn results_are_reported_in_request_order_not_completion_order() {
    let mock = Mock::new(vec![
        // The slow one is asked for first and finishes last.
        two_calls(("call-slow", "slow_a"), ("call-fast", "slow_b")),
        Mock::text("都做完了。"),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(slow_tool("slow_a", 120, "A"));
    tools.register(slow_tool("slow_b", 5, "B"));

    let (result, _) = run_one_round(&mock, &tools).await;

    let names: Vec<&str> = result.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["slow_a", "slow_b"], "工具账按请求顺序记");
    let sent = mock.received.lock().unwrap().pop().unwrap();
    let blocks = &sent.messages.last().expect("最后一条是工具结果").content;
    let ids: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["call-slow", "call-fast"], "回灌给模型的也保序");
}

#[tokio::test]
async fn an_exclusive_tool_is_a_barrier() {
    let mock = Mock::new(vec![
        two_calls(("call-1", "reader"), ("call-2", "writer")),
        two_calls(("call-3", "reader2"), ("call-4", "reader3")),
        Mock::text("都做完了。"),
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
        "独占工具必须是把前后隔开的屏障：{elapsed:?}"
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
        Mock::text("收到。"),
    ]);
    let mut tools = joyczl_tools::ToolRegistry::new();
    tools.register(joyczl_tools::Tool {
        name: "streamer".to_string(),
        description: "边跑边报".to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|ctx: joyczl_tools::ToolCtx, _args| {
            Box::pin(async move {
                // A real tool (run_command) writes stdout here; the fake just proves the
                // wiring: the sink reaches the loop, the return value stays the answer.
                if let Some(sink) = &ctx.on_delta {
                    sink("第一段");
                    sink("第二段");
                }
                Ok("完整结果".to_string())
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
    assert_eq!(deltas, vec!["第一段", "第二段"], "增量要按到达顺序发出去");
    assert!(
        events.iter().any(|event| matches!(
            event,
            crate::LoopEvent::ToolDelta { tool_use_id, .. } if tool_use_id == "call-1"
        )),
        "增量必须带 call id，否则并行时前端不知道更新哪一行"
    );
    let finished: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            crate::LoopEvent::Tool { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec!["完整结果"], "Tool 事件仍然只发最终结果");
    assert_eq!(result.tool_calls[0].output, "完整结果");
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
