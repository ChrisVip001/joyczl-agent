//! `joyczl-protocol` —— Joy 的跨语言契约，唯一事实来源。
//!
//! 三语言工程里最容易烂掉的地方，是「同一个概念在三处各写一遍」：
//! Rust 定义一遍、前端手抄一遍、Python SDK 再抄一遍，然后慢慢漂移。
//!
//! 本 crate 的做法：
//!
//!   * 类型只在 Rust 定义一次；
//!   * `#[derive(TS)]`    → `schema/typescript/`（ts-rs 生成）
//!   * `#[derive(JsonSchema)]` → `schema/json/v2.json`（schemars 生成，
//!     再交给 datamodel-code-generator 产出 Python 的 pydantic 模型）
//!   * 生成物 **check-in**，让类型漂移在 PR diff 里一眼看见。
//!
//! 而生成用的两个 derive 在非 test 构建下会被换成空宏，所以线上二进制
//! 既不依赖 ts-rs / schemars，也不付任何 derive 成本。见下面的 cfg 切换。

mod protocol;
pub mod rpc;

pub use protocol::v2::*;
pub use rpc::*;

// ---- 生成用 derive 的编译期切换 ------------------------------------------
// test 构建：真的展开，供 `cargo test` 触发代码生成。
// 其他构建：换成 joyczl-protocol-noop-macros 里的空宏。
#[cfg(not(test))]
pub(crate) use joyczl_protocol_noop_macros::JsonSchema;
#[cfg(not(test))]
pub(crate) use joyczl_protocol_noop_macros::TS;
#[cfg(test)]
pub(crate) use schemars::JsonSchema;
#[cfg(test)]
pub(crate) use ts_rs::TS;

#[cfg(test)]
mod export;
#[cfg(test)]
#[path = "export_tests.rs"]
mod export_tests;
