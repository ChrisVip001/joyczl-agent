//! Config parsing and the allowlist boundary. The allowlist is the one place where a
//! mistake means "the agent mailed a stranger", so every branch gets a case.

use super::*;
use tempfile::tempdir;

fn policy(entries: &[&str]) -> RecipientPolicy {
    RecipientPolicy {
        entries: entries.iter().map(|e| e.to_string()).collect(),
    }
}

#[test]
fn an_exact_address_is_allowed_and_a_lookalike_is_not() {
    let policy = policy(&["alice@corp.com"]);
    assert!(policy.permits("alice@corp.com"));
    assert!(
        policy.permits("  ALICE@Corp.com "),
        "地址比较不区分大小写与空白"
    );
    assert!(!policy.permits("alice@corp.com.evil.io"), "后缀不算命中");
    assert!(!policy.permits("bob@corp.com"));
}

#[test]
fn an_at_domain_entry_allows_the_whole_domain() {
    let policy = policy(&["@corp.com"]);
    assert!(policy.permits("anyone@corp.com"));
    assert!(!policy.permits("anyone@evil.com"));
    assert!(!policy.permits("anyone@notcorp.com"));
}

#[test]
fn an_empty_list_allows_nobody() {
    let policy = policy(&[]);
    assert!(
        !policy.permits("alice@corp.com"),
        "失败关闭：没白名单就是谁都不许"
    );
    assert!(!policy.permits(""));
    assert!(!policy.permits("no-at-sign"));
}

#[test]
fn a_missing_file_means_no_connectors_and_a_broken_one_is_loud() {
    let dir = tempdir().expect("临时目录");
    let quiet = load(dir.path()).expect("没有文件不是错误");
    assert!(quiet.email.is_none());

    std::fs::write(dir.path().join("connectors.json"), "{ not json").expect("写");
    let error = load(dir.path()).expect_err("坏文件必须大声失败");
    assert!(error.to_string().contains("connectors.json"), "{error}");
}

#[test]
fn a_typo_in_a_key_is_rejected_instead_of_silently_meaning_nobody() {
    let dir = tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("connectors.json"),
        r#"{"email": {
            "smtp_host": "h", "smtp_port": 587, "from": "a@b.c",
            "allow_recipents": ["@b.c"]
        }}"#,
    )
    .expect("写");
    let error = load(dir.path()).expect_err("拼错的字段名必须被拒");
    assert!(error.to_string().contains("allow_recipents"), "{error}");
}

#[test]
fn live_is_off_unless_the_file_says_so() {
    let config: ConnectorConfig =
        serde_json::from_str(r#"{"email": {"smtp_host": "h", "smtp_port": 25, "from": "a@b.c"}}"#)
            .expect("解析");
    let email = config.email.expect("有 email 段");
    assert!(!email.live, "默认只能是草稿");
    assert!(email.allow_recipients.is_empty(), "默认谁都不许");
}
