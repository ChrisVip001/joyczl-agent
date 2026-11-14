//! Wire-format assertions for the v2 protocol.
//!
//! These are **ordinary unit tests** (no `#[ignore]`); `cargo test` runs them.
//! export_tests owns artifact correctness; this file owns "what the serialized bytes look like" —
//! i.e. what the frontend and the Python SDK actually receive.

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
    let v: serde_json::Value = serde_json::to_value(&n).expect("serialise");
    assert_eq!(v["type"], "gateDecided");
    assert_eq!(v["decision"]["decision"], "retrieve");
    assert_eq!(v["decision"]["query"], "alex meetings");
}

#[test]
fn every_notification_variant_has_a_stable_tag() {
    // If the discriminant ever changes, every frontend switch silently falls into its default branch.
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
        let v: serde_json::Value = serde_json::to_value(&n).expect("serialise");
        assert_eq!(v["type"], expected, "{expected}'s discriminant changed");
    }
}

#[test]
fn params_are_camel_case_on_the_wire() {
    let p = TurnStartParams {
        session_id: None,
        message: "hi".into(),
        stream: Some(true),
        cwd: None,
        attachments: None,
        tier: None,
    };
    let v: serde_json::Value = serde_json::to_value(&p).expect("serialise");
    // Option fields also appear on *Params (as null), so the frontend can tell "not sent" from "empty".
    assert!(v.as_object().expect("object").contains_key("sessionId"));
    assert!(v["sessionId"].is_null());
    assert_eq!(v["message"], "hi");
    assert_eq!(v["stream"], true);
    assert!(!v.as_object().unwrap().contains_key("session_id"));
}

#[test]
fn tool_status_round_trips() {
    for (status, wire) in [(ToolStatus::Ok, "ok"), (ToolStatus::Error, "error")] {
        let v: serde_json::Value = serde_json::to_value(status).expect("serialise");
        assert_eq!(v, wire);
        let back: ToolStatus = serde_json::from_value(v).expect("deserialise");
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
    let v: serde_json::Value = serde_json::to_value(&g).expect("serialise");
    assert_eq!(v["route"], "quick");
}

#[test]
fn message_role_and_page_cursor_are_camel_case_on_the_wire() {
    for (role, wire) in [
        (MessageRole::User, "user"),
        (MessageRole::Assistant, "assistant"),
    ] {
        let v: serde_json::Value = serde_json::to_value(role).expect("serialise");
        assert_eq!(v, wire);
    }

    // The history-path payload: `sessionId` + cursor.
    let p = SessionMessagesParams {
        session_id: "default".into(),
        cursor: Some("42".into()),
        limit: None,
    };
    let v: serde_json::Value = serde_json::to_value(&p).expect("serialise");
    assert_eq!(v["sessionId"], "default");
    assert_eq!(v["cursor"], "42");
    assert!(v.as_object().unwrap().contains_key("limit"));

    let r = SessionMessagesResponse {
        data: vec![],
        next_cursor: None,
    };
    let v: serde_json::Value = serde_json::to_value(&r).expect("serialise");
    assert_eq!(v["nextCursor"], serde_json::Value::Null);
}

#[test]
fn settings_patch_default_leaves_everything_unset() {
    // An all-null patch written in must change nothing.
    let v: serde_json::Value = serde_json::to_value(SettingsPatch::default()).expect("serialise");
    let obj = v.as_object().expect("object");
    assert!(
        !obj.is_empty(),
        "every patch field has to appear on the wire (as null)"
    );
    assert!(obj.values().all(|x| x.is_null()));
}
