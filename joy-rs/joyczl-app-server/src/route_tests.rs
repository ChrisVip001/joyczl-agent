//! Pure-function tests for route decisions — tier selection and the fallback chain.
//!
//! Integration with Server (real resolution, fallback paths) is covered by eval's mock-provider scenarios;
//! what is pinned here is the rules themselves.

use joyczl_config::routes::{RouteFile, RouteMode};
use std::collections::HashMap;

use super::{pick_tier, profile_for_tier};

fn routes_with(entries: &[(&str, &str)]) -> RouteFile {
    let mut routes = RouteFile {
        mode: RouteMode::Auto,
        routes: HashMap::new(),
    };
    for (tier, id) in entries {
        routes.routes.insert(tier.to_string(), id.to_string());
    }
    routes
}

/// Half the window is the boundary: small messages chat, past half reasoning. The window follows the
/// chat tier's own provider — the same rule holds on a 32k local model and a 1M gemini alike.
#[test]
fn tier_is_reasoning_past_half_the_window() {
    assert_eq!(pick_tier(1_000, 32_768), "chat");
    assert_eq!(pick_tier(16_383, 32_768), "chat");
    assert_eq!(pick_tier(16_384, 32_768), "reasoning");
    assert_eq!(
        pick_tier(499_999, 1_000_000),
        "chat",
        "大窗口要消息真的很大才换"
    );
    assert_eq!(pick_tier(600_000, 1_000_000), "reasoning");
}

/// The fallback chain: missing reasoning/fast entries fall to chat, a missing chat entry falls to active,
/// and when everything is missing it returns None (the caller falls back to active and explains).
#[test]
fn tier_lookup_falls_back_through_the_chain() {
    let empty = routes_with(&[]);
    assert_eq!(
        profile_for_tier(&empty, "reasoning", Some("ds")).as_deref(),
        Some("ds"),
        "reasoning 缺项 → chat → active"
    );
    assert_eq!(
        profile_for_tier(&empty, "fast", Some("ds")).as_deref(),
        Some("ds")
    );
    assert_eq!(profile_for_tier(&empty, "chat", None), None, "全缺 = None");

    let full = routes_with(&[("reasoning", "ds-r1"), ("chat", "ds")]);
    assert_eq!(
        profile_for_tier(&full, "reasoning", Some("oa")).as_deref(),
        Some("ds-r1")
    );
    assert_eq!(
        profile_for_tier(&full, "fast", Some("oa")).as_deref(),
        Some("ds"),
        "fast 缺项 → chat 那套"
    );
    assert_eq!(
        profile_for_tier(&full, "chat", Some("oa")).as_deref(),
        Some("ds")
    );

    // Only reasoning is configured: fast reaches active through the missing chat entry — reasoning is never misused.
    let only_reasoning = routes_with(&[("reasoning", "ds-r1")]);
    assert_eq!(
        profile_for_tier(&only_reasoning, "fast", Some("oa")).as_deref(),
        Some("oa")
    );
}

/// The ambiguous band: below 2k is obviously chat (no classify call spent), past half the window L0 has
/// already ruled — only the middle stretch is worth one cheap-model question.
#[test]
fn classification_only_runs_in_the_ambiguous_band() {
    use super::needs_classify;

    assert!(!needs_classify(1_999, 32_768), "短消息不分类");
    assert!(needs_classify(2_000, 32_768));
    assert!(needs_classify(16_383, 32_768));
    assert!(
        !needs_classify(16_384, 32_768),
        "过半已被 L0 判成 reasoning"
    );
}

/// Parsing the classifier's reply: fail-open — only a first word of reasoning counts; vagueness, blank,
/// or extra explanation is all chat. Bias conservative: misjudging chat only saves less money; misjudging
/// reasoning wastes money on the expensive model.
#[test]
fn tier_reply_parsing_fails_open() {
    use super::parse_tier_reply;

    assert_eq!(parse_tier_reply("reasoning"), Some("reasoning"));
    assert_eq!(
        parse_tier_reply("Reasoning\n"),
        Some("reasoning"),
        "大小写与换行不敏感"
    );
    assert_eq!(parse_tier_reply("chat"), None);
    assert_eq!(parse_tier_reply(""), None);
    assert_eq!(
        parse_tier_reply("这看起来像 reasoning 任务，因为……"),
        None,
        "多余解释不算数 —— 只要一个词"
    );
}
