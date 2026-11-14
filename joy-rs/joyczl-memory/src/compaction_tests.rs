//! Compaction tests: waterline arithmetic, rolling fold, failure fallback.

use joyczl_provider::mock::Mock;

use super::compaction::{newly_evicted, refresh, roll_forward, summary_section};

fn pairs(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| (format!("question {i}"), format!("answer {i}")))
        .collect()
}

/// Three handles on one temporary state.db — other test modules in this crate start alike.
async fn stores() -> (
    joyczl_state::Facts,
    joyczl_state::Episodes,
    joyczl_state::Chat,
) {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    (
        joyczl_state::Facts::new(pool.clone()),
        joyczl_state::Episodes::new(pool.clone()),
        joyczl_state::Chat::new(pool),
    )
}

#[test]
fn the_waterline_only_reports_turns_newly_pushed_out() {
    // 10 turns, window 4: 6 evicted. Never summarized → give all.
    assert_eq!(newly_evicted(&pairs(10), 4, 0).len(), 6);
    // Summarized through turn 6 → nothing new.
    assert!(newly_evicted(&pairs(10), 4, 6).is_empty());
    // Two more turns → only the two newly evicted, no full recompute.
    let fresh = newly_evicted(&pairs(12), 4, 6);
    assert_eq!(fresh.len(), 2);
    assert_eq!(fresh[0].0, "question 6");
    // Session still within the window → nothing to do.
    assert!(newly_evicted(&pairs(3), 12, 0).is_empty());
}

#[tokio::test]
async fn rolling_forward_folds_new_turns_into_the_previous_summary() {
    let mock = Mock::new(vec![Mock::text(
        "Alex likes morning meetings; the release is next Monday.",
    )]);
    let (covered, summary) = roll_forward(
        &mock,
        "small",
        Some((
            2,
            "we talked about the project schedule before.".to_string(),
        )),
        &pairs(3)[2..3],
    )
    .await;

    assert_eq!(covered, 3, "the watermark has to move forward");
    assert!(summary.contains("the release"), "{summary}");

    // Fed to the model: the previous summary and the new turns — folded, not redone.
    let sent = mock.received.lock().unwrap()[0].messages[0].text();
    assert!(
        sent.contains("we talked about the project schedule before."),
        "the previous summary was not carried over: {sent}"
    );
    assert!(
        sent.contains("answer 2"),
        "the newly pushed-out turns were left behind: {sent}"
    );
}

#[tokio::test]
async fn a_dead_summarizer_falls_back_to_a_deterministic_excerpt() {
    // The model strikes (empty queue = error). Content must survive: the fallback excerpt shows the original.
    let mock = Mock::new(vec![]);
    let (covered, summary) = roll_forward(&mock, "small", None, &pairs(2)).await;

    assert_eq!(
        covered, 2,
        "folded is folded, whether or not a model is involved"
    );
    assert!(
        summary.contains("question 0"),
        "the fallback has to keep the words: {summary}"
    );
    assert!(summary.contains("answer 1"), "{summary}");
    assert!(
        summary.starts_with("(the model summary is unavailable"),
        "{summary}"
    );
}

#[tokio::test]
async fn an_empty_model_answer_is_treated_as_a_failure() {
    // An empty string reply: treated as failure — an empty summary is lost content.
    let mock = Mock::new(vec![Mock::text("   ")]);
    let (_, summary) = roll_forward(&mock, "small", None, &pairs(1)).await;
    assert!(summary.contains("question 0"), "{summary}");
}

#[test]
fn the_summary_section_is_omitted_when_empty() {
    assert!(summary_section("   ").is_none());
    let section =
        summary_section("Alex likes morning meetings.").expect("non-empty means a section");
    assert!(
        section.contains("Earlier in this conversation"),
        "{section}"
    );
    assert!(section.contains("Alex likes morning meetings."));
}

#[tokio::test]
async fn refresh_persists_the_waterline_so_the_next_turn_does_not_recompute() {
    let (facts, episodes, chat) = stores().await;
    let _ = (facts, episodes);
    let mock = Mock::new(vec![Mock::text("the first version of the summary.")]);

    let first = refresh(&chat, &mock, "small", "s1", &pairs(5), 0)
        .await
        .expect("it can be computed");
    assert_eq!(first.as_deref(), Some("the first version of the summary."));
    let stored = chat
        .load_rollup("s1")
        .await
        .unwrap()
        .expect("it reached the store");
    assert_eq!(stored.0, 5);
    assert_eq!(stored.1, "the first version of the summary.");

    // Turn count unchanged: no second model call this round (nothing left in the queue).
    let again = refresh(&chat, &mock, "small", "s1", &pairs(5), 0)
        .await
        .expect("it does not recompute");
    assert_eq!(again.as_deref(), Some("the first version of the summary."));
    assert_eq!(
        mock.received.lock().unwrap().len(),
        1,
        "the watermark is not reached, so the model must not be called again"
    );
}

#[test]
fn the_token_budget_decides_how_many_turns_survive() {
    use super::compaction::turns_that_fit;

    let pairs = pairs(5);
    assert_eq!(
        turns_that_fit(&pairs, usize::MAX),
        5,
        "with enough budget, all of it stays"
    );
    assert_eq!(
        turns_that_fit(&pairs, 0),
        1,
        "however tight the budget, the newest turn stays"
    );
    assert!(turns_that_fit(&pairs, 1_000_000) == 5);
    assert_eq!(turns_that_fit(&[], 100), 0, "no history, no turns");

    // The budget fits only a trailing part: packed from the **newest** backwards.
    let tight = turns_that_fit(&pairs, 40);
    assert!(
        (1..5).contains(&tight),
        "five rounds do not fit, but it must not be 0 either: {tight}"
    );
}
