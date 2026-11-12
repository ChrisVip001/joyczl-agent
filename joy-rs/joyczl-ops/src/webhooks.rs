//! Inbound webhooks: a foreign system knocks, one turn runs (`<home>/webhooks.json`).
//!
//! This is the business form of "someone else's event, our agent": CI failing, a form submitted,
//! a payment landed — the machine carries a payload and a secret, and gets one answer back.
//!
//! Three fences, the same shape as connectors:
//! 1. **No file / no matching hook → 404.** An unconfigured name is not a weak door, it is no door;
//!    the model never learns the hook exists either (this path has no tools).
//! 2. **The secret fails closed.** A hook without a readable secret refuses every request; a wrong
//!    or missing `X-Joy-Secret` is 401. Comparison is constant-time (see `constant_time_eq`).
//! 3. **The turn is still an ordinary turn.** Tools keep their approval cards; nobody is at the
//!    keyboard to approve, so destructive tools time out and are denied. A webhook adds no capability.
//!
//! The route lives *outside* the dashboard token layer on purpose: the caller has never heard of
//! `JOY_OPS_TOKEN`; its proof is the per-hook secret. A hook can only **hand over one message** —
//! it cannot read state, list sessions or start more turns.
//!
//! Phase 2 adds the three things a door needs once strangers knock on it:
//! 4. **Replay**: with `signature: true` the caller proves the request is fresh
//!    (`X-Joy-Timestamp` + HMAC over `<timestamp>.<body>`), and each signature is accepted once.
//!    Plain `X-Joy-Secret` still works, and cannot refuse a replay — that difference is the point
//!    of asking for the signature.
//! 5. **Rate**: `limit: N` caps accepted requests per rolling minute per hook, after auth (so
//!    garbage does not burn the budget), 429 + `Retry-After`.
//! 6. **Async**: `callback: "https://…"` answers `202` at once and POSTs the result there instead
//!    of holding the caller's connection open for a turn.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub const CONFIG_FILE: &str = "webhooks.json";

/// How far a request's timestamp may sit from our clock (both directions).
pub const SIGNATURE_WINDOW: i64 = 300;

/// How many accepted signatures we remember before dropping the ones closest to expiry.
const MAX_SEEN: usize = 8192;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhooksConfig {
    #[serde(default)]
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// The last path segment: `POST /api/hook/<name>`.
    pub name: String,
    /// `env:VAR` (read at request time) or a literal. Empty is refused at load — never "open door".
    pub secret: String,
    /// The prompt template. `{{payload}}` is replaced by the raw request body; without the
    /// placeholder the body is appended on its own line (a body silently dropped would be worse).
    pub prompt: String,
    /// Which session the turn lands in; absent = `hook:<name>` (one hook, one thread of history).
    #[serde(default)]
    pub session: Option<String>,
    /// Model tier pin (`fast` / `chat` / `reasoning`). A typo fails the turn loudly instead of
    /// quietly costing the wrong model.
    #[serde(default)]
    pub tier: Option<String>,
    /// Default true. `false` = the door answers 404 like an unknown name (a hook switched off).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `true` = require `X-Joy-Timestamp` + `X-Joy-Signature` (HMAC-SHA256 over
    /// `<timestamp>.<body>`, hex) and refuse a signature already used inside the window.
    /// Off = the plain `X-Joy-Secret` mode, which proves the secret but not freshness.
    #[serde(default)]
    pub signature: Option<bool>,
    /// Accepted requests per rolling minute for this hook. Absent = no cap. `0` is refused at
    /// load: "configured but can never pass" is a typo, not a policy.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Run the turn in the background and POST the answer here (`http`/`https`), answering
    /// `202` to the caller at once. Absent = synchronous (the caller waits for the answer).
    #[serde(default)]
    pub callback: Option<String>,
}

impl Hook {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn wants_signature(&self) -> bool {
        self.signature.unwrap_or(false)
    }

    pub fn callback_url(&self) -> Option<&str> {
        self.callback
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    }

    /// The secret's actual value. `None` = configured but unreadable (missing env var) — refuse.
    pub fn secret_value(&self) -> Option<String> {
        let raw = self.secret.trim();
        if let Some(var) = raw.strip_prefix("env:") {
            return std::env::var(var.trim())
                .ok()
                .filter(|value| !value.is_empty());
        }
        (!raw.is_empty()).then(|| raw.to_string())
    }

    pub fn session_id(&self) -> String {
        self.session
            .as_deref()
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("hook:{}", self.name))
    }

    /// Put the payload into the prompt. The body stays **as text** — it is data the caller sent,
    /// never instructions Joy composes itself; the model reads it inside the prompt, nothing else.
    pub fn render(&self, payload: &str) -> String {
        const SLOT: &str = "{{payload}}";
        if self.prompt.contains(SLOT) {
            return self.prompt.replace(SLOT, payload.trim());
        }
        format!("{}\n\n{}", self.prompt.trim_end(), payload.trim())
    }
}

/// Read the config. Missing file = no hooks (not an error); a broken file is an error on purpose —
/// a typo in a hook must be visible, not silently "no such hook" forever.
pub fn load(home: &Path) -> Result<WebhooksConfig> {
    let path = home.join(CONFIG_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(WebhooksConfig::default());
    };
    let config: WebhooksConfig = serde_json::from_str(&raw).map_err(|e| {
        anyhow!(
            "{} could not be read: {e} (the fields are name/secret/prompt/session/tier/enabled/signature/limit/callback)",
            path.display()
        )
    })?;
    for hook in &config.hooks {
        if hook.name.trim().is_empty() {
            return Err(anyhow!("a hook in {} has no name", path.display()));
        }
        if hook.secret.trim().is_empty() {
            return Err(anyhow!(
                "hook '{}' in {} has no secret — refusing to enable it (an inbound webhook must be able to prove who it is)",
                path.display(),
                hook.name
            ));
        }
        if hook.prompt.trim().is_empty() {
            return Err(anyhow!(
                "hook '{}' in {} has no prompt — it would have nothing to say when a request arrives",
                path.display(),
                hook.name
            ));
        }
        if hook.limit == Some(0) {
            return Err(anyhow!(
                "hook '{}' in {} sets limit: 0 — \"configured but never allowed\" is not a policy, it is a typo",
                path.display(),
                hook.name
            ));
        }
        if let Some(url) = hook.callback_url() {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(anyhow!(
                    "the callback of hook '{}' in {} must be an http(s) URL, got: {url}",
                    path.display(),
                    hook.name
                ));
            }
        }
    }
    Ok(config)
}

/// One refusal, straight from `admit`. The HTTP layer turns each into a status code.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Missing/wrong secret, missing/wrong signature, or a timestamp outside the window.
    Unauthorized(&'static str),
    /// A correct signature that was already used inside the window.
    Replay,
    /// Over the hook's per-minute cap; the number is "seconds until it can pass again".
    TooFast(u64),
}

/// What the caller presented. Lifted off the headers by the HTTP layer; empty = absent.
pub struct Credential<'a> {
    pub secret: &'a str,
    pub timestamp: &'a str,
    pub signature: &'a str,
}

/// `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`.
///
/// Hex and `timestamp.body` on purpose: the other side verifies it with the tools it already has
/// (`openssl dgst -sha256 -hmac "$SECRET"` over `printf '%s.%s' "$TS" "$BODY"`), not with anything
/// invented here.
pub fn signature_of(secret: &str, timestamp: &str, body: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let mut ctx = ring::hmac::Context::with_key(&key);
    ctx.update(timestamp.as_bytes());
    ctx.update(b".");
    ctx.update(body.as_bytes());
    ctx.sign()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Default)]
struct Guard {
    /// Accepted signature → when (for refusing a replay inside the window).
    seen: HashMap<String, Instant>,
    /// Hook name → (window start, accepted count). Per-process, reset on restart.
    windows: HashMap<String, (Instant, u32)>,
}

fn guard() -> &'static Mutex<Guard> {
    static STATE: OnceLock<Mutex<Guard>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Guard::default()))
}

fn lock_guard() -> std::sync::MutexGuard<'static, Guard> {
    // A panic while holding it cannot corrupt a counter into something unsafe; keep serving.
    guard()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Guard {
    fn prune(&mut self, now: Instant) {
        let window = Duration::from_secs(SIGNATURE_WINDOW as u64);
        self.seen.retain(|_, at| now.duration_since(*at) <= window);
        if self.seen.len() > MAX_SEEN {
            // Still full: drop the older half of the window (almost expired anyway) rather than
            // let a knocker grow this map without bound.
            let half = window / 2;
            self.seen.retain(|_, at| now.duration_since(*at) <= half);
        }
    }

    fn rate(&mut self, name: &str, limit: u32, now: Instant) -> std::result::Result<(), u64> {
        let entry = self.windows.entry(name.to_string()).or_insert((now, 0));
        let elapsed = now.duration_since(entry.0);
        if elapsed >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            let wait = 60u64.saturating_sub(elapsed.as_secs()).max(1);
            return Err(wait);
        }
        entry.1 += 1;
        Ok(())
    }
}

/// The door's whole policy, minus HTTP: who you are, is it fresh, may you pass right now.
///
/// One lock, two clocks: `now` (monotonic, for the rate window and replay age) and `now_epoch`
/// (wall clock, for the signature's freshness). Both are arguments so the rules are testable
/// without sleeping.
pub fn admit(
    hook: &Hook,
    credential: &Credential<'_>,
    body: &str,
    now_epoch: i64,
    now: Instant,
) -> std::result::Result<(), Refusal> {
    let Some(secret) = hook.secret_value() else {
        // Configured but unreadable (missing env var): refuse, never "open door".
        return Err(Refusal::Unauthorized(
            "this hook's secret could not be read (is the env var set?)",
        ));
    };

    if hook.wants_signature() {
        let timestamp = credential.timestamp.trim();
        let signature = credential.signature.trim().to_ascii_lowercase();
        if timestamp.is_empty() || signature.is_empty() {
            return Err(Refusal::Unauthorized(
                "signature mode needs X-Joy-Timestamp and X-Joy-Signature",
            ));
        }
        let Ok(stamp) = timestamp.parse::<i64>() else {
            return Err(Refusal::Unauthorized(
                "X-Joy-Timestamp must be an integer number of seconds",
            ));
        };
        if (now_epoch - stamp).abs() > SIGNATURE_WINDOW {
            return Err(Refusal::Unauthorized(
                "the timestamp is outside the window (±300 seconds): old requests may not be replayed",
            ));
        }
        let expected = signature_of(&secret, timestamp, body);
        if !crate::constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return Err(Refusal::Unauthorized("the signature is wrong"));
        }
        let mut guard = lock_guard();
        guard.prune(now);
        if guard.seen.contains_key(&signature) {
            return Err(Refusal::Replay);
        }
        guard.seen.insert(signature, now);
    } else if !crate::constant_time_eq(credential.secret.as_bytes(), secret.as_bytes()) {
        return Err(Refusal::Unauthorized("X-Joy-Secret is wrong"));
    }

    // After auth on purpose: garbage must not burn this hook's budget.
    if let Some(limit) = hook.limit {
        lock_guard()
            .rate(&hook.name, limit, now)
            .map_err(Refusal::TooFast)?;
    }
    Ok(())
}

/// POST the answer to the hook's callback: three attempts, 1s then 2s apart.
///
/// There is no queue: if all three fail (or the process dies first) the delivery is lost — the
/// turn itself already ran and sits in history, so the answer is never *gone*, just not pushed.
pub async fn deliver(callback: &str, payload: Value) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("(joy) the HTTP client for hook callbacks could not be built: {error}");
            return;
        }
    };
    for attempt in 1..=3u32 {
        match client.post(callback).json(&payload).send().await {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => eprintln!(
                "(joy) hook callback {callback} returned {} (attempt {attempt})",
                response.status()
            ),
            Err(error) => {
                eprintln!("(joy) hook callback {callback} failed (attempt {attempt}): {error}")
            }
        }
        if attempt < 3 {
            tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
        }
    }
    eprintln!("(joy) hook callback {callback} did not get through in three attempts; this delivery is lost (the answer is still in the session history)");
}

/// The 202 body for async hooks: the knock was accepted, the answer will be pushed.
pub fn accepted(name: &str, session: &str) -> Value {
    json!({ "accepted": true, "hook": name, "session": session })
}

pub fn find<'a>(config: &'a WebhooksConfig, name: &str) -> Option<&'a Hook> {
    config.hooks.iter().find(|hook| hook.name == name)
}

#[cfg(test)]
mod webhooks_tests {
    use super::*;

    fn sample() -> Hook {
        Hook {
            name: "deploy".to_string(),
            secret: "env:JOY_TEST_HOOK_SECRET".to_string(),
            prompt: "处理这个部署事件：\n{{payload}}".to_string(),
            session: None,
            tier: Some("fast".to_string()),
            enabled: None,
            signature: None,
            limit: None,
            callback: None,
        }
    }

    fn plain(name: &str, secret: &str) -> Hook {
        Hook {
            name: name.to_string(),
            secret: secret.to_string(),
            ..sample()
        }
    }

    fn signed(name: &str, secret: &str) -> Hook {
        Hook {
            signature: Some(true),
            ..plain(name, secret)
        }
    }

    /// A signature computed at `epoch` over `body`, with the timestamp header it was made on.
    fn signed_at(secret: &str, epoch: i64, body: &str) -> Credential<'static> {
        // Leaked on purpose: in real life the credential borrows header text owned by the request.
        let timestamp: &'static str = Box::leak(epoch.to_string().into_boxed_str());
        let signature: &'static str =
            Box::leak(signature_of(secret, timestamp, body).into_boxed_str());
        Credential {
            secret: "",
            timestamp,
            signature,
        }
    }

    #[test]
    fn missing_file_means_no_hooks_and_a_broken_one_is_loud() {
        let dir = tempfile::tempdir().expect("临时目录");
        let config = load(dir.path()).expect("没文件不是错误");
        assert!(config.hooks.is_empty());

        std::fs::write(dir.path().join(CONFIG_FILE), r#"{ "hooks": [] }"#).expect("写");
        assert!(load(dir.path()).is_ok());

        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{ "hooks": [ { "name": "x" } ] }"#,
        )
        .expect("写");
        let error = load(dir.path()).expect_err("缺 secret 必须报错");
        assert!(
            format!("{error}").contains("secret"),
            "错误里要说清是 secret：{error}"
        );
    }

    #[test]
    fn the_payload_lands_in_the_prompt_and_the_session_has_a_default() {
        let hook = sample();
        let rendered = hook.render("{\"status\":\"failed\"}");
        assert!(rendered.contains("{\"status\":\"failed\"}"));
        assert!(!rendered.contains("{{payload}}"));
        assert_eq!(hook.session_id(), "hook:deploy");

        let mut without_slot = hook.clone();
        without_slot.prompt = "看一下这个事件".to_string();
        let rendered = without_slot.render("boom");
        assert!(
            rendered.ends_with("boom"),
            "没有占位符也要把 body 带上：{rendered}"
        );
    }

    #[test]
    fn a_missing_env_var_is_not_an_empty_secret() {
        let hook = Hook {
            secret: "env:JOY_DEFINITELY_NOT_SET_12345".to_string(),
            ..sample()
        };
        assert!(
            hook.secret_value().is_none(),
            "读不到就是读不到，不能退化成空串放行"
        );

        let literal = Hook {
            secret: "  s3cret  ".to_string(),
            ..sample()
        };
        assert_eq!(literal.secret_value().as_deref(), Some("s3cret"));
    }

    /// Signature mode: the HMAC proves *who* and the timestamp proves *when* — a correct signature
    /// from twenty minutes ago is refused, and the same signature twice is a replay, not a retry.
    #[test]
    fn a_signature_proves_freshness_and_is_accepted_only_once() {
        let hook = signed("sig-a", "s3cret-a");
        let body = r#"{"status":"failed"}"#;
        let now = Instant::now();
        let epoch = 1_700_000_000;

        let credential = signed_at("s3cret-a", epoch, body);
        assert_eq!(admit(&hook, &credential, body, epoch, now), Ok(()));
        assert_eq!(
            admit(
                &hook,
                &credential,
                body,
                epoch,
                now + Duration::from_secs(1)
            ),
            Err(Refusal::Replay),
            "同一个签名不许用第二次"
        );

        // Fresh timestamp, stale signature: the body may not grow a new timestamp under an old seal.
        let stale = signed_at("s3cret-a", epoch, body);
        assert_eq!(
            admit(&hook, &stale, body, epoch + SIGNATURE_WINDOW + 1, now),
            Err(Refusal::Unauthorized(
                "the timestamp is outside the window (±300 seconds): old requests may not be replayed"
            ))
        );

        // Right shape, wrong secret: refused before the replay table is touched.
        let forged = signed_at("not-the-secret", epoch + 10, body);
        assert_eq!(
            admit(&hook, &forged, body, epoch + 10, now),
            Err(Refusal::Unauthorized("the signature is wrong"))
        );

        let missing = Credential {
            secret: "",
            timestamp: "",
            signature: "",
        };
        assert!(matches!(
            admit(&hook, &missing, body, epoch, now),
            Err(Refusal::Unauthorized(_))
        ));
    }

    /// The signature is `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))` — the exact string another
    /// side computes with `openssl dgst -sha256 -hmac`. Pinned so a refactor can't quietly change it.
    #[test]
    fn the_signature_is_plain_hex_hmac_over_timestamp_dot_body() {
        let signature = signature_of("s3cret", "1700000000", "boom");
        assert_eq!(signature.len(), 64, "SHA-256 是 32 字节 = 64 个十六进制位");
        assert!(
            signature
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "小写十六进制：{signature}"
        );
        assert_eq!(
            signature,
            signature_of("s3cret", "1700000000", "boom"),
            "同样输入必须同样输出"
        );
        // Golden vector, cross-checked against the recipe in the docs:
        //   printf '%s' '1700000000.{"status":"failed"}' | openssl dgst -sha256 -hmac s3cret -r
        assert_eq!(
            signature_of("s3cret", "1700000000", r#"{"status":"failed"}"#),
            "82ff010e35b5514d818199f607587e304942a6aafe29ccc4ce0dd21a63613135",
            "格式一旦变了，另一侧的 openssl 就验不过"
        );
        assert_ne!(
            signature,
            signature_of("s3cret", "1700000001", "boom"),
            "时间戳要真的参与"
        );
        assert_ne!(
            signature,
            signature_of("s3cret", "1700000000", "boom!"),
            "body 要真的参与"
        );
    }

    /// Plain mode: the secret alone. It cannot refuse a replay (documented), but a wrong or missing
    /// one is still 401, and a readable-secret hook never opens for an empty string.
    #[test]
    fn the_plain_mode_compares_the_secret_and_cannot_claim_freshness() {
        let hook = plain("plain-a", "s3cret-b");
        let now = Instant::now();
        let good = Credential {
            secret: "s3cret-b",
            timestamp: "",
            signature: "",
        };
        assert_eq!(admit(&hook, &good, "boom", 0, now), Ok(()));
        assert_eq!(
            admit(&hook, &good, "boom", 0, now),
            Ok(()),
            "plain 模式不拦重放 —— 这正是要 signature 的理由"
        );
        let bad = Credential {
            secret: "wrong",
            timestamp: "",
            signature: "",
        };
        assert_eq!(
            admit(&hook, &bad, "boom", 0, now),
            Err(Refusal::Unauthorized("X-Joy-Secret is wrong"))
        );

        let unreadable = Hook {
            secret: "env:JOY_DEFINITELY_NOT_SET_67890".to_string(),
            ..plain("plain-unreadable", "x")
        };
        assert!(matches!(
            admit(&unreadable, &good, "boom", 0, now),
            Err(Refusal::Unauthorized(_))
        ));
    }

    /// The cap counts **accepted** requests only: five wrong secrets must not eat the minute, and
    /// once the window rolls over the door opens again.
    #[test]
    fn the_rate_cap_counts_accepted_requests_and_resets_after_a_minute() {
        let hook = Hook {
            limit: Some(2),
            ..plain("rate-a", "s3cret-c")
        };
        let start = Instant::now();
        let good = Credential {
            secret: "s3cret-c",
            timestamp: "",
            signature: "",
        };
        let bad = Credential {
            secret: "nope",
            timestamp: "",
            signature: "",
        };

        for _ in 0..5 {
            assert!(matches!(
                admit(&hook, &bad, "boom", 0, start),
                Err(Refusal::Unauthorized(_))
            ));
        }
        assert_eq!(admit(&hook, &good, "boom", 0, start), Ok(()));
        assert_eq!(admit(&hook, &good, "boom", 0, start), Ok(()));
        match admit(&hook, &good, "boom", 0, start) {
            Err(Refusal::TooFast(wait)) => assert!((1..=60).contains(&wait), "retry after {wait}"),
            other => panic!("第三次该被限流，得到 {other:?}"),
        }
        assert_eq!(
            admit(&hook, &good, "boom", 0, start + Duration::from_secs(61)),
            Ok(()),
            "过了一分钟该重新放行"
        );
    }

    /// A config that can never pass is a typo, not a policy: both are refused at load, loudly.
    #[test]
    fn impossible_limits_and_non_http_callbacks_are_refused_at_load() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join(CONFIG_FILE);

        std::fs::write(
            &path,
            r#"{"hooks":[{"name":"x","secret":"s","prompt":"p","limit":0}]}"#,
        )
        .expect("写");
        let error = load(dir.path()).expect_err("limit 0 必须报错");
        assert!(format!("{error}").contains("limit"), "{error}");

        std::fs::write(
            &path,
            r#"{"hooks":[{"name":"x","secret":"s","prompt":"p","callback":"ftp://example.com/x"}]}"#,
        )
        .expect("写");
        let error = load(dir.path()).expect_err("ftp 不是回调");
        assert!(format!("{error}").contains("http"), "{error}");

        std::fs::write(
            &path,
            r#"{"hooks":[{"name":"x","secret":"s","prompt":"p","callback":"https://example.com/x","signature":true,"limit":6}]}"#,
        )
        .expect("写");
        let config = load(dir.path()).expect("这三个字段都要认得");
        let hook = find(&config, "x").expect("找得到");
        assert!(hook.wants_signature());
        assert_eq!(hook.limit, Some(6));
        assert_eq!(hook.callback_url(), Some("https://example.com/x"));
    }
}
