//! 上下文压缩的测试：水位线算术、滚动折叠、失败兜底。

use joyczl_provider::mock::Mock;

use super::compaction::{newly_evicted, refresh, roll_forward, summary_section};

fn pairs(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| (format!("问题 {i}"), format!("回答 {i}")))
        .collect()
}

/// 一个临时 state.db 上的三个句柄 —— 同一个 crate 的其它测试模块也这么起。
async fn stores() -> (
    joyczl_state::Facts,
    joyczl_state::Episodes,
    joyczl_state::Chat,
) {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    std::mem::forget(dir); // sqlite 要写 -wal/-shm，目录不能提前消失
    (
        joyczl_state::Facts::new(pool.clone()),
        joyczl_state::Episodes::new(pool.clone()),
        joyczl_state::Chat::new(pool),
    )
}

#[test]
fn the_waterline_only_reports_turns_newly_pushed_out() {
    // 10 轮、窗口 4：被挤出 6 轮。还没摘要过 → 全给。
    assert_eq!(newly_evicted(&pairs(10), 4, 0).len(), 6);
    // 已经摘要到 6 轮 → 没有新东西。
    assert!(newly_evicted(&pairs(10), 4, 6).is_empty());
    // 又聊了两轮 → 只给新挤出的那两轮，不重算全史。
    let fresh = newly_evicted(&pairs(12), 4, 6);
    assert_eq!(fresh.len(), 2);
    assert_eq!(fresh[0].0, "问题 6");
    // 会话还没超过窗口 → 什么都不用做。
    assert!(newly_evicted(&pairs(3), 12, 0).is_empty());
}

#[tokio::test]
async fn rolling_forward_folds_new_turns_into_the_previous_summary() {
    let mock = Mock::new(vec![Mock::text("阿明喜欢早上的会议；下周一要发版。")]);
    let (covered, summary) = roll_forward(
        &mock,
        "small",
        Some((2, "之前聊过项目排期。".to_string())),
        &pairs(3)[2..3],
    )
    .await;

    assert_eq!(covered, 3, "水位线要往前走");
    assert!(summary.contains("发版"), "{summary}");

    // 喂给模型的东西：既有上一版摘要，也有新轮次 —— 折叠而不是重来。
    let sent = mock.received.lock().unwrap()[0].messages[0].text();
    assert!(
        sent.contains("之前聊过项目排期。"),
        "上一版摘要没带上：{sent}"
    );
    assert!(sent.contains("回答 2"), "新挤出去的轮次没带上：{sent}");
}

#[tokio::test]
async fn a_dead_summarizer_falls_back_to_a_deterministic_excerpt() {
    // 模型罢工（队列空 = 报错）。内容绝不能丢：兜底摘录里要能看见原文。
    let mock = Mock::new(vec![]);
    let (covered, summary) = roll_forward(&mock, "small", None, &pairs(2)).await;

    assert_eq!(covered, 2, "折叠过就是折叠过，跟上不上模型无关");
    assert!(summary.contains("问题 0"), "兜底要留住原话：{summary}");
    assert!(summary.contains("回答 1"), "{summary}");
    assert!(summary.starts_with("（模型摘要不可用"), "{summary}");
}

#[tokio::test]
async fn an_empty_model_answer_is_treated_as_a_failure() {
    // 回了个空字符串：与失败同等对待 —— 空摘要等于丢内容。
    let mock = Mock::new(vec![Mock::text("   ")]);
    let (_, summary) = roll_forward(&mock, "small", None, &pairs(1)).await;
    assert!(summary.contains("问题 0"), "{summary}");
}

#[test]
fn the_summary_section_is_omitted_when_empty() {
    assert!(summary_section("   ").is_none());
    let section = summary_section("阿明喜欢早会。").expect("非空就有段落");
    assert!(
        section.contains("Earlier in this conversation"),
        "{section}"
    );
    assert!(section.contains("阿明喜欢早会。"));
}

#[tokio::test]
async fn refresh_persists_the_waterline_so_the_next_turn_does_not_recompute() {
    let (facts, episodes, chat) = stores().await;
    let _ = (facts, episodes);
    let mock = Mock::new(vec![Mock::text("第一版摘要。")]);

    let first = refresh(&chat, &mock, "small", "s1", &pairs(5), 0)
        .await
        .expect("算得出来");
    assert_eq!(first.as_deref(), Some("第一版摘要。"));
    let stored = chat.load_rollup("s1").await.unwrap().expect("落了库");
    assert_eq!(stored.0, 5);
    assert_eq!(stored.1, "第一版摘要。");

    // 轮次没涨：这一轮不再调模型（队列里没有第二句）。
    let again = refresh(&chat, &mock, "small", "s1", &pairs(5), 0)
        .await
        .expect("不重算");
    assert_eq!(again.as_deref(), Some("第一版摘要。"));
    assert_eq!(
        mock.received.lock().unwrap().len(),
        1,
        "水位线没过，不该再调一次模型"
    );
}
