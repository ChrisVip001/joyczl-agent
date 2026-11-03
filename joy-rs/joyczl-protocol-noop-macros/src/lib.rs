//! No-op derive macros.
//!
//! Protocol types carry `#[derive(TS, JsonSchema)]`, but `ts-rs` and `schemars`
//! are **code-generation** tools — they must not enter the shipped dependency
//! tree. The approach:
//!
//! ```ignore
//! #[cfg(test)]      pub(crate) use ts_rs::TS;
//! #[cfg(not(test))] pub(crate) use joyczl_protocol_noop_macros::TS;
//! ```
//!
//! So `cargo build` never touches ts-rs/schemars; only `cargo test` actually
//! expands them.
//!
//! Both macros must declare the same helper-attribute list (`ts` / `schemars` /
//! `serde`), otherwise the `#[ts(export_to = "v2/")]` on a type is reported as
//! an unknown attribute and the build fails.

use proc_macro::TokenStream;

/// `#[derive(TS)]` under a non-test build: generates nothing.
#[proc_macro_derive(TS, attributes(ts, serde))]
pub fn derive_ts(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// `#[derive(JsonSchema)]` under a non-test build: generates nothing.
#[proc_macro_derive(JsonSchema, attributes(schemars, serde))]
pub fn derive_json_schema(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
