//! 按任务档位自动选模型（抄 openhuman 的 tier 抽象层：
//! "You don't pick the model, the task does."）。
//!
//! 路由是**服务端策略**，不是模型的能力 —— 与 `/model` 同一条人类授权边界：
//! 模型没有换模型的工具，用户 pin（active profile）永远优先。`fixed` 模式
//! （默认，路由文件不存在也是它）下这个模块整个是透明的。

use joyczl_config::routes::{load_routes, RouteFile, RouteMode};
use joyczl_provider::Resolved;

use crate::Server;

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

impl Server {
    /// 每轮一次：决定这一轮用哪套模型。`base` 是 active profile 的解析结果 ——
    /// fixed 模式、选中同一套、或任何失败，都原样返回它（失败开放，
    /// 绝不因路由炸掉一轮；回落原因进 `route_reason`，不静默）。
    pub(crate) fn resolved_for_turn(
        &self,
        base: Resolved,
        message: &str,
    ) -> (Resolved, Option<String>) {
        let routes = load_routes(&self.settings().home);
        if routes.mode != RouteMode::Auto {
            return (base, None);
        }
        let active_id = {
            let file = self.models.read().expect("models 锁不该中毒");
            file.active.clone()
        };
        let tier = pick_tier(
            joyczl_provider::tokens::estimate_text(message),
            base.context_window(),
        );
        let Some(profile_id) = profile_for_tier(&routes, tier, active_id.as_deref()) else {
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
