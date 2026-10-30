//! 按任务档位自动选模型（抄 openhuman 的 tier 抽象层：
//! "You don't pick the model, the task does."）。
//!
//! 路由是**服务端策略**，不是模型的能力 —— 与 `/model` 同一条人类授权边界：
//! 模型没有换模型的工具，用户 pin（active profile）永远优先。`fixed` 模式
//! （默认，路由文件不存在也是它）下这个模块整个是透明的。

use joyczl_config::routes::{load_routes, RouteFile, RouteMode};
use joyczl_provider::{tokens, Resolved};

use crate::Server;

/// 模糊带下界：低于它的消息显然是 chat（问一句、派个活），不值得为它
/// 花一次分类调用；高于半窗口的已被 L0 直接判成 reasoning。分类只发生在
/// 这条带里 —— openhuman 干脆不分类（纯规则），这个分类器是可选项里的可选项。
const CLASSIFY_MIN_TOKENS: usize = 2_000;

/// 这条估算量落在「L0 拿不准」的模糊带里吗？
pub(crate) fn needs_classify(estimated_tokens: usize, chat_window: u32) -> bool {
    let half = (chat_window as u64 / 2).max(1) as usize;
    estimated_tokens >= CLASSIFY_MIN_TOKENS && estimated_tokens < half
}

/// 解析分类器的回答。**fail-open**：含糊、多余的话、没提到 reasoning ——
/// 一律当 chat。误判成 chat 的代价是多走一轮普通模型，误判成 reasoning
/// 的代价是白花贵模型的钱，所以偏向保守。
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

/// token 估算超过 chat 档窗口的一半 → 换 reasoning 档。
/// 阈值跟着 chat 档自己的上下文窗口走（self-tuning），不设新旋钮。
pub(crate) fn pick_tier(estimated_tokens: usize, chat_window: u32) -> &'static str {
    let half = (chat_window as u64 / 2).max(1) as usize;
    if estimated_tokens >= half {
        "reasoning"
    } else {
        "chat"
    }
}

/// 档位 → profile id 的缺省链（表缺项时逐级回落，最后一跳是 active）：
/// `reasoning` → `chat` → active；`fast` → `chat` → active；`chat` → active。
/// 表里引用了已删除的 profile 由 `resolved_profile` 回落 —— 这里只管查表。
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

/// L1 分类器（与 gate/judge 同一条便宜模型模式）：无工具、只看消息、
/// 回一个词。任何失败都返回 None（调用方保持 L0 结论）。
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
    /// 每轮一次：决定这一轮用哪套模型。`base` 是 active profile 的解析结果 ——
    /// fixed 模式、选中同一套、或任何失败，都原样返回它（失败开放，
    /// 绝不因路由炸掉一轮；回落原因进 `route_reason`，不静默）。
    pub(crate) async fn resolved_for_turn(
        &self,
        base: Resolved,
        message: &str,
    ) -> (Resolved, Option<String>) {
        let routes = load_routes(&self.settings().home);
        // JOY_MODEL_ROUTE 环境变量可**强制**指定模式（auto/fixed）—— 无人值守
        // 与 CI 里改不了 home 文件；文件里的值是它的缺省。
        let mode = match std::env::var("JOY_MODEL_ROUTE").as_deref() {
            Ok("auto") => RouteMode::Auto,
            Ok("fixed") => RouteMode::Fixed,
            _ => routes.mode,
        };
        if mode != RouteMode::Auto {
            return (base, None);
        }
        let active_id = {
            let file = self.models.read().expect("models 锁不该中毒");
            file.active.clone()
        };
        let estimated = tokens::estimate_text(message);
        let window = base.context_window();
        let mut tier: String = pick_tier(estimated, window).to_string();
        // L1：模糊带里问一次便宜模型（active 的 small_model，永远存在）。
        // 失败/含糊 fail-open 回 L0 的结论 —— 路由绝不能成为新的故障点。
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
            // 选中的就是 active —— 无需 reason，turn 页脚保持干净。
            return (base, None);
        }
        match self.resolved_profile(&profile_id) {
            Ok(resolved) => (resolved, Some(format!("auto:{tier} → {profile_id}"))),
            Err(why) => (base, Some(format!("fallback: {why}"))),
        }
    }

    /// 按档位解析具体 profile。每个 profile 只解析一次 —— provider client
    /// 连着连接池，每轮重建太浪费；profile 被改/删时整表作废（见 `reapply`）。
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
        let resolved = joyczl_provider::resolve(&settings)
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
