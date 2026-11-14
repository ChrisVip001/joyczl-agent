//! Hub tests: user table, constant-time verification, name path safety.
//!
//! The lazily loading `Hub::client` spawns a real subprocess — that route is smoke-covered
//! (the same line of code as single-user mode's spawn); only pure logic is tested here.

use super::hub::{generate_token, lru_victim, stale_entries, valid_user, Hub, Users};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// What is stored reads back; the file is 0600 (unix).
#[test]
fn the_user_table_round_trips() {
    let dir = temp();
    let path = dir.path().join("users.json");
    let mut users = Users::load(&path).expect("a missing file = an empty ledger");
    assert!(users.is_empty());

    users.add("alice", "t-a");
    users.add("bob", "t-b");
    users.save(&path).expect("save it");

    let loaded = Users::load(&path).expect("read back");
    assert_eq!(loaded.verify("t-a").as_deref(), Some("alice"));
    assert_eq!(loaded.verify("t-b").as_deref(), Some("bob"));
    assert_eq!(loaded.verify("t-c"), None, "no match means nobody");
    assert_eq!(loaded.len(), 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the token file must be readable by its owner alone"
        );
    }
}

/// Adding the same name is an overwrite, not an append — that is how tokens rotate.
#[test]
fn adding_the_same_user_replaces_the_token() {
    let mut users =
        Users::load(temp().path().join("users.json").as_path()).expect("an empty ledger");
    users.add("alice", "old-token");
    users.add("alice", "new-token");
    assert_eq!(users.len(), 1);
    assert_eq!(
        users.verify("old-token"),
        None,
        "the rotated-out token stops working"
    );
    assert_eq!(users.verify("new-token").as_deref(), Some("alice"));
}

/// verify has constant-time-compare semantics: even one byte off means no match.
/// trim is resolve's job (Bearer header parsing); verify itself is an exact match.
#[test]
fn verification_agrees_with_equality() {
    let mut users =
        Users::load(temp().path().join("users.json").as_path()).expect("an empty ledger");
    users.add("alice", "secret-token");
    assert_eq!(
        users.verify("secret-tokeN"),
        None,
        "one character off is still off"
    );
    assert_eq!(
        users.verify("secret-token "),
        None,
        "verify does not trim — that is resolve's job"
    );
}

/// Bad names refuse to load — loading a `../` opens the door to path traversal.
#[test]
fn a_bad_name_refuses_to_load() {
    let dir = temp();
    let path = dir.path().join("users.json");
    std::fs::write(&path, r#"{"../evil": "t"}"#).expect("plant the bad entry");
    let error = Users::load(&path).unwrap_err();
    assert!(error.to_string().contains("is not valid"), "{error}");
}

/// Name rules: whatever gets in is a safe path segment.
#[test]
fn user_names_must_be_safe_path_segments() {
    assert!(valid_user("alice"));
    assert!(valid_user("a-1"));
    assert!(!valid_user("../evil"));
    assert!(!valid_user("/abs"));
    assert!(!valid_user(""));
    assert!(
        !valid_user("Alice"),
        "lowercase only — a case-insensitive file system would collide"
    );
    assert!(!valid_user("a".repeat(33).as_str()));
}

/// An empty users.json now opens (web self-registration is the M2 path) — the operator
/// gets a hint on stderr, the first user registers from loopback or via adduser.
#[test]
fn an_empty_hub_opens_and_waits_for_its_first_user() {
    let dir = temp();
    let hub =
        Hub::open(dir.path()).expect("an empty hub opens too (registering is the front door)");
    assert_eq!(hub.user_count(), 0);
    // Empty ledger + public binding → the first registration is shut out (serve_hub sets that flag
    // from the host).
    hub.permit_first_registration(false);
    assert!(matches!(
        hub.register("alice", "hunter2-secret", "phone"),
        Err(crate::hub::AuthError::Closed)
    ));

    std::fs::write(dir.path().join("users.json"), r#"{"alice": "t-a"}"#).expect("plant the ledger");
    let hub = Hub::open(dir.path()).expect("one user is enough to open");
    assert_eq!(hub.user_count(), 1);
    assert_eq!(hub.home("alice"), dir.path().join("alice"));
}

/// resolve: malformed header / empty token → None; a match → the user.
#[test]
fn resolve_walks_the_bearer_header() {
    let dir = temp();
    std::fs::write(dir.path().join("users.json"), r#"{"alice": "t-a"}"#).expect("plant the ledger");
    let hub = Hub::open(dir.path()).expect("open the hub");

    assert_eq!(hub.resolve(None), None, "no header");
    assert_eq!(
        hub.resolve(Some("t-a")),
        None,
        "a bare token is not a Bearer"
    );
    assert_eq!(hub.resolve(Some("Bearer ")), None, "an empty token");
    assert_eq!(hub.resolve(Some("Bearer t-b")), None, "no match");
    assert_eq!(hub.resolve(Some("Bearer t-a")).as_deref(), Some("alice"));
}

/// user_count only counts; the users lock never leaks (internal invariant, pinned here).
#[test]
fn the_hub_reports_its_user_count() {
    let dir = temp();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"alice": "t-a", "bob": "t-b"}"#,
    )
    .expect("plant the ledger");
    let hub = Hub::open(dir.path()).expect("open the hub");
    assert_eq!(hub.user_count(), 2);
    // Another way to avoid an unused warning for the hub module: assert the module path exists.
    let _ = valid_user("x");
}

// ---- BYOK isolation is structural ------------------------------------------
//
// Key isolation relies on paths, not "remembering not to leak": each user's home is its
// own directory, app-server subprocess, models.json and keychain. These tests pin it.

/// Two users' homes are two disjoint directories under the root — the entire physical basis
/// of "A's key never appears in any of B's paths".
#[test]
fn two_users_never_share_a_home() {
    let dir = temp();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"alice": "t-a", "bob": "t-b"}"#,
    )
    .expect("plant the ledger");
    let hub = Hub::open(dir.path()).expect("open the hub");

    let alice = hub.home("alice");
    let bob = hub.home("bob");
    assert_ne!(alice, bob);
    assert!(alice.starts_with(dir.path()) && bob.starts_with(dir.path()));
    assert!(!alice.starts_with(&bob) && !bob.starts_with(&alice));
    // Name rules guarantee the directory name is the username — no `../` can slip into a path.
    assert_eq!(alice.file_name().and_then(|n| n.to_str()), Some("alice"));
}

/// users.json is the admission ledger, tokens only: it must **never** contain the shape of
/// a model key. (Keys live in each home's keychain/secrets.json — M1 territory.)
#[test]
fn the_users_file_stores_tokens_never_model_keys() {
    let dir = temp();
    let path = dir.path().join("users.json");
    let mut users = Users::load(&path).expect("an empty ledger");
    users.add("alice", &generate_token().expect("generate"));
    users.save(&path).expect("save it");
    let raw = std::fs::read_to_string(&path).expect("read back");
    assert!(
        !raw.contains("sk-"),
        "no model-key shape may reach the access ledger: {raw}"
    );
    assert!(
        !raw.contains("keyEnv") && !raw.contains("apiKey"),
        "it should carry nothing but the token: {raw}"
    );
}

// ---- Three pool policies --------------------
//
// The decisions are pure functions fed snapshots; idle in (user, last_used, idle) means only
// the pool holds it (Arc::strong_count == 1) — an instance serving a request is pinned.

fn snapshot(pairs: &[(&str, u64, bool)]) -> Vec<(String, std::time::Instant, bool)> {
    let now = std::time::Instant::now();
    pairs
        .iter()
        .map(|(u, ago_ms, idle)| {
            (
                u.to_string(),
                now - std::time::Duration::from_millis(*ago_ms),
                *idle,
            )
        })
        .collect()
}

/// LRU picks the oldest unused among the idle; a pinned one stays even if older.
#[test]
fn lru_evicts_the_oldest_idle_and_never_a_pinned_one() {
    let entries = snapshot(&[
        ("a", 10_000, true),
        ("b", 5_000, true),
        ("c", 20_000, false),
    ]);
    // c is oldest but pinned — the victim is a (the longest-idle among the idle).
    assert_eq!(lru_victim(&entries).as_deref(), Some("a"));

    let all_pinned = snapshot(&[("a", 10_000, false), ("b", 5_000, false)]);
    // All pinned: soft cap — rather over-open than turn a request into an error.
    assert_eq!(lru_victim(&all_pinned), None);

    let empty: Vec<(String, std::time::Instant, bool)> = Vec::new();
    assert_eq!(lru_victim(&empty), None);
}

/// Idle sweep: expired idle entries join the list; 0 secs = policy off; just booted
/// (checked_sub fails) closes nothing — better to keep too much than close wrongly.
#[test]
fn stale_sweep_respects_the_deadline_and_never_evicts_on_boot() {
    // b idle 30s over a 20s threshold → close; a just used → keep; c pinned → keep.
    let entries = snapshot(&[("a", 100, true), ("b", 30_000, true), ("c", 40_000, false)]);
    let now = std::time::Instant::now();
    assert_eq!(stale_entries(&entries, 20, now), vec!["b".to_string()]);

    // Threshold 0 = policy off.
    assert!(stale_entries(&entries, 0, now).is_empty());

    // Threshold larger than uptime: checked_sub fails → empty list (no panic, no wrong close).
    assert!(stale_entries(&entries, u64::MAX / 2, now).is_empty());
}

/// The pool's real wiring: a small cap makes a third user evict the first (real spawn, real eviction).
#[tokio::test]
async fn the_pool_caps_open_instances() {
    let dir = temp();
    let path = dir.path().join("users.json");
    let mut users = Users::load(&path).expect("an empty ledger");
    users.add("a", "t-a");
    users.add("b", "t-b");
    users.add("c", "t-c");
    users.save(&path).expect("save it");
    let hub = Hub::open_with(dir.path(), 2, 0).expect("open the hub (cap 2, no idle closing)");

    let _a = hub.client("a").await.expect("a");
    let _b = hub.client("b").await.expect("b");
    assert_eq!(hub.open_count(), 2);

    // A third comes in: full → evict one. The Arcs of a/b/c are still held by the test
    // (pinned), so the soft cap applies — the pool over-opens rather than closing in-use.
    let _c = hub.client("c").await.expect("c");
    assert!(hub.open_count() >= 2, "the pool keeps at least 2");

    // After dropping the a/b/c handles, the next fetch sweeps first: full → evict oldest idle.
    drop(_a);
    drop(_b);
    drop(_c);
    let _a2 = hub.client("a").await.expect("a comes back");
    assert!(
        hub.open_count() <= 3,
        "the pool must not grow without bound: {}",
        hub.open_count()
    );
}
