//! 出口那一层的测试：库里的行 → 协议里的值。
//!
//! 只测两个纯函数（`iso` 和 `to_message`）。真跟 state 打交道的那部分由两头
//! 夹住 —— 里面是 `state_tests.rs` 的 SQL，外面是 `smoke-dashboard.sh` 的
//! 真实 HTTP 往返。这一层不该长出第二套 SQL。

use super::{iso, iso_opt, to_message};
use joyczl_protocol::MessageRole;
use joyczl_state::MessageRow;

fn row(role: &str, meta: Option<&str>) -> MessageRow {
    MessageRow {
        id: 7,
        role: role.to_string(),
        content: "你好".to_string(),
        at: "2026-09-19 16:46:49".to_string(),
        meta: meta.map(str::to_string),
    }
}

/// 库里存的是 `datetime('now')`：UTC、空格分隔、**没有时区标记**。
/// 出去必须带上 `Z` —— 不带的话浏览器按本地时间读，东八区就整整差 8 小时。
#[test]
fn sqlite_datetime_goes_out_as_iso_8601_utc() {
    assert_eq!(
        iso("2026-09-19 16:46:49".to_string()),
        "2026-09-19T16:46:49Z"
    );
    assert_eq!(iso_opt(None), None);
    assert_eq!(
        iso_opt(Some("2026-01-02 03:04:05".to_string())),
        Some("2026-01-02T03:04:05Z".to_string())
    );
}

/// 认不出来就原样带出去：这只是一层格式化，没资格让整个方法报错。
#[test]
fn iso_passes_through_what_it_cannot_parse() {
    for weird in ["", "2026-09-19T16:46:49+08:00", "不是时间"] {
        assert_eq!(iso(weird.to_string()), weird);
    }
}

/// 角色是**闭集**：只有这两种画得出来。认不出来的跳过，
/// 而不是硬塞一个值进去骗前端的 `switch`。
#[test]
fn only_renderable_roles_make_it_into_the_protocol() {
    assert_eq!(
        to_message(row("user", None)).expect("user").role,
        MessageRole::User
    );
    assert_eq!(
        to_message(row("assistant", None)).expect("assistant").role,
        MessageRole::Assistant
    );
    assert!(to_message(row("system", None)).is_none());
    assert!(to_message(row("", None)).is_none());
}

/// 遥测那一列是 JSON 文本：形状将来会变，也可能是手写进去的脏数据。
/// 解不出来就当没有遥测 —— 历史的主体是说过的话，不是遥测。
#[test]
fn broken_telemetry_does_not_take_the_message_with_it() {
    let raw = r#"{"gate":null,"graph":null,"iterations":2,"latencyMs":120,
                  "tools":[],"model":"qwen","provider":"dashscope"}"#;
    let meta = to_message(row("assistant", Some(raw)))
        .expect("assistant")
        .meta
        .expect("这份遥测该被解析出来");
    assert_eq!(meta.model, "qwen");
    assert_eq!(meta.iterations, 2);
    assert_eq!(meta.provider, "dashscope");

    let broken = to_message(row("assistant", Some("这不是 JSON"))).expect("assistant");
    assert!(broken.meta.is_none());
    assert_eq!(broken.content, "你好");
}

/// 行号与时刻要原样过桥：行号就是下一页的游标。
#[test]
fn id_and_timestamp_survive_the_trip() {
    let message = to_message(row("user", None)).expect("user");
    assert_eq!(message.id, 7);
    assert_eq!(message.at, "2026-09-19T16:46:49Z");
    assert!(message.meta.is_none());
}
