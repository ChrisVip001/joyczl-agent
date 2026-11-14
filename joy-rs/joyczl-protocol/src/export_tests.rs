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
#[ignore = "write the files; driven by scripts/write_schema.py"]
fn write_schema_fixtures() {
    let out = schema_out();
    let ts_out = out.join("typescript");
    let json_out = out.join("json");
    fs::create_dir_all(&ts_out).expect("create the typescript directory");
    fs::create_dir_all(&json_out).expect("create the json directory");

    generate_ts(&ts_out).expect("the TypeScript export fails");

    let schema = generate_json_schema();
    let defs = schema
        .get("definitions")
        .and_then(|d| d.as_object())
        .expect("the definitions are missing");
    assert!(
        defs.len() >= EXPECTED_MIN_DEFS,
        "only {} type definitions were produced, fewer than the expected {} — check the protocol_types! list in export.rs",
        defs.len(),
        EXPECTED_MIN_DEFS
    );

    let json_text = serde_json::to_string_pretty(&schema).expect("serialise the schema");
    fs::write(json_out.join("v2.json"), &json_text).expect("write v2.json");

    // Constants as a separate JSON: JSON Schema cannot hold values, and the Python side needs the method names and error codes.
    // Like v2.json: a generated artifact, checked in, and watched by the drift check.
    let consts = generate_consts_json();
    let consts_text = serde_json::to_string_pretty(&consts).expect("serialise the constants");
    fs::write(json_out.join("consts.json"), &consts_text).expect("write consts.json");

    println!(
        "TypeScript → {} ({} type definitions)",
        ts_out.display(),
        defs.len()
    );
    println!("JSON Schema → {}", json_out.join("v2.json").display());
    println!("constants → {}", json_out.join("consts.json").display());
}
