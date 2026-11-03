//! Driver test for the protocol export.
//!
//! `#[ignore]`d: it writes files and cannot run with ordinary unit tests.
//! Triggered by `just write-app-server-schema` (→ scripts/write_schema.py).
//!
//! Where to write is decided by `JOYCZL_SCHEMA_OUT`. This **only generates**:
//! formatting (prettier) and drift comparison live on the Python side — formatting
//! happens after generation, so comparing raw Rust output to formatted commits never matches.

use std::fs;

use crate::export::{
    generate_consts_json, generate_json_schema, generate_ts, schema_out, EXPECTED_MIN_DEFS,
};

#[test]
#[ignore = "写文件；由 scripts/write_schema.py 驱动"]
fn write_schema_fixtures() {
    let out = schema_out();
    let ts_out = out.join("typescript");
    let json_out = out.join("json");
    fs::create_dir_all(&ts_out).expect("创建 typescript 目录");
    fs::create_dir_all(&json_out).expect("创建 json 目录");

    generate_ts(&ts_out).expect("TypeScript 导出失败");

    let schema = generate_json_schema();
    let defs = schema
        .get("definitions")
        .and_then(|d| d.as_object())
        .expect("definitions 缺失");
    assert!(
        defs.len() >= EXPECTED_MIN_DEFS,
        "只生成了 {} 个类型定义，少于预期的 {} —— 检查 export.rs 的 protocol_types! 清单",
        defs.len(),
        EXPECTED_MIN_DEFS
    );

    let json_text = serde_json::to_string_pretty(&schema).expect("schema 序列化");
    fs::write(json_out.join("v2.json"), &json_text).expect("写 v2.json");

    // Constants as a separate JSON: JSON Schema cannot hold values, and the Python side needs the method names and error codes.
    // Like v2.json: a generated artifact, checked in, and watched by the drift check.
    let consts = generate_consts_json();
    let consts_text = serde_json::to_string_pretty(&consts).expect("常量序列化");
    fs::write(json_out.join("consts.json"), &consts_text).expect("写 consts.json");

    println!(
        "TypeScript → {}（{} 个类型定义）",
        ts_out.display(),
        defs.len()
    );
    println!("JSON Schema → {}", json_out.join("v2.json").display());
    println!("常量 → {}", json_out.join("consts.json").display());
}
