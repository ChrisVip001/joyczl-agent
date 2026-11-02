//! 护栏的判据与替换规则。全是纯函数，不需要模型也不需要库。

use serde_json::json;

use super::guard::{for_model, reminder, GuardVerdict, StallGuard};

/// 观察一次调用，返回判定。
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

    // 参数变了就不算同一个调用。
    let mut guard = StallGuard::new();
    for i in 0..3 {
        feed(&mut guard, "save_note", json!({"subject": i}), "已记住");
    }
    assert_eq!(guard.hits(), 0, "参数不同不该触发");
}

#[test]
fn an_alternating_batch_is_caught_even_though_it_never_repeats_consecutively() {
    // A,B,A,B —— 每次交替都会把「连续相同」的计数重置，所以必须单独判。
    let mut guard = StallGuard::new();
    let a = json!({"q": "a"});
    let b = json!({"q": "b"});

    feed(&mut guard, "search_memory", a.clone(), "结果 A");
    feed(&mut guard, "search_memory", b.clone(), "结果 B");
    feed(&mut guard, "search_memory", a.clone(), "结果 A");
    let verdict = feed(&mut guard, "search_memory", b.clone(), "结果 B");
    assert_eq!(verdict, GuardVerdict::Cyclic { period: 2 });

    // 三拍子的循环同样要抓到。
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
    // 同一次调用重复两遍：既不满足「连续 3 次」，也不该被报成「周期 2 的循环」。
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

    // 第 1 次命中（times=3）时短结果原样保留 —— 桩比原文还长。
    let repeated = GuardVerdict::Repeated { times: 3 };
    let text = for_model(short, "read", &args, &repeated);
    assert!(text.contains(short), "短结果不该被换掉：{text}");
    assert!(text.contains("[guard]"), "但提醒要加上：{text}");

    // 长结果从第 2 次重复起换桩，桩里留着参数预览。
    let twice = GuardVerdict::Repeated { times: 2 };
    let text = for_model(&long, "read", &args, &twice);
    assert!(text.contains("第 2 次相同调用"), "{text}");
    assert!(text.contains("/tmp/a"), "桩里要有参数预览：{text}");
    assert!(!text.contains("xxxx"), "长正文该被省略：{text}");
    // 第 1 次重复（times=1）还不换桩：可能是正常的两次调用。
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
