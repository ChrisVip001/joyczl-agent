//! loop 的测试。
//!
//! 用 mock provider 精确编排「模型每轮说什么」，把护栏逐条验证：
//! 自然结束、工具往返、错误不当崩、迭代上限。
//! scripted model：模型说什么完全由测试决定。

use std::sync::{Arc, Mutex};

use joyczl_provider::mock::Mock;
use joyczl_provider::StopReason;
use joyczl_tools::handlers;
use serde_json::json;

use crate::{run, LoopEvent, Turn};

/// 一个真的 state.db + 默认工具集。
async fn ctx() -> joyczl_tools::ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    joyczl_tools::ToolCtx {
        approval: None,
        hooks: None,
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
    // 用户消息进了 prompt，system 也在。
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

    // 工具结果真的被喂回给了模型。
    let second = mock.received.lock().unwrap()[1].clone();
    let has_tool_result = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, joyczl_provider::ContentBlock::ToolResult { .. }))
    });
    assert!(has_tool_result, "第二轮的 messages 里应当有 tool_result");
    // 而且事实也真的写进库了。
    assert_eq!(ctx.facts.search("早会", 4).await.unwrap().len(), 1);
}

#[tokio::test]
async fn tool_error_becomes_text_and_the_loop_continues() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": 42})), // 类型错 → 工具返回 Error: 文本
        Mock::text("参数好像不对，你能再说一遍吗？"),
    ]);
    let tools = handlers::build_default();
    let result = turn(&mock, &tools, ctx().await, None).await;

    assert!(!result.tool_calls[0].ok());
    assert_eq!(result.iterations, 2, "出错后 loop 应当继续，而不是崩掉");
    assert_eq!(result.reply, "参数好像不对，你能再说一遍吗？");
    // 错误文本作为 tool_result 回给了模型。
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
    // 模型永远在调工具、不给文本 —— 必须由护栏硬停，而不是无限转下去。
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
        max_iterations: 2, // 故意卡在两条工具应答上
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
    // mock 拆成两字一块 —— 应当按序到达，且最终应答仍是完整文本。
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

    // observer 也收到了同样的 Text 事件。
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
    // 第一轮流式 + 工具调用，第二轮纯文本 —— 两条路径不能互相干扰。
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

/// 预先拨下取消令牌：loop 一进来就收兵，不发任何模型调用，
/// reply 诚实地说这轮被打断了。
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

/// 打断发生在工具往返之间：save_note 真的执行了，但第二轮模型调用没有发生，
/// 已经拿到的结果带 interrupted 标记回来。
#[tokio::test]
async fn an_interrupt_between_tool_rounds_keeps_what_already_happened() {
    let mock = Mock::new(vec![
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("这句话不该出现。"),
    ]);
    let tools = handlers::build_default();
    let ctx = ctx().await;

    let interrupt = crate::Interrupt::new();
    // 第一轮流式（mock 不拆块，没有增量可监听）—— 用 observer 在第一个
    // 工具事件到达时拨下令牌：此刻第一轮工具已完成，第二轮还没开始。
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

/// 循环护栏接进了 loop：同一调用重复到第 3 次时，喂回模型的文本会多一段提醒，
/// 但 `tool_calls` 里记的仍是**真结果**（trace 与通知该看到真的）。
#[tokio::test]
async fn the_stall_guard_warns_the_model_but_keeps_real_tool_output() {
    // 用 `search_memory` 而不是 `save_note`：后者现在会去重，第一次的答复
    // （「已记住 #1」）和后来的（「已经记过了」）不一样，恰好构不成「三次完全相同」。
    // 护栏要的是**同一个调用、同一份结果**，所以挑一个输出稳定的只读工具。
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

    // 喂回模型的那条 ToolResult 里有提醒……
    let warned = result.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, joyczl_provider::ContentBlock::ToolResult { content, .. }
                     if content.contains("[guard]"))
        })
    });
    assert!(warned, "提醒必须进到模型看得见的地方");

    // ……而记录下来的工具输出还是干净的。
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

/// 轮内预算真的在跑：第二次请求里那条 10 万字符的结果已经被换成桩，
/// 而且桩指向的文件里有完整原文。
#[tokio::test]
async fn an_oversized_tool_result_becomes_a_stub_before_the_next_request() {
    use joyczl_tools::{BoxFut, Tool, ToolRegistry};
    use serde_json::Value;

    let home = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .expect("打开库");
    let ctx = joyczl_tools::ToolCtx {
        approval: None,
        hooks: None,
        session_id: "budget".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home: home.path().to_path_buf(),
    };

    // 一个「MCP 式」的工具：没人管它的输出，回 10 万字符。
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

    // 桩指的文件里是完整原文（10 万字符）。
    let relative = stub
        .split("完整输出在 ")
        .nth(1)
        .and_then(|rest| rest.split('）').next())
        .expect("路径");
    let saved = std::fs::read_to_string(home.path().join(relative)).expect("读回落盘");
    assert_eq!(saved.chars().count(), 100_000);

    // 落盘的文件在会话目录下（`spill/日期/…`）。
    assert!(relative.starts_with("spill/"), "{relative}");
}
