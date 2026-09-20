//! v2 协议的 wire format 断言。
//!
//! 这些是**普通单测**（不带 `#[ignore]`），`cargo test` 就会跑。
//! 生成物对不对由 export_tests 管，这里管的是「序列化出来的字节长什么样」——
//! 也就是前端和 Python SDK 实际会收到的东西。

use super::v2::*;

#[test]
fn notification_is_tagged_with_type() {
    let n = ServerNotification::GateDecided(GateDecidedNotification {
        turn_id: "t-1".to_string(),
        decision: GateDecision {
            decision: GateDecisionKind::Retrieve,
            reason: "mentions alex".to_string(),
            query: Some("alex meetings".to_string()),
        },
    });
    let v: serde_json::Value = serde_json::to_value(&n).expect("序列化");
    assert_eq!(v["type"], "gateDecided");
    assert_eq!(v["decision"]["decision"], "retrieve");
    assert_eq!(v["decision"]["query"], "alex meetings");
}

#[test]
fn every_notification_variant_has_a_stable_tag() {
    // 判别式一旦变了，所有前端的 switch 都会静默掉进 default 分支。
    let cases: Vec<(ServerNotification, &str)> = vec![
        (
            ServerNotification::TurnStarted(TurnStartedNotification {
                turn_id: "t".into(),
                session_id: "default".into(),
                user_message: "hi".into(),
                ts: "2026-09-19T00:00:00.000Z".into(),
            }),
            "turnStarted",
        ),
        (
            ServerNotification::TextDelta(TextDeltaNotification {
                turn_id: "t".into(),
                delta: "he".into(),
            }),
            "textDelta",
        ),
        (
            ServerNotification::ConsolidationCompleted(ConsolidationCompletedNotification {
                new_facts: 2,
            }),
            "consolidationCompleted",
        ),
        (
            ServerNotification::GraphEnded(GraphEndedNotification {
                workflow: "triage".into(),
                ms: 12,
                steps: 3,
                path: vec!["classify".into()],
                error: None,
            }),
            "graphEnded",
        ),
        (
            ServerNotification::Error(ErrorNotification {
                code: -32000,
                message: "boom".into(),
                data: None,
            }),
            "error",
        ),
    ];

    for (n, expected) in cases {
        let v: serde_json::Value = serde_json::to_value(&n).expect("序列化");
        assert_eq!(v["type"], expected, "{expected} 的判别式变了");
    }
}

#[test]
fn params_are_camel_case_on_the_wire() {
    let p = TurnStartParams {
        session_id: None,
        message: "hi".into(),
        stream: Some(true),
    };
    let v: serde_json::Value = serde_json::to_value(&p).expect("序列化");
    // Option 在 *Params 上也会出现（值为 null），这样前端能区分「没传」和「空」。
    assert!(v.as_object().expect("object").contains_key("sessionId"));
    assert!(v["sessionId"].is_null());
    assert_eq!(v["message"], "hi");
    assert_eq!(v["stream"], true);
    assert!(!v.as_object().unwrap().contains_key("session_id"));
}

#[test]
fn tool_status_round_trips() {
    for (status, wire) in [(ToolStatus::Ok, "ok"), (ToolStatus::Error, "error")] {
        let v: serde_json::Value = serde_json::to_value(status).expect("序列化");
        assert_eq!(v, wire);
        let back: ToolStatus = serde_json::from_value(v).expect("反序列化");
        assert_eq!(back, status);
    }
}

#[test]
fn graph_route_and_gate_decision_share_the_camel_case_rule() {
    let g = GraphInfo {
        workflow: "triage".into(),
        route: GraphRouteKind::Quick,
        reason: "trivial".into(),
        path: vec![],
    };
    let v: serde_json::Value = serde_json::to_value(&g).expect("序列化");
    assert_eq!(v["route"], "quick");
}

#[test]
fn message_role_and_page_cursor_are_camel_case_on_the_wire() {
    for (role, wire) in [
        (MessageRole::User, "user"),
        (MessageRole::Assistant, "assistant"),
    ] {
        let v: serde_json::Value = serde_json::to_value(role).expect("序列化");
        assert_eq!(v, wire);
    }

    // 历史那一路的载荷：`sessionId` + 游标。
    let p = SessionMessagesParams {
        session_id: "default".into(),
        cursor: Some("42".into()),
        limit: None,
    };
    let v: serde_json::Value = serde_json::to_value(&p).expect("序列化");
    assert_eq!(v["sessionId"], "default");
    assert_eq!(v["cursor"], "42");
    assert!(v.as_object().unwrap().contains_key("limit"));

    let r = SessionMessagesResponse {
        data: vec![],
        next_cursor: None,
    };
    let v: serde_json::Value = serde_json::to_value(&r).expect("序列化");
    assert_eq!(v["nextCursor"], serde_json::Value::Null);
}

#[test]
fn settings_patch_default_leaves_everything_unset() {
    // 全 null 的 patch 写进去必须什么都不改。
    let v: serde_json::Value = serde_json::to_value(SettingsPatch::default()).expect("序列化");
    let obj = v.as_object().expect("object");
    assert!(
        !obj.is_empty(),
        "patch 字段全都得出现在 wire 上（值为 null）"
    );
    assert!(obj.values().all(|x| x.is_null()));
}
