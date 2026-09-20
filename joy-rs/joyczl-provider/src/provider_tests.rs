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
