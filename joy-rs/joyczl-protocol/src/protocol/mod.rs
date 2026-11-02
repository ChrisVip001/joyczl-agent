//! 协议版本。
//!
//! v1 是搬迁期的兼容层（只读，不再新增方法）；所有新 API 一律进 v2。

pub mod v2;

#[cfg(test)]
#[path = "v2_tests.rs"]
mod v2_tests;
