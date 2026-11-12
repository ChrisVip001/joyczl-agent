//! The memory center's arithmetic and its two decoders. No database, no files: these are
//! the parts that can be wrong in a way the panel would show confidently.

use crate::memory_center::{preview_of, usage_of};

/// The meter warns exactly when the configured ratio is crossed — never earlier, never by
/// a different denominator than the one it prints.
#[test]
fn the_meter_warns_at_the_threshold_it_was_given() {
    let full = usage_of(
        vec![("soul", 100), ("memory", 50), ("history", 850)],
        1000,
        85,
    );
    assert_eq!(full.used_tokens, 1_000);
    assert!(
        (full.ratio - 1.0).abs() < 1e-9,
        "比例要按预算算：{}",
        full.ratio
    );
    assert!(full.notice.is_some(), "100% 必须提示");

    let calm = usage_of(vec![("soul", 100), ("history", 400)], 1000, 85);
    assert_eq!(calm.used_tokens, 500);
    assert!(calm.notice.is_none(), "50% 不该弹提示");
}

/// A zero budget (no window resolved yet) must not divide by zero into `inf` — the panel
/// would render "inf%" and nobody would trust the rest of it again.
#[test]
fn a_missing_budget_reads_as_zero_rather_than_infinity() {
    let usage = usage_of(vec![("soul", 120)], 0, 85);
    assert_eq!(usage.ratio, 0.0);
    assert!(usage.notice.is_none());
}

/// A ledger row must be readable without being opened: the payload's own short field wins,
/// then anything textual, then the raw bytes. Folded backfill rows are not always JSON.
#[test]
fn a_ledger_preview_uses_the_payloads_own_text() {
    assert_eq!(preview_of("{\"text\":\"第一行\\n第二行\"}"), "第一行");
    assert_eq!(
        preview_of("{\"preview\":\"ptr → spill/…\",\"bytes\":12}"),
        "ptr → spill/…"
    );
    assert_eq!(preview_of("{\"kind\":\"image\"}"), "{\"kind\":\"image\"}");
    assert_eq!(preview_of("纯文本的一行"), "纯文本的一行");
}

/// Long single lines are clipped for the list row; the full bytes stay in the ledger.
#[test]
fn a_long_preview_is_clipped_not_squeezed() {
    let long = "字".repeat(400);
    let clipped = preview_of(&format!("{{\"text\":\"{long}\"}}"));
    assert_eq!(clipped.chars().count(), 160);
}
