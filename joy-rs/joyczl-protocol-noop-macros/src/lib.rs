//! 空操作 derive 宏。
//!
//! 协议类型挂着 `#[derive(TS, JsonSchema)]`，但 `ts-rs` 和 `schemars` 只是
//! **代码生成**工具，不该进入线上依赖树。做法：
//!
//! ```ignore
//! #[cfg(test)]      pub(crate) use ts_rs::TS;
//! #[cfg(not(test))] pub(crate) use joyczl_protocol_noop_macros::TS;
//! ```
//!
//! 于是 `cargo build` 完全不碰 ts-rs/schemars，只有 `cargo test` 才会真正展开。
//!
//! 两个宏都必须声明同一个 helper attribute 列表（`ts` / `schemars` / `serde`），
//! 否则类型上的 `#[ts(export_to = "v2/")]` 会被当成未知属性报编译错误。

use proc_macro::TokenStream;

/// 非 test 构建下的 `#[derive(TS)]`：什么都不生成。
#[proc_macro_derive(TS, attributes(ts, serde))]
pub fn derive_ts(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// 非 test 构建下的 `#[derive(JsonSchema)]`：什么都不生成。
#[proc_macro_derive(JsonSchema, attributes(schemars, serde))]
pub fn derive_json_schema(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
