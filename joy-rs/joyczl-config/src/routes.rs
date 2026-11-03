//! `<home>/model-routes.json` — the routing table that auto-picks models by task tier.
//!
//! Users configure models for "task tiers" (each tier points to an existing profile id),
//! instead of tagging each model — profiles stay as they are (provider + model + key all
//! intact); routing is just a "tier → profile id" table lookup.
//!
//! Three disciplines:
//! * **fixed is the default**: missing file, corruption, absent mode — all treated as
//!   fixed, behavior identical to the file not existing (fail-open).
//! * **Failed lookup falls back to active**: a deleted profile referenced, a mistyped
//!   mode — never blows up the turn; route_reason reports the cause honestly.
//! * **Re-read every turn**: same pattern as `load_jobs` in `schedule.rs` — edits take
//!   effect immediately, no restart needed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The three task tiers:
/// * `fast` — answer a question, fix a typo (not configured separately by default: falls to chat);
/// * `chat` — everyday conversation + tool work;
/// * `reasoning` — long reasoning, large context.
pub const TIERS: [&str; 3] = ["fast", "chat", "reasoning"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteMode {
    /// Use the active profile; routing does not intervene at all (default).
    #[default]
    #[serde(rename = "fixed")]
    Fixed,
    /// Pick a tier per turn based on task characteristics.
    #[serde(rename = "auto")]
    Auto,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouteFile {
    #[serde(default)]
    pub mode: RouteMode,
    /// tier → profile id. Missing entries follow the defaults (chat=active,
    /// reasoning=chat, fast=chat); a nonexistent id is resolved by the parser with a fallback.
    #[serde(default)]
    pub routes: HashMap<String, String>,
}

pub fn routes_path(home: &Path) -> PathBuf {
    home.join("model-routes.json")
}

/// Read the routing table. Missing file, broken JSON, wrong field types — always return
/// the default (fixed); a bad config file must never block the conversation.
pub fn load_routes(home: &Path) -> RouteFile {
    std::fs::read_to_string(routes_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_routes(home: &Path, routes: &RouteFile) -> std::io::Result<()> {
    std::fs::write(routes_path(home), serde_json::to_string_pretty(routes)?)
}

#[cfg(test)]
mod routes_tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_means_fixed_mode() {
        let dir = tempfile::tempdir().expect("临时目录");
        let routes = load_routes(dir.path());
        assert_eq!(routes.mode, RouteMode::Fixed, "没文件 = fixed");
        assert!(routes.routes.is_empty());

        std::fs::write(routes_path(dir.path()), "{ 不是 json").unwrap();
        assert_eq!(
            load_routes(dir.path()).mode,
            RouteMode::Fixed,
            "坏文件也 = fixed"
        );
    }

    #[test]
    fn the_file_round_trips_with_tiers() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut routes = RouteFile {
            mode: RouteMode::Auto,
            routes: HashMap::new(),
        };
        routes
            .routes
            .insert("reasoning".to_string(), "ds-r1".to_string());
        save_routes(dir.path(), &routes).expect("写");

        let back = load_routes(dir.path());
        assert_eq!(back.mode, RouteMode::Auto);
        assert_eq!(
            back.routes.get("reasoning").map(String::as_str),
            Some("ds-r1")
        );
    }
}
