//! The guard's criteria and replacement rules. All pure functions — no model, no store needed.

use serde_json::json;

use super::guard::{for_model, reminder, GuardVerdict, StallGuard};

/// Observe one call, return the verdict.
fn feed(guard: &mut StallGuard, name: &str, args: serde_json::Value, output: &str) -> GuardVerdict {
    guard.observe(name, &args, output)
}

#[test]
fn three_identical_calls_in_a_row_trip_the_guard() {
    let mut guard = StallGuard::new();
    let args = json!({"subject": "alex"});

    assert!(
        feed(&mut guard, "save_note", args.clone(), "已记住").is_ok(),
        "第 1 次：还早"
    );
    assert!(
        feed(&mut guard, "save_note", args.clone(), "已记住").is_ok(),
        "第 2 次：可能是复核"
    );
    let verdict = feed(&mut guard, "save_note", args.clone(), "已记住");
    assert_eq!(verdict, GuardVerdict::Repeated { times: 3 });
    assert_eq!(guard.hits(), 1);
    assert!(guard.last_note().is_some());

    // Different args means not the same call.
    let mut guard = StallGuard::new();
    for i in 0..3 {
        feed(&mut guard, "save_note", json!({"subject": i}), "已记住");
    }
    assert_eq!(guard.hits(), 0, "参数不同不该触发");
}

#[test]
fn an_alternating_batch_is_caught_even_though_it_never_repeats_consecutively() {
    // A,B,A,B — every alternation resets the "consecutive identical" count, so it needs its own check.
    let mut guard = StallGuard::new();
    let a = json!({"q": "a"});
    let b = json!({"q": "b"});

    feed(&mut guard, "search_memory", a.clone(), "结果 A");
    feed(&mut guard, "search_memory", b.clone(), "结果 B");
    feed(&mut guard, "search_memory", a.clone(), "结果 A");
    let verdict = feed(&mut guard, "search_memory", b.clone(), "结果 B");
    assert_eq!(verdict, GuardVerdict::Cyclic { period: 2 });

    // A three-beat cycle must be caught too.
    let mut guard = StallGuard::new();
    for _ in 0..2 {
        feed(&mut guard, "t", json!({"i": 1}), "r1");
        feed(&mut guard, "t", json!({"i": 2}), "r2");
        feed(&mut guard, "t", json!({"i": 3}), "r3");
    }
    assert_eq!(guard.hits(), 1, "A,B,C 重复两遍要触发一次");
}

#[test]
fn pure_repetition_is_not_reported_as_a_cycle() {
    // One call repeated twice: neither "3 in a row", nor should it be reported as a "period-2 cycle".
    let mut guard = StallGuard::new();
    let args = json!({"x": 1});
    feed(&mut guard, "t", args.clone(), "同一结果");
    feed(&mut guard, "t", args.clone(), "同一结果");
    assert_eq!(guard.hits(), 0, "两遍重复不该报循环");
}

#[test]
fn only_long_repeated_results_are_replaced_by_a_stub() {
    let short = "很短的结果";
    let long = "x".repeat(600);
    let args = json!({"path": "/tmp/a"});

    // On the first hit (times=3) the short result is kept as-is — the stub would be longer.
    let repeated = GuardVerdict::Repeated { times: 3 };
    let text = for_model(short, "read", &args, &repeated);
    assert!(text.contains(short), "短结果不该被换掉：{text}");
    assert!(text.contains("[guard]"), "但提醒要加上：{text}");

    // Long results are stubbed from the 2nd repeat on, with an args preview in the stub.
    let twice = GuardVerdict::Repeated { times: 2 };
    let text = for_model(&long, "read", &args, &twice);
    assert!(text.contains("第 2 次相同调用"), "{text}");
    assert!(text.contains("/tmp/a"), "桩里要有参数预览：{text}");
    assert!(!text.contains("xxxx"), "长正文该被省略：{text}");
    // The 1st repeat (times=1) is not stubbed yet: two calls can be perfectly normal.
    let text = for_model(&long, "read", &args, &GuardVerdict::Repeated { times: 1 });
    assert!(text.contains("xxxx"), "第一次重复仍给全文：{}", text.len());
}

#[test]
fn a_clean_run_returns_the_output_untouched() {
    let output = "一切正常";
    assert_eq!(
        for_model(output, "t", &json!({}), &GuardVerdict::Ok),
        output
    );
    assert!(reminder(&GuardVerdict::Ok).is_none());
}
