//! 协议导出的驱动测试。
//!
//! 被 `#[ignore]`：它会写文件，不能跟普通单测一起跑。
//! 由 `just write-app-server-schema`（→ scripts/write_schema.py）触发。
//!
//! 写到哪由 `JOYCZL_SCHEMA_OUT` 决定。这里**只管生成**：排版（prettier）和
//! 漂移比对都在 Python 侧做 —— 因为排版发生在生成之后，在 Rust 里比对会
//! 拿未排版的产物去比已排版的提交物，永远不相等。

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

    // 常量单独一份 JSON：JSON Schema 装不下值，而 Python 侧要拿方法名和错误码。
    // 跟 v2.json 一样是生成物、一样 check-in、一样被漂移检查盯着。
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
