//! Tests for the outlet layer: rows in the db → values in the protocol.
//!
//! Only two pure functions are tested (`iso` and `to_message`). The part that really talks to state is
//! pinched from both ends — inside, the SQL in `state_tests.rs`; outside, the real HTTP round-trips of
//! `smoke-dashboard.sh`. This layer must never grow a second set of SQL.

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

/// The db stores `datetime('now')`: UTC, space-separated, **no timezone marker**.
/// Outbound it must carry a `Z` — without it the browser reads local time, a full 8-hour shift in UTC+8.
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

/// What it cannot parse passes through unchanged: this is only a formatting layer, with no standing to error the whole method.
#[test]
fn iso_passes_through_what_it_cannot_parse() {
    for weird in ["", "2026-09-19T16:46:49+08:00", "不是时间"] {
        assert_eq!(iso(weird.to_string()), weird);
    }
}

/// Roles are a **closed set**: only these two are renderable. Unrecognizable ones are skipped,
/// rather than forcing a value in to fool the frontend's `switch`.
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

/// The telemetry column is JSON text: its shape will change, or it may be hand-written dirty data.
/// Unparsable means no telemetry — the body of history is what was said, not the telemetry.
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

/// The row id and timestamp cross the bridge unchanged: the row id is the next page's cursor.
#[test]
fn id_and_timestamp_survive_the_trip() {
    let message = to_message(row("user", None)).expect("user");
    assert_eq!(message.id, 7);
    assert_eq!(message.at, "2026-09-19T16:46:49Z");
    assert!(message.meta.is_none());
}
