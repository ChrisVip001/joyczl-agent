//! 只测一件事：**哪些通知属于这一轮**。
//!
//! 广播是给所有订阅者的，app-server 又是一个进程跑所有人的会话 ——
//! 网关那边另一个会话跑起来的时候，它的 `textDelta` 也会流到驾驶舱这条
//! 连接上。过滤错了的表现是「回复里混进别人的字」，所以这里把它钉住。

use serde_json::json;

use super::belongs;
use joyczl_protocol::ServerNotification;

fn note(value: serde_json::Value) -> ServerNotification {
    serde_json::from_value(value).expect("测试用的通知")
}

fn started(session: &str, turn: &str) -> ServerNotification {
    note(json!({
        "type": "turnStarted",
        "turnId": turn,
        "sessionId": session,
        "userMessage": "你好",
        "ts": "2026-01-01T00:00:00+08:00",
    }))
}

fn delta(turn: &str) -> ServerNotification {
    note(json!({"type": "textDelta", "turnId": turn, "delta": "字"}))
}

#[test]
fn 第一帧靠会话认_之后的帧靠轮次认() {
    let opened = started("s1", "t1");
    assert_eq!(belongs(&opened, "s1", None).as_deref(), Some("t1"));

    // 认下这一轮之后，同 id 的都放行。
    assert_eq!(
        belongs(&delta("t1"), "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // 另一个轮次的丢掉 —— 即使会话 id 一样（并发跑两轮时会出现）。
    assert_eq!(belongs(&delta("t2"), "s1", Some("t1")), None);
}

#[test]
fn 别的会话的通知不会混进来() {
    assert_eq!(belongs(&started("s2", "t9"), "s1", None), None);
    // 还没认下自己那一轮之前，什么都不转发。
    assert_eq!(belongs(&delta("t9"), "s1", None), None);
}

#[test]
fn 没有轮次id的通知_认下之后照常转发() {
    // consolidationCompleted 和 graph* 不带 turnId：它们跟会话同属一轮。
    let consolidation = note(json!({"type": "consolidationCompleted", "newFacts": 2}));
    assert_eq!(
        belongs(&consolidation, "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // 但没认下之前，它们同样无处可去。
    assert_eq!(belongs(&consolidation, "s1", None), None);
}
