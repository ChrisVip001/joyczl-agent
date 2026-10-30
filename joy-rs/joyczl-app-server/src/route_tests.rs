//! 路由决策的纯函数测试 —— tier 选择与缺省回落链。
//!
//! 与 Server 的集成（真实解析、回落路径）由 eval 的 mock provider 场景覆盖；
//! 这里钉住的是规则本身。

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

/// 窗口的一半是分界：小消息 chat，超过一半 reasoning。窗口跟着 chat 档
/// 自己的 provider 走 —— 同一条规则在 32k 本地模型和 1M gemini 上都成立。
#[test]
fn tier_is_reasoning_past_half_the_window() {
    assert_eq!(pick_tier(1_000, 32_768), "chat");
    assert_eq!(pick_tier(16_383, 32_768), "chat");
    assert_eq!(pick_tier(16_384, 32_768), "reasoning");
    assert_eq!(pick_tier(499_999, 1_000_000), "chat", "大窗口要消息真的很大才换");
    assert_eq!(pick_tier(600_000, 1_000_000), "reasoning");
}

/// 缺省链：reasoning/fast 缺项都落 chat，chat 缺项落 active，
/// 全缺时返回 None（调用方回落 active 并说明）。
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
    assert_eq!(profile_for_tier(&full, "reasoning", Some("oa")).as_deref(), Some("ds-r1"));
    assert_eq!(profile_for_tier(&full, "fast", Some("oa")).as_deref(), Some("ds"), "fast 缺项 → chat 那套");
    assert_eq!(profile_for_tier(&full, "chat", Some("oa")).as_deref(), Some("ds"));

    // 只配了 reasoning 一条：fast 经 chat 缺项直达 active —— 不会误用 reasoning。
    let only_reasoning = routes_with(&[("reasoning", "ds-r1")]);
    assert_eq!(
        profile_for_tier(&only_reasoning, "fast", Some("oa")).as_deref(),
        Some("oa")
    );
}
