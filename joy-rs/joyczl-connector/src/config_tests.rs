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
        "address comparison ignores case and whitespace"
    );
    assert!(
        !policy.permits("alice@corp.com.evil.io"),
        "a suffix is not a hit"
    );
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
        "fail closed: no allowlist means nobody is allowed"
    );
    assert!(!policy.permits(""));
    assert!(!policy.permits("no-at-sign"));
}

#[test]
fn a_missing_file_means_no_connectors_and_a_broken_one_is_loud() {
    let dir = tempdir().expect("temp dir");
    let quiet = load(dir.path()).expect("a missing file is not an error");
    assert!(quiet.email.is_none());

    std::fs::write(dir.path().join("connectors.json"), "{ not json").expect("write");
    let error = load(dir.path()).expect_err("a bad file has to fail loudly");
    assert!(error.to_string().contains("connectors.json"), "{error}");
}

#[test]
fn a_typo_in_a_key_is_rejected_instead_of_silently_meaning_nobody() {
    let dir = tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("connectors.json"),
        r#"{"email": {
            "smtp_host": "h", "smtp_port": 587, "from": "a@b.c",
            "allow_recipents": ["@b.c"]
        }}"#,
    )
    .expect("write");
    let error = load(dir.path()).expect_err("a misspelt field name must be refused");
    assert!(error.to_string().contains("allow_recipents"), "{error}");
}

#[test]
fn live_is_off_unless_the_file_says_so() {
    let config: ConnectorConfig =
        serde_json::from_str(r#"{"email": {"smtp_host": "h", "smtp_port": 25, "from": "a@b.c"}}"#)
            .expect("parse");
    let email = config.email.expect("there is an email section");
    assert!(!email.live, "the default can only be a draft");
    assert!(
        email.allow_recipients.is_empty(),
        "by default nobody is allowed"
    );
}
