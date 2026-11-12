//! Round-trip tests for the wire formats.
//!
//! No network — the conversion functions are pure; fixed response bodies are fed directly.
//! Real network behavior is covered by smoke/integration tests.

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
            output_tokens: 40,
            ..Usage::default()
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
    // A server adding a new stop reason must not crash Joy.
    assert_eq!(stop_reason("end_turn"), StopReason::EndTurn);
    assert_eq!(stop_reason("tool_use"), StopReason::ToolUse);
    assert_eq!(stop_reason("max_tokens"), StopReason::MaxTokens);
    assert_eq!(stop_reason("pause_turn"), StopReason::Other);
}

#[test]
fn openai_conversion_round_trips_tool_calls() {
    let request = CreateRequest {
        cache_prefix: None,
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
    // system comes first
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "你是 Joy");

    // assistant: text + tool_calls merged into one
    let assistant = &body["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "好的");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let arguments = assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments");
    let parsed: Value = serde_json::from_str(arguments).expect("arguments 是 JSON 字符串");
    assert_eq!(parsed["subject"], "alex");

    // tool results get their own tool message
    let tool = &body["messages"][3];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_1");

    // tools declarations
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "save_note");

    // round trip
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
    // OpenRouter rate limits return 200 + error and no choices.
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

// ---- Local inference (ollama) -----------------------------------------------

#[test]
fn local_provider_needs_no_key() {
    // Resolves even with nothing in the environment — that is the whole point of local
    // inference: offline, zero cost, conversation never leaves this machine.
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
    // LM Studio / vLLM are pointed at exactly via a profile's base_url + model.
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
    // Key-free holds only for local providers. A dev machine may happen to have this key —
    // skip if so, otherwise the assertion tests the machine's .env, not the code.
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

// ---- Token estimation --------------------------------------------------------

#[test]
fn token_estimation_is_monotonic_and_never_zero_for_real_text() {
    assert_eq!(crate::tokens::estimate_text(""), 0);
    assert_eq!(crate::tokens::estimate_text("   "), 0, "空白不算 token");

    let short = crate::tokens::estimate_text("你好");
    let long = crate::tokens::estimate_text("你好，今天天气不错，我们下午去公园散步吧");
    assert!(short > 0, "中文至少要数出 token");
    assert!(long > short, "更长的文本成本更高");

    // Chinese runs high on this encoder, but the magnitude must hold: at least one token per char.
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
    // OpenAI style
    assert!(looks_like_context_overflow(
        400,
        "This model's maximum context length is 200000 tokens"
    ));
    // Anthropic style
    assert!(looks_like_context_overflow(
        400,
        "prompt is too long: 250000 tokens > 200000 maximum"
    ));
    // Chinese gateway
    assert!(looks_like_context_overflow(400, "请求超出上下文长度限制"));
    // Other 4xx must not be misjudged as overflow — a misjudgment wastes a compaction
    assert!(!looks_like_context_overflow(400, "invalid api key"));
    assert!(!looks_like_context_overflow(404, "context length"));
    // 2xx even less so
    assert!(!looks_like_context_overflow(200, "context length"));
}

// ---- Tiered backoff retries ---------------------------------------------------

/// **Finish reading** a request (head + the body its `Content-Length` names) before replying.
///
/// Replying before the read completes and closing the connection knocks over a client still
/// writing its body — mostly harmless on macOS/Linux, RST on Windows: the client sees
/// "error sending request", so "the server did answer" becomes a network error. This test's
/// request carries a full system prompt and tool table; the body exceeds one read, so it
/// reproduces reliably only on Windows.
async fn drain_request(socket: &mut tokio::net::TcpStream) {
    // Imports live inside the function: each test file uses what it needs elsewhere; nothing else pulls this trait.
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

/// A just-good-enough HTTP server: answers one by one per the script.
///
/// Handwritten instead of pulling in a test framework — all we want is a minimal script like
/// "first 429, then 200"; a TcpListener plus a few lines suffices, and `hits` shows the retries.
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
            // The request body is not inspected, but must be **fully read** (see drain_request).
            drain_request(&mut socket).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let (status, body) = responses.get(index).copied().unwrap_or((200, "{}"));
            index += 1;
            let reason = if status == 429 {
                "Too Many Requests"
            } else {
                "OK"
            };
            // The 429 carries `Retry-After: 0`: proves we read it, and keeps the test from really waiting.
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

/// One completed Responses turn with a single text item.
///
/// The openai client goes through the Responses API (`responses.rs`), so this body is the
/// shape that path parses — `status`, `output[]`, and usage under `input_tokens` /
/// `output_tokens`. The Chat Completions shape that used to live here made every test that
/// mocked a successful call fail with "Responses status is missing".
const OK_BODY: &str = r#"{"status":"completed",
                           "output":[{"type":"message","content":[{"type":"output_text","text":"好了"}]}],
                           "usage":{"input_tokens":3,"output_tokens":1}}"#;

/// 429 → backoff → retry succeeds, and it is **announced**.
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
            cache_prefix: None,
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

/// With retries off (`JOY_LLM_RETRIES=0`), a 429 surfaces immediately and honestly.
#[tokio::test]
async fn retries_can_be_switched_off() {
    let server = scripted(vec![(429, r#"{"error":{"message":"slow down"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 0)).expect("解析");
    let (seen, sink) = notice_sink();

    let error = crate::retry::with_note_sink(
        sink,
        resolved.client.create(crate::CreateRequest {
            cache_prefix: None,
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

/// 400 (bad key, bad params) is **not** retried: a retry only fails again the same way.
#[tokio::test]
async fn a_client_error_is_not_retried() {
    let server = scripted(vec![(400, r#"{"error":{"message":"bad key"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 3)).expect("解析");
    let (seen, sink) = notice_sink();

    let error = crate::retry::with_note_sink(
        sink,
        resolved.client.create(crate::CreateRequest {
            cache_prefix: None,
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

/// The retryability decision table itself must be pinned — it is the mechanism's foundation.
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
    // Context overflow means "compact", not "slam into it again".
    assert!(!ProviderError::ContextOverflow {
        status: 400,
        body: "maximum context length".to_string()
    }
    .retryable());
}

// ---- catalog：内置表 + <home>/models-catalog.json 合并（见 catalog.rs）----

#[test]
fn built_in_catalog_matches_the_static_table() {
    let catalog = crate::catalog::Catalog::built_in();
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
    assert!(catalog.lookup("deepseek").is_some());
    assert!(catalog.lookup("ollama").is_some());
}

#[test]
fn load_without_an_override_file_is_built_in() {
    let dir = tempfile::tempdir().expect("临时目录");
    let catalog = crate::catalog::Catalog::load(dir.path());
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
}

#[test]
fn load_patches_an_existing_provider_without_adding_a_row() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"deepseek","context_window":999999}]"#,
    )
    .expect("写目录文件");
    let catalog = crate::catalog::Catalog::load(dir.path());
    let deepseek = catalog.lookup("deepseek").expect("还在");
    assert_eq!(deepseek.context_window, 999_999);
    // 未提字段保留内置值：补丁是补丁，不是重写。
    let built_in = crate::lookup("deepseek").expect("内置表里有 deepseek");
    assert_eq!(deepseek.model, built_in.model);
    assert_eq!(deepseek.wire, built_in.wire);
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
}

#[test]
fn load_appends_a_keyless_custom_provider_and_resolves_it() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"mybox","wire":"openai","model":"qwen3:8b","base_url":"http://127.0.0.1:9/v1"}]"#,
    )
    .expect("写目录文件");
    let catalog = crate::catalog::Catalog::load(dir.path());
    let entry = catalog.lookup("mybox").expect("自定义 provider 已加入");
    assert!(!entry.needs_key(), "未给 key_env = 本地免 key");
    assert_eq!(
        entry.small_model.as_deref(),
        Some("qwen3:8b"),
        "small_model 缺省回落主模型"
    );
    let settings = joyczl_config::Settings {
        provider: "mybox".to_string(),
        ..Default::default()
    };
    let resolved = crate::resolve_with(&settings, &catalog).expect("可解析");
    assert_eq!(resolved.context_window, crate::DEFAULT_CONTEXT_WINDOW);
    assert_eq!(resolved.model, "qwen3:8b");
}

#[test]
fn load_skips_broken_rows_and_survives_broken_json() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"half","wire":"openai"},{"id":"deepseek","context_window":1}]"#,
    )
    .expect("写目录文件");
    let catalog = crate::catalog::Catalog::load(dir.path());
    assert!(catalog.lookup("half").is_none(), "缺 model 的新行被跳过");
    assert_eq!(
        catalog
            .lookup("deepseek")
            .expect("合法行照常生效")
            .context_window,
        1
    );

    let dir2 = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir2.path().join("models-catalog.json"), "{not json").expect("写坏文件");
    let catalog2 = crate::catalog::Catalog::load(dir2.path());
    assert_eq!(
        catalog2.entries().len(),
        crate::PROVIDERS.len(),
        "坏文件退回内置表，不炸启动"
    );
}

#[test]
fn resolve_with_an_unknown_provider_lists_catalog_ids() {
    let catalog = crate::catalog::Catalog::built_in();
    let settings = joyczl_config::Settings {
        provider: "不存在".to_string(),
        ..Default::default()
    };
    let error = crate::resolve_with(&settings, &catalog).expect_err("应当报错");
    assert!(error.contains("Options"), "{error}");
    assert!(error.contains("deepseek"), "{error}");
}

/// 一套模型自己声明视觉，优先于厂商默认：同一家既有看图模型也有纯文本模型。
#[test]
fn a_profile_vision_declaration_beats_the_vendor_default() {
    let catalog = crate::catalog::Catalog::built_in();
    let base = joyczl_config::Settings {
        provider: "deepseek".to_string(),
        api_key: Some("sk-test".to_string()),
        ..Default::default()
    };

    let plain = crate::resolve_with(&base, &catalog).expect("可解析");
    assert!(!plain.vision, "没声明就跟着目录行（deepseek 默认不看图）");

    let declared = crate::resolve_with(
        &joyczl_config::Settings {
            vision: Some(true),
            ..base.clone()
        },
        &catalog,
    )
    .expect("可解析");
    assert!(declared.vision, "这套模型声明了视觉，就该放行图片");

    // 反方向也要能收：openai 的目录行是 true，声明 false 就成了纯文本。
    let off = crate::resolve_with(
        &joyczl_config::Settings {
            provider: "openai".to_string(),
            api_key: Some("sk-test".to_string()),
            vision: Some(false),
            ..Default::default()
        },
        &catalog,
    )
    .expect("可解析");
    assert!(!off.vision, "声明的 false 不该被厂商默认盖回去");
}

// ---- 多模态：Image 块在两条 wire 上的形状 ----

#[test]
fn openai_wire_sends_images_as_data_uris_in_array_content() {
    let request = CreateRequest {
        cache_prefix: None,
        model: "gpt-5.5".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "这张图里是什么？".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
            ],
        }],
        tools: Vec::new(),
        max_tokens: 100,
    };
    let body = crate::openai::to_openai(&request);
    let content = body["messages"][0]["content"].clone();
    assert!(content.is_array(), "带图消息必须是数组 content：{content}");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );

    // 无图消息保持纯字符串 content（对既有客户端/测试零扰动）。
    let plain = CreateRequest {
        cache_prefix: None,
        model: "gpt-5.5".to_string(),
        system: None,
        messages: vec![Message::user_text("纯文本")],
        tools: Vec::new(),
        max_tokens: 100,
    };
    let body = crate::openai::to_openai(&plain);
    assert_eq!(body["messages"][0]["content"], "纯文本");
}

#[test]
fn anthropic_wire_wraps_images_in_a_source_object() {
    let messages = vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "看这张图".to_string(),
            },
            ContentBlock::Image {
                media_type: "image/jpeg".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        ],
    }];
    let wire = crate::anthropic::anthropic_messages(&messages);
    let image = &wire[0]["content"][1];
    assert_eq!(image["type"], "image");
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["media_type"], "image/jpeg");
    assert_eq!(image["source"]["data"], "aGVsbG8=");
    // 文本块保持 serde 直通的形状。
    assert_eq!(wire[0]["content"][0]["type"], "text");
}

#[test]
fn image_token_estimate_is_byte_based_and_clamped() {
    use crate::tokens::estimate_messages;
    let tiny = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "aGk=".to_string(), // 2 字节原始数据
        }],
    }];
    // 小图不约成零：clamp 下限 200。
    assert_eq!(estimate_messages(&tiny), 200 + 4);

    let big = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "A".repeat(8_000_000), // ~6MB 原始数据
        }],
    }];
    // 超大图不失控：clamp 上限 8000。
    assert_eq!(estimate_messages(&big), 8_000 + 4);
}

#[test]
fn thinking_content_survives_tool_rounds_without_becoming_answer_text() {
    let response=from_openai(&json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"reasoning_content":"private continuation","tool_calls":[{"id":"call","type":"function","function":{"name":"lookup","arguments":"{}"}}]}}]}).to_string()).unwrap();
    assert_eq!(response.text(), "");
    let messages = vec![Message {
        role: Role::Assistant,
        content: response.content,
    }];
    let messages = serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
    let request = CreateRequest {
        model: "test".into(),
        cache_prefix: None,
        system: None,
        messages,
        tools: vec![],
        max_tokens: 200,
    };
    let body = to_openai(&request);
    assert_eq!(
        body["messages"][0]["reasoning_content"],
        "private continuation"
    );
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call");
    assert!(body["messages"][0]["content"].is_null());
}
