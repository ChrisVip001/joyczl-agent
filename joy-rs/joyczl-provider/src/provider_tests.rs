//! wire format 的往返测试。
//!
//! 不联网 —— 转换函数是纯函数，直接喂固定的应答体。
//! 真正的网络行为由 smoke/集成测试覆盖。

use serde_json::{json, Value};

use super::anthropic::{parse_message, stop_reason};
use super::openai::{from_openai, to_openai};
use crate::{ContentBlock, CreateRequest, Message, Role, StopReason, ToolSchema, Usage};

#[test]
fn anthropic_parse_handles_text_and_tool_use() {
    let body = json!({
        "content": [
            {"type": "text", "text": "让我查一下"},
            {"type": "tool_use", "id": "tu_1", "name": "save_note",
             "input": {"subject": "alex", "content": "likes mornings"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "output_tokens": 40}
    });

    let response = parse_message(&body).expect("解析");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 100,
            output_tokens: 40
        }
    );
    assert_eq!(response.tool_uses().len(), 1);
    let (id, name, input) = response.tool_uses()[0];
    assert_eq!((id, name), ("tu_1", "save_note"));
    assert_eq!(input["subject"], "alex");
    assert_eq!(response.text(), "让我查一下");
}

#[test]
fn anthropic_parse_survives_missing_usage() {
    let body = json!({"content": [{"type": "text", "text": "hi"}]});
    let response = parse_message(&body).expect("解析");
    assert_eq!(response.usage, Usage::default());
    assert_eq!(response.stop_reason, StopReason::EndTurn);
}

#[test]
fn stop_reason_unknown_value_becomes_other() {
    // 服务端加新停因不该把 Joy 打崩。
    assert_eq!(stop_reason("end_turn"), StopReason::EndTurn);
    assert_eq!(stop_reason("tool_use"), StopReason::ToolUse);
    assert_eq!(stop_reason("max_tokens"), StopReason::MaxTokens);
    assert_eq!(stop_reason("pause_turn"), StopReason::Other);
}

#[test]
fn openai_conversion_round_trips_tool_calls() {
    let request = CreateRequest {
        model: "gpt-5.5".to_string(),
        system: Some("你是 Joy".to_string()),
        messages: vec![
            Message::user_text("记住 alex 喜欢早会"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "好的".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "save_note".to_string(),
                        input: json!({"subject": "alex", "content": "likes mornings"}),
                        extra: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "已保存到 .joy/state.db 的 facts 表".to_string(),
                }],
            },
        ],
        tools: vec![ToolSchema {
            name: "save_note".to_string(),
            description: "记住一件事".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        max_tokens: 1024,
    };

    let body = to_openai(&request);
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["max_completion_tokens"], 1024);
    // system 是第一条
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "你是 Joy");

    // assistant：文本 + tool_calls 合成一条
    let assistant = &body["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "好的");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let arguments = assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments");
    let parsed: Value = serde_json::from_str(arguments).expect("arguments 是 JSON 字符串");
    assert_eq!(parsed["subject"], "alex");

    // tool 结果单独一条 tool 消息
    let tool = &body["messages"][3];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_1");

    // tools 声明
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "save_note");

    // 往返
    let response_body = json!({
        "choices": [{"message": {
            "content": "已记住。",
            "tool_calls": [{"id": "call_2", "type": "function",
                            "function": {"name": "search_memory", "arguments": "{\"query\":\"alex\"}"}}]
        }}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 12}
    });
    let response = from_openai(&serde_json::to_string(&response_body).unwrap()).expect("解析");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 50);
    assert_eq!(response.text(), "已记住。");
    let (id, name, input) = response.tool_uses()[0];
    assert_eq!((id, name), ("call_2", "search_memory"));
    assert_eq!(input["query"], "alex");
}

#[test]
fn openai_rate_limit_error_body_becomes_api_error_not_parse_error() {
    // OpenRouter 限流时回 200 + error、没有 choices。
    let body = json!({"error": {"message": "Rate limit exceeded", "code": 429}});
    let error = from_openai(&serde_json::to_string(&body).unwrap()).expect_err("应当报错");
    assert!(error.to_string().contains("Rate limit"), "{error}");
}

#[test]
fn resolve_rejects_unknown_provider_with_the_list() {
    let settings = joyczl_config::Settings {
        provider: "nope".to_string(),
        ..Default::default()
    };
    let error = crate::resolve(&settings).expect_err("应当报错");
    assert!(
        error.contains("anthropic"),
        "错误里该列出可选 provider：{error}"
    );
}

// ---- 本地推理（ollama）------------------------------------------------------

#[test]
fn local_provider_needs_no_key() {
    // 环境里什么都没有也解析得出来 —— 这正是本地推理的意义：
    // 离线、零成本、对话不出这台机器。
    let settings = joyczl_config::Settings {
        provider: "ollama".to_string(),
        ..Default::default()
    };
    let resolved = crate::resolve(&settings).expect("本地 provider 不需要 key");
    assert_eq!(resolved.provider_id, "ollama");
    assert_eq!(resolved.model, "qwen3:8b");
    assert_eq!(resolved.small_model, "qwen3:4b");
}

#[test]
fn local_provider_is_openai_wire_at_the_local_endpoint() {
    let info = crate::lookup("ollama").expect("目录里有 ollama");
    assert_eq!(info.wire, crate::Wire::OpenAi);
    assert!(!info.needs_key(), "空 key_env 就是「不需要 key」的声明");
    assert_eq!(info.base_url, Some("http://127.0.0.1:11434/v1"));
}

#[test]
fn local_provider_still_honours_explicit_overrides() {
    // LM Studio / vLLM 就是靠 JOY_BASE_URL + JOY_MODEL 指过去的。
    let settings = joyczl_config::Settings {
        provider: "ollama".to_string(),
        base_url: Some("http://127.0.0.1:1234/v1".to_string()),
        model: Some("my-local-model".to_string()),
        small_model: Some("my-local-small".to_string()),
        ..Default::default()
    };
    let resolved = crate::resolve(&settings).expect("解析");
    assert_eq!(resolved.model, "my-local-model");
    assert_eq!(resolved.small_model, "my-local-small");
}

#[test]
fn a_cloud_provider_still_requires_a_key() {
    // 免 key 只对本地成立。开发机上可能恰好配着这个 key —— 有就跳过，
    // 不然断言的不是代码而是这台机器的 .env。
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return;
    }
    let settings = joyczl_config::Settings {
        provider: "anthropic".to_string(),
        api_key: None,
        ..Default::default()
    };
    let error = crate::resolve(&settings).expect_err("云端缺 key 必须报错");
    assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
}

// ---- token 估算 ------------------------------------------------------------

#[test]
fn token_estimation_is_monotonic_and_never_zero_for_real_text() {
    assert_eq!(crate::tokens::estimate_text(""), 0);
    assert_eq!(crate::tokens::estimate_text("   "), 0, "空白不算 token");

    let short = crate::tokens::estimate_text("你好");
    let long = crate::tokens::estimate_text("你好，今天天气不错，我们下午去公园散步吧");
    assert!(short > 0, "中文至少要数出 token");
    assert!(long > short, "更长的文本成本更高");

    // 中文在这个编码器下偏高，但量级要对：一个字至少一个 token。
    assert!(crate::tokens::estimate_text("中文测试四个字") >= 4);
}

#[test]
fn tool_declarations_cost_tokens_too() {
    let schema = ToolSchema {
        name: "save_note".to_string(),
        description: "记住一件值得长期记住的事".to_string(),
        input_schema: json!({"type": "object", "properties": {"a": {"type": "string"}}}),
    };
    assert!(crate::tokens::estimate_tools(std::slice::from_ref(&schema)) > 0);
    assert_eq!(
        crate::tokens::estimate_tools(&[]),
        0,
        "没有工具就不该占预算"
    );
    let two = crate::tokens::estimate_tools(&[schema.clone(), schema.clone()]);
    let one = crate::tokens::estimate_tools(std::slice::from_ref(&schema));
    assert!(
        two > one,
        "工具越多越贵 —— 这正是 MCP 接一堆工具时压缩会提前的原因"
    );
}

#[test]
fn context_overflow_is_recognised_from_the_wordings_providers_actually_use() {
    use crate::error::looks_like_context_overflow;
    // OpenAI 风格
    assert!(looks_like_context_overflow(
        400,
        "This model's maximum context length is 200000 tokens"
    ));
    // Anthropic 风格
    assert!(looks_like_context_overflow(
        400,
        "prompt is too long: 250000 tokens > 200000 maximum"
    ));
    // 中文网关
    assert!(looks_like_context_overflow(400, "请求超出上下文长度限制"));
    // 别的 4xx 不该被误判成溢出 —— 误判会带来一次没意义的压缩
    assert!(!looks_like_context_overflow(400, "invalid api key"));
    assert!(!looks_like_context_overflow(404, "context length"));
    // 2xx 更不该
    assert!(!looks_like_context_overflow(200, "context length"));
}

// ---- 分级退避重试 ------------------------------------------------------------

/// 把一次请求**读完**（头部 + `Content-Length` 指的 body）再回话。
///
/// 不等读完就回、然后关连接，会把还在写 body 的客户端顶掉 —— macOS/Linux 上
/// 多半没事，Windows 上表现为 RST，客户端那边看到的是
/// 「error sending request」，于是「服务器明明回了」变成一条网络错误。
/// 这个测试的请求带着整份 system prompt 与工具表，body 超过一次 read 的量，
/// 所以它只在 Windows 上稳定复现。
async fn drain_request(socket: &mut tokio::net::TcpStream) {
    // 导入写在函数里：这两个测试文件各自在别处按需 use，别处不引这个 trait。
    use tokio::io::AsyncReadExt;

    let mut seen: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                seen.extend_from_slice(&chunk[..read]);
                let Some(head_end) = seen.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&seen[..head_end]).to_lowercase();
                let want: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if seen.len() >= head_end + 4 + want {
                    return;
                }
            }
        }
    }
}

/// 一个只够用的 HTTP 服务器：按剧本逐个应答。
///
/// 手写而不是引测试框架 —— 我们要的就是「第一次 429、第二次 200」这种最小
/// 剧本，一个 TcpListener 加几行文本足够，而且能从 hits 上直接看出重试了几次。
struct Scripted {
    base_url: String,
    hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

async fn scripted(responses: Vec<(u16, &'static str)>) -> Scripted {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("监听端口");
    let addr = listener.local_addr().expect("本地地址");
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();

    tokio::spawn(async move {
        let mut index = 0usize;
        while let Ok((mut socket, _)) = listener.accept().await {
            // 请求内容不看，但必须**读完**（见 drain_request 的说明）。
            drain_request(&mut socket).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let (status, body) = responses.get(index).copied().unwrap_or((200, "{}"));
            index += 1;
            let reason = if status == 429 {
                "Too Many Requests"
            } else {
                "OK"
            };
            // 429 带一个 `Retry-After: 0`：既验证我们读了它，又让测试不真等。
            let extra = if status == 429 {
                "Retry-After: 0\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    Scripted {
        base_url: format!("http://{addr}"),
        hits,
    }
}

fn openai_settings(server: &Scripted, retries: i32) -> joyczl_config::Settings {
    joyczl_config::Settings {
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: Some(server.base_url.clone()),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        llm_retries: retries,
        ..Default::default()
    }
}

fn notice_sink() -> (
    std::sync::Arc<std::sync::Mutex<Vec<crate::retry::RetryNotice>>>,
    crate::retry::NoteSink,
) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: crate::retry::NoteSink = {
        let seen = seen.clone();
        std::sync::Arc::new(move |notice: crate::retry::RetryNotice| {
            seen.lock().expect("锁").push(notice);
        })
    };
    (seen, sink)
}

const OK_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"好了"}}],
                           "usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

/// 429 → 退避 → 重试成功，而且**说了出去**。
#[tokio::test]
async fn a_rate_limit_is_retried_and_announced() {
    let server = scripted(vec![
        (429, r#"{"error":{"message":"slow down"}}"#),
        (200, OK_BODY),
    ])
    .await;
    let resolved = crate::resolve(&openai_settings(&server, 2)).expect("解析");
    let (seen, sink) = notice_sink();

    let response = crate::retry::with_note_sink(
        sink,
        resolved.client.create(crate::CreateRequest {
            model: "test-model".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 32,
        }),
    )
    .await
    .expect("第二次该成功");

    assert_eq!(response.text(), "好了");
    assert_eq!(
        server.hits.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "一次 429 加一次成功"
    );
    let seen = seen.lock().expect("锁");
    assert_eq!(seen.len(), 1, "每次重试都必须有一条通知");
    assert!(seen[0].reason.contains("429"), "{:?}", seen[0]);
    assert_eq!(seen[0].delay_ms, 0, "要听服务端 Retry-After 的（这里是 0）");
}

/// 关掉重试（`JOY_LLM_RETRIES=0`）时，429 立刻如实冒上去。
#[tokio::test]
async fn retries_can_be_switched_off() {
    let server = scripted(vec![(429, r#"{"error":{"message":"slow down"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 0)).expect("解析");
    let (seen, sink) = notice_sink();

    let error = crate::retry::with_note_sink(
        sink,
        resolved.client.create(crate::CreateRequest {
            model: "test-model".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 32,
        }),
    )
    .await
    .expect_err("关掉重试就该报错");

    assert!(error.retryable(), "429 是「可以重试」的那一类");
    assert_eq!(server.hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(seen.lock().expect("锁").is_empty(), "没有重试就没有通知");
}

/// 400（key 错、参数错）**不**重试：重试只会以同样方式再失败一次。
#[tokio::test]
async fn a_client_error_is_not_retried() {
    let server = scripted(vec![(400, r#"{"error":{"message":"bad key"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 3)).expect("解析");
    let (seen, sink) = notice_sink();

    let error = crate::retry::with_note_sink(
        sink,
        resolved.client.create(crate::CreateRequest {
            model: "test-model".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 32,
        }),
    )
    .await
    .expect_err("400 要如实报错");

    assert!(!error.retryable(), "400 不属于可重试的一类");
    assert_eq!(
        server.hits.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "一次都不该重试"
    );
    assert!(seen.lock().expect("锁").is_empty());
}

/// 重试类别的判定表本身也要钉住 —— 这是整个机制的地基。
#[test]
fn only_transient_failures_are_retryable() {
    use crate::error::ProviderError;
    let http = |status| ProviderError::from_http(status, "{}".to_string());
    assert!(http(429).retryable(), "限流是典型可重试");
    assert!(http(500).retryable());
    assert!(http(503).retryable());
    assert!(!http(400).retryable());
    assert!(!http(401).retryable());
    assert!(!http(404).retryable());
    assert!(!ProviderError::Api("x".to_string()).retryable());
    assert!(!ProviderError::Parse("x".to_string()).retryable());
    // 上下文溢出是「该压缩」，不是「该再撞一次」。
    assert!(!ProviderError::ContextOverflow {
        status: 400,
        body: "maximum context length".to_string()
    }
    .retryable());
}
