//! Auto-select the model by task tier (the task-tier abstraction:
//! "You don't pick the model, the task does.").
//!
//! Routing is a **server-side policy**, not a model capability — the same human-authorization
//! boundary as `/model`: the model has no tool to change models, and the user's pin (active profile)
//! always wins. In `fixed` mode (the default, also when the routes file is absent) this whole module is transparent.

use joyczl_config::routes::{load_routes, RouteFile, RouteMode, TIERS};
use joyczl_protocol::{codes, ErrorObject};
use joyczl_provider::{tokens, Resolved};

use crate::Server;

/// Lower bound of the fuzzy band: below it, a message is obviously chat (a quick question, a small
/// errand) and not worth a classify call; above half the window, L0 already rules reasoning directly.
/// Classification happens only inside this band — without it, pure rules; this classifier is an optional extra.
const CLASSIFY_MIN_TOKENS: usize = 2_000;

/// Does this estimate fall in the fuzzy band where "L0 is unsure"?
pub(crate) fn needs_classify(estimated_tokens: usize, chat_window: u32) -> bool {
    let half = (chat_window as u64 / 2).max(1) as usize;
    estimated_tokens >= CLASSIFY_MIN_TOKENS && estimated_tokens < half
}

/// Parse the classifier's reply. **fail-open**: vague, extra words, no mention of reasoning —
/// all treated as chat. Misjudging as chat costs one extra round of the ordinary model; misjudging as
/// reasoning wastes money on the expensive model, so bias conservative.
pub(crate) fn parse_tier_reply(text: &str) -> Option<&'static str> {
    let first = text.trim().to_lowercase();
    if first.is_empty() {
        return None;
    }
    if first.starts_with("reasoning") {
        Some("reasoning")
    } else {
        None
    }
}

/// Token estimate over half the chat tier's window → switch to the reasoning tier.
/// The threshold follows the chat tier's own context window (self-tuning); no new knob.
pub(crate) fn pick_tier(estimated_tokens: usize, chat_window: u32) -> &'static str {
    let half = (chat_window as u64 / 2).max(1) as usize;
    if estimated_tokens >= half {
        "reasoning"
    } else {
        "chat"
    }
}

/// Validate a declared tier. A typo must be refused, not silently read as "chat": the caller
/// (task API / webhook / task card) pinned a tier precisely so it would not be guessed at.
pub(crate) fn clean_tier(raw: Option<&str>) -> Result<Option<String>, ErrorObject> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    if TIERS.contains(&text) {
        return Ok(Some(text.to_string()));
    }
    Err(ErrorObject {
        code: codes::INVALID_PARAMS,
        message: format!("tier 只能是 {} —— 收到 '{text}'", TIERS.join(" / ")),
        data: None,
    })
}

/// Tier → profile id fallback chain (missing table entries fall through level by level; the last hop is active):
/// `reasoning` → `chat` → active; `fast` → `chat` → active; `chat` → active.
/// A deleted profile referenced by the table falls back in `resolved_profile` — this only looks up the table.
pub(crate) fn profile_for_tier(
    routes: &RouteFile,
    tier: &str,
    active_id: Option<&str>,
) -> Option<String> {
    let chat = routes
        .routes
        .get("chat")
        .cloned()
        .or_else(|| active_id.map(str::to_string));
    match tier {
        "reasoning" => routes.routes.get("reasoning").cloned().or(chat),
        "fast" => routes.routes.get("fast").cloned().or(chat),
        _ => chat,
    }
}

/// The L1 classifier (the same cheap-model pattern as gate/judge): no tools, reads only the message,
/// replies with one word. Any failure returns None (the caller keeps the L0 conclusion).
async fn classify_tier(cheap: &Resolved, message: &str) -> Option<String> {
    use joyczl_provider::{ContentBlock, CreateRequest, Message};

    const MAX_TOKENS: i32 = 256;
    let system = "判断这条用户消息是不是需要深度推理的任务（多步数学、长链推导、复杂规划、代码架构设计）。\
                  日常问答、工具调用、写作改写都算 chat。\n只回一个词：reasoning 或 chat。不要别的话。";
    let response = cheap
        .client
        .create(CreateRequest {
            model: cheap.small_model.clone(),
            system: Some(system.to_string()),
            messages: vec![Message::user_text(format!(
                "<message>\n{}\n</message>",
                message.chars().take(4_000).collect::<String>()
            ))],
            tools: Vec::new(),
            max_tokens: MAX_TOKENS,
        })
        .await
        .ok()?;
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    parse_tier_reply(&text).map(str::to_string)
}

impl Server {
    /// Run once per turn: decide which profile this turn uses. `base` is the active profile's resolution —
    /// in fixed mode, when the same profile is selected, or on any failure, it is returned as-is
    /// (fail-open; routing must never blow up a turn; the fallback reason goes into `route_reason`, unhidden).
    pub(crate) async fn resolved_for_turn(
        &self,
        base: Resolved,
        message: &str,
        tier: Option<&str>,
    ) -> (Resolved, Option<String>) {
        let routes = load_routes(&self.settings().home);
        let active_id = {
            let file = self.models.read().expect("models 锁不该中毒");
            file.active.clone()
        };
        // A **declared** tier wins over the mode. `fixed` is the default policy, not a veto: a task
        // card or a webhook saying "this is reasoning work" is a human instruction, like `/model`.
        // Tier names were validated upstream (`clean_tier`), so anything here is one of the three.
        if let Some(tier) = tier {
            let Some(profile_id) = profile_for_tier(&routes, tier, active_id.as_deref()) else {
                return (
                    base,
                    Some(format!("tier:{tier}: 没有可用的 profile，按 active 跑")),
                );
            };
            if active_id.as_deref() == Some(profile_id.as_str()) {
                return (base, Some(format!("tier:{tier} → {profile_id}（当前）")));
            }
            return match self.resolved_profile(&profile_id) {
                Ok(resolved) => (resolved, Some(format!("tier:{tier} → {profile_id}"))),
                Err(why) => (base, Some(format!("fallback: {why}"))),
            };
        }
        // The JOY_MODEL_ROUTE env var can **force** the mode (auto/fixed) — unattended runs
        // and CI cannot edit the home files; the file's value is its default.
        let mode = match std::env::var("JOY_MODEL_ROUTE").as_deref() {
            Ok("auto") => RouteMode::Auto,
            Ok("fixed") => RouteMode::Fixed,
            _ => routes.mode,
        };
        if mode != RouteMode::Auto {
            return (base, None);
        }
        let estimated = tokens::estimate_text(message);
        let window = base.context_window();
        let mut tier: String = pick_tier(estimated, window).to_string();
        // L1: ask a cheap model once inside the fuzzy band (active's small_model, always present).
        // Failure/vagueness fail-opens back to the L0 conclusion — routing must never become a new failure point.
        if needs_classify(estimated, window) {
            if let Some(said) = classify_tier(&base, message).await {
                tier = said;
            }
        }
        let Some(profile_id) = profile_for_tier(&routes, &tier, active_id.as_deref()) else {
            return (
                base,
                Some("auto: 没有可用的 profile，按 active 跑".to_string()),
            );
        };
        if active_id.as_deref() == Some(profile_id.as_str()) {
            // The selected one is already active — no reason needed; the turn footer stays clean.
            return (base, None);
        }
        match self.resolved_profile(&profile_id) {
            Ok(resolved) => (resolved, Some(format!("auto:{tier} → {profile_id}"))),
            Err(why) => (base, Some(format!("fallback: {why}"))),
        }
    }

    /// Resolve the concrete profile for a tier. Each profile is resolved once — the provider client
    /// holds the connection pool; rebuilding it every turn is wasteful; when a profile is edited/deleted the whole table is invalidated (see `reapply`).
    fn resolved_profile(&self, profile_id: &str) -> Result<Resolved, String> {
        if let Some(hit) = self
            .route_cache
            .lock()
            .expect("route 缓存锁不该中毒")
            .get(profile_id)
        {
            return Ok(hit.clone());
        }
        let file = self.models.read().expect("models 锁不该中毒").clone();
        let profile = joyczl_config::models::find(&file, profile_id)
            .ok_or_else(|| format!("路由表引用的 '{profile_id}' 不存在"))?
            .clone();
        let mut settings = self.settings();
        let home = settings.home.clone();
        {
            let mut store = self.secrets.lock().expect("secrets 锁不该中毒");
            joyczl_config::models::apply_profile(&profile, &home, &mut store, &mut settings);
        }
        let resolved = joyczl_provider::resolve_with(&settings, &self.catalog)
            .map_err(|why| format!("'{profile_id}' 解析失败：{why}"))?;
        self.route_cache
            .lock()
            .expect("route 缓存锁不该中毒")
            .insert(profile_id.to_string(), resolved.clone());
        Ok(resolved)
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod route_tests;
