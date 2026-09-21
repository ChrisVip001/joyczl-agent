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
