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
            {"type": "text", "text": "let me check"},
            {"type": "tool_use", "id": "tu_1", "name": "save_note",
             "input": {"subject": "alex", "content": "likes mornings"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "output_tokens": 40}
    });

    let response = parse_message(&body).expect("parses");
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
    assert_eq!(response.text(), "let me check");
}

#[test]
fn anthropic_parse_survives_missing_usage() {
    let body = json!({"content": [{"type": "text", "text": "hi"}]});
    let response = parse_message(&body).expect("parse");
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
        system: Some("You are Joy".to_string()),
        messages: vec![
            Message::user_text("remember that alex likes morning meetings"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "fine".to_string(),
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
                    content: "saved to the facts table in .joy/state.db".to_string(),
                }],
            },
        ],
        tools: vec![ToolSchema {
            name: "save_note".to_string(),
            description: "remember one thing".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        max_tokens: 1024,
    };

    let body = to_openai(&request);
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["max_completion_tokens"], 1024);
    // system comes first
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "You are Joy");

    // assistant: text + tool_calls merged into one
    let assistant = &body["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "fine");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let arguments = assistant["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments");
    let parsed: Value = serde_json::from_str(arguments).expect("arguments is a JSON string");
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
            "content": "noted.",
            "tool_calls": [{"id": "call_2", "type": "function",
                            "function": {"name": "search_memory", "arguments": "{\"query\":\"alex\"}"}}]
        }}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 12}
    });
    let response = from_openai(&serde_json::to_string(&response_body).unwrap()).expect("parse");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 50);
    assert_eq!(response.text(), "noted.");
    let (id, name, input) = response.tool_uses()[0];
    assert_eq!((id, name), ("call_2", "search_memory"));
    assert_eq!(input["query"], "alex");
}

#[test]
fn openai_rate_limit_error_body_becomes_api_error_not_parse_error() {
    // OpenRouter rate limits return 200 + error and no choices.
    let body = json!({"error": {"message": "Rate limit exceeded", "code": 429}});
    let error = from_openai(&serde_json::to_string(&body).unwrap()).expect_err("this should fail");
    assert!(error.to_string().contains("Rate limit"), "{error}");
}

#[test]
fn resolve_rejects_unknown_provider_with_the_list() {
    let settings = joyczl_config::Settings {
        provider: "nope".to_string(),
        ..Default::default()
    };
    let error = crate::resolve(&settings).expect_err("it should fail");
    assert!(
        error.contains("anthropic"),
        "the error should list the available providers: {error}"
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
    let resolved = crate::resolve(&settings).expect("a local provider needs no key");
    assert_eq!(resolved.provider_id, "ollama");
    assert_eq!(resolved.model, "qwen3:8b");
    assert_eq!(resolved.small_model, "qwen3:4b");
}

#[test]
fn local_provider_is_openai_wire_at_the_local_endpoint() {
    let info = crate::lookup("ollama").expect("ollama is in the catalog");
    assert_eq!(info.wire, crate::Wire::OpenAi);
    assert!(
        !info.needs_key(),
        "an empty key_env is a declaration that no key is needed"
    );
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
    let resolved = crate::resolve(&settings).expect("parse");
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
    let error = crate::resolve(&settings).expect_err("a cloud provider without a key must fail");
    assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
}

// ---- Token estimation --------------------------------------------------------

#[test]
fn token_estimation_is_monotonic_and_never_zero_for_real_text() {
    assert_eq!(crate::tokens::estimate_text(""), 0);
    assert_eq!(
        crate::tokens::estimate_text("   "),
        0,
        "whitespace is not a token"
    );

    let short = crate::tokens::estimate_text("hello");
    let long = crate::tokens::estimate_text(
        "hello, nice weather today, let us walk in the park this afternoon",
    );
    assert!(
        short > 0,
        "real text has to come out as at least some tokens"
    );
    assert!(long > short, "longer text costs more");

    // A CJK fixture on purpose: this encoder counts CJK high, and the magnitude has to hold —
    // at least one token per character.
    assert!(crate::tokens::estimate_text("中文测试四个字") >= 4);
}

#[test]
fn tool_declarations_cost_tokens_too() {
    let schema = ToolSchema {
        name: "save_note".to_string(),
        description: "remember something worth remembering for a long time".to_string(),
        input_schema: json!({"type": "object", "properties": {"a": {"type": "string"}}}),
    };
    assert!(crate::tokens::estimate_tools(std::slice::from_ref(&schema)) > 0);
    assert_eq!(
        crate::tokens::estimate_tools(&[]),
        0,
        "no tools means no budget taken"
    );
    let two = crate::tokens::estimate_tools(&[schema.clone(), schema.clone()]);
    let one = crate::tokens::estimate_tools(std::slice::from_ref(&schema));
    assert!(
        two > one,
        "more tools cost more — which is exactly why compaction starts earlier once MCP brings a pile"
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
    assert!(
        looks_like_context_overflow(400, "the request exceeds the context length limit"),
        "a Chinese gateway words it in Chinese"
    );
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
        .expect("bind the port");
    let addr = listener.local_addr().expect("the local address");
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
            seen.lock().expect("lock").push(notice);
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
                           "output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],
                           "usage":{"input_tokens":3,"output_tokens":1}}"#;

/// 429 → backoff → retry succeeds, and it is **announced**.
#[tokio::test]
async fn a_rate_limit_is_retried_and_announced() {
    let server = scripted(vec![
        (429, r#"{"error":{"message":"slow down"}}"#),
        (200, OK_BODY),
    ])
    .await;
    let resolved = crate::resolve(&openai_settings(&server, 2)).expect("parse");
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
    .expect("the second attempt should succeed");

    assert_eq!(response.text(), "done");
    assert_eq!(
        server.hits.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "one 429 then one success"
    );
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1, "every retry owes a notification");
    assert!(seen[0].reason.contains("429"), "{:?}", seen[0]);
    assert_eq!(
        seen[0].delay_ms, 0,
        "the server's Retry-After is obeyed (here it is 0)"
    );
}

/// With retries off (`JOY_LLM_RETRIES=0`), a 429 surfaces immediately and honestly.
#[tokio::test]
async fn retries_can_be_switched_off() {
    let server = scripted(vec![(429, r#"{"error":{"message":"slow down"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 0)).expect("parse");
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
    .expect_err("with retries off it should fail");

    assert!(error.retryable(), "429 is the retryable kind");
    assert_eq!(server.hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(
        seen.lock().expect("lock").is_empty(),
        "no retry, no notification"
    );
}

/// 400 (bad key, bad params) is **not** retried: a retry only fails again the same way.
#[tokio::test]
async fn a_client_error_is_not_retried() {
    let server = scripted(vec![(400, r#"{"error":{"message":"bad key"}}"#)]).await;
    let resolved = crate::resolve(&openai_settings(&server, 3)).expect("parse");
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
    .expect_err("400 has to be reported as it is");

    assert!(!error.retryable(), "400 is not a retryable kind");
    assert_eq!(
        server.hits.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "it must not retry at all"
    );
    assert!(seen.lock().expect("lock").is_empty());
}

/// The retryability decision table itself must be pinned — it is the mechanism's foundation.
#[test]
fn only_transient_failures_are_retryable() {
    use crate::error::ProviderError;
    let http = |status| ProviderError::from_http(status, "{}".to_string());
    assert!(
        http(429).retryable(),
        "a rate limit is the textbook retryable case"
    );
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

// ---- catalog: the built-in table merged with <home>/models-catalog.json (see catalog.rs) ----

#[test]
fn built_in_catalog_matches_the_static_table() {
    let catalog = crate::catalog::Catalog::built_in();
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
    assert!(catalog.lookup("deepseek").is_some());
    assert!(catalog.lookup("ollama").is_some());
}

#[test]
fn load_without_an_override_file_is_built_in() {
    let dir = tempfile::tempdir().expect("temp dir");
    let catalog = crate::catalog::Catalog::load(dir.path());
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
}

#[test]
fn load_patches_an_existing_provider_without_adding_a_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"deepseek","context_window":999999}]"#,
    )
    .expect("write the catalog file");
    let catalog = crate::catalog::Catalog::load(dir.path());
    let deepseek = catalog.lookup("deepseek").expect("still there");
    assert_eq!(deepseek.context_window, 999_999);
    // Fields not mentioned keep their built-in value: a patch is a patch, not a rewrite.
    let built_in = crate::lookup("deepseek").expect("deepseek is in the built-in table");
    assert_eq!(deepseek.model, built_in.model);
    assert_eq!(deepseek.wire, built_in.wire);
    assert_eq!(catalog.entries().len(), crate::PROVIDERS.len());
}

#[test]
fn load_appends_a_keyless_custom_provider_and_resolves_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"mybox","wire":"openai","model":"qwen3:8b","base_url":"http://127.0.0.1:9/v1"}]"#,
    )
    .expect("write the catalog file");
    let catalog = crate::catalog::Catalog::load(dir.path());
    let entry = catalog
        .lookup("mybox")
        .expect("the custom provider was added");
    assert!(
        !entry.needs_key(),
        "no key_env means a local, keyless provider"
    );
    assert_eq!(
        entry.small_model.as_deref(),
        Some("qwen3:8b"),
        "a missing small_model falls back to the main model"
    );
    let settings = joyczl_config::Settings {
        provider: "mybox".to_string(),
        ..Default::default()
    };
    let resolved = crate::resolve_with(&settings, &catalog).expect("it resolves");
    assert_eq!(resolved.context_window, crate::DEFAULT_CONTEXT_WINDOW);
    assert_eq!(resolved.model, "qwen3:8b");
}

#[test]
fn load_skips_broken_rows_and_survives_broken_json() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"half","wire":"openai"},{"id":"deepseek","context_window":1}]"#,
    )
    .expect("write the catalog file");
    let catalog = crate::catalog::Catalog::load(dir.path());
    assert!(
        catalog.lookup("half").is_none(),
        "a new line without a model is skipped"
    );
    assert_eq!(
        catalog
            .lookup("deepseek")
            .expect("the legal line takes effect as usual")
            .context_window,
        1
    );

    let dir2 = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir2.path().join("models-catalog.json"), "{not json")
        .expect("write the broken file");
    let catalog2 = crate::catalog::Catalog::load(dir2.path());
    assert_eq!(
        catalog2.entries().len(),
        crate::PROVIDERS.len(),
        "a bad file falls back to the built-in table instead of breaking startup"
    );
}

#[test]
fn resolve_with_an_unknown_provider_lists_catalog_ids() {
    let catalog = crate::catalog::Catalog::built_in();
    let settings = joyczl_config::Settings {
        provider: "does not exist".to_string(),
        ..Default::default()
    };
    let error = crate::resolve_with(&settings, &catalog).expect_err("it should fail");
    assert!(error.contains("Options"), "{error}");
    assert!(error.contains("deepseek"), "{error}");
}

/// A profile declaring vision itself outranks the vendor default: one vendor ships both vision
/// and text-only models.
#[test]
fn a_profile_vision_declaration_beats_the_vendor_default() {
    let catalog = crate::catalog::Catalog::built_in();
    let base = joyczl_config::Settings {
        provider: "deepseek".to_string(),
        api_key: Some("sk-test".to_string()),
        ..Default::default()
    };

    let plain = crate::resolve_with(&base, &catalog).expect("it resolves");
    assert!(
        !plain.vision,
        "no declaration follows the catalog line (deepseek is text-only by default)"
    );

    let declared = crate::resolve_with(
        &joyczl_config::Settings {
            vision: Some(true),
            ..base.clone()
        },
        &catalog,
    )
    .expect("it resolves");
    assert!(
        declared.vision,
        "this profile declares vision, so images go through"
    );

    // It has to work the other way too: openai's catalog entry is true, and declaring false makes
    // it text-only.
    let off = crate::resolve_with(
        &joyczl_config::Settings {
            provider: "openai".to_string(),
            api_key: Some("sk-test".to_string()),
            vision: Some(false),
            ..Default::default()
        },
        &catalog,
    )
    .expect("it resolves");
    assert!(
        !off.vision,
        "a declared false must not be overridden by the vendor default"
    );
}

// ---- multimodal: the shape of an Image block on both wires ----

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
                    text: "what is in this image?".to_string(),
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
    assert!(
        content.is_array(),
        "a message with an image needs array content: {content}"
    );
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );

    // A message with no image keeps a plain string content (no disturbance to existing clients or
    // tests).
    let plain = CreateRequest {
        cache_prefix: None,
        model: "gpt-5.5".to_string(),
        system: None,
        messages: vec![Message::user_text("plain text")],
        tools: Vec::new(),
        max_tokens: 100,
    };
    let body = crate::openai::to_openai(&plain);
    assert_eq!(body["messages"][0]["content"], "plain text");
}

#[test]
fn anthropic_wire_wraps_images_in_a_source_object() {
    let messages = vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "look at this image".to_string(),
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
    // Text blocks keep the shape serde passes straight through.
    assert_eq!(wire[0]["content"][0]["type"], "text");
}

#[test]
fn image_token_estimate_is_byte_based_and_clamped() {
    use crate::tokens::estimate_messages;
    let tiny = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "aGk=".to_string(), // 2 bytes of raw data
        }],
    }];
    // A small image does not round to zero: the clamp floor is 200.
    assert_eq!(estimate_messages(&tiny), 200 + 4);

    let big = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "A".repeat(8_000_000), // ~6MB of raw data
        }],
    }];
    // A huge image does not run away: the clamp ceiling is 8000.
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
