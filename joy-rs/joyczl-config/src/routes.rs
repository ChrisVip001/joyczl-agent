//! `<home>/model-routes.json` —— 按任务档位自动选模型的路由表。
//!
//! 抄 openhuman 的 tier 抽象层：**「You don't pick the model, the task does.」**
//! 用户给「任务档位」配模型（每个档位指向一个现成 profile 的 id），而不是给
//! 每个模型贴标签 —— profile 本身保持原样（provider + model + key 一应俱全），
//! 路由只做「档位 → profile id」的查表。
//!
//! 三条纪律：
//! * **fixed 是默认**：文件不存在、损坏、mode 缺省 —— 全部按 fixed 处理，
//!   行为与没有这个文件时完全一致（失败开放）。
//! * **查表失败回落 active**：引用了已删除的 profile、mode 写错 —— 不炸轮次，
//!   在 route_reason 里如实说原因。
//! * **每轮重读**：与 `schedule.rs` 的 `load_jobs` 同一模式 —— 改文件即生效，
//!   不需要重启。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 三个任务档位（openhuman 有 7 个，joy 只留真正用得上的三个 —— 没有消费者的
/// 档位是死配置）：
/// * `fast` —— 答一句、改个错字（默认不单独配：落到 chat 那套）；
/// * `chat` —— 日常对话 + 工具活；
/// * `reasoning` —— 长推理、大上下文。
pub const TIERS: [&str; 3] = ["fast", "chat", "reasoning"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteMode {
    /// 用 active profile，路由完全不介入（默认）。
    #[default]
    #[serde(rename = "fixed")]
    Fixed,
    /// 每轮按任务特征选档位。
    #[serde(rename = "auto")]
    Auto,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouteFile {
    #[serde(default)]
    pub mode: RouteMode,
    /// tier → profile id。缺项按缺省规则补（chat=active、reasoning=chat、
    /// fast=chat）；引用了不存在的 id 由解析层回落并说明。
    #[serde(default)]
    pub routes: HashMap<String, String>,
}

pub fn routes_path(home: &Path) -> PathBuf {
    home.join("model-routes.json")
}

/// 读路由表。文件不存在、JSON 损坏、字段类型不对 —— 一律回默认（fixed），
/// 绝不让一个坏配置文件挡住对话。
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
