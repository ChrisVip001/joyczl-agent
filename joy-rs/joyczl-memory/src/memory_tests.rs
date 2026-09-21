//! gate 与 consolidation 的测试。
//!
//! 全部用 joyczl_provider::mock::Mock —— 模型说什么完全由测试决定，
//! 所以「失败开放」这类约定可以被精确断言。

use joyczl_provider::mock::Mock;
use joyczl_provider::ProviderError;

use super::consolidation::consolidate_if_due;
use super::gate::{extract_json, should_retrieve};
use super::retrieve_context;
use super::skills::{parse_skill_text, SkillLoader};
use joyczl_state::{Chat, Episodes, Facts};

async fn stores() -> (Facts, Episodes, Chat) {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    (
        Facts::new(pool.clone()),
        Episodes::new(pool.clone()),
        Chat::new(pool),
    )
}

// ---- gate -----------------------------------------------------------------

#[tokio::test]
async fn gate_says_retrieve_with_a_query() {
    let mock = Mock::new(vec![Mock::text(
        r#"{"retrieve": true, "query": "alex meetings", "reason": "mentions a person"}"#,
    )]);
    let d = should_retrieve(&mock, "small", "我什么时候和阿明开会").await;
    assert!(d.retrieve);
    assert_eq!(d.query, "alex meetings");
    assert_eq!(d.reason, "mentions a person");
}

#[tokio::test]
async fn gate_lets_small_talk_through() {
    let mock = Mock::new(vec![Mock::text(
        r#"{"retrieve": false, "query": "", "reason": "just math"}"#,
    )]);
    let d = should_retrieve(&mock, "small", "2+2 等于几").await;
    assert!(!d.retrieve);
}

#[tokio::test]
async fn gate_fails_open_on_model_error() {
    struct Broken;
    impl joyczl_provider::Provider for Broken {
        fn create(
            &self,
            _: joyczl_provider::CreateRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<joyczl_provider::CreateResponse, ProviderError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(std::future::ready(Err(ProviderError::Api("挂了".into()))))
        }
    }
    let d = should_retrieve(&Broken, "small", "我什么时候和阿明开会").await;
    assert!(d.retrieve, "门坏了必须失败开放");
    assert_eq!(d.query, "我什么时候和阿明开会", "检索词退回原消息");
    assert!(
        d.reason.contains("失败开放"),
        "理由要说明是失败开放：{}",
        d.reason
    );
}

#[tokio::test]
async fn gate_fails_open_when_model_only_thinks() {
    // 推理模型有时只输出思考、没有 JSON —— 不是错误，但也没答案。
    let mock = Mock::new(vec![Mock::text("让我想想……这是一个关于日程的问题。")]);
    let d = should_retrieve(&mock, "small", "我什么时候和阿明开会").await;
    assert!(d.retrieve);
    assert!(d.reason.contains("失败开放"));
}

#[tokio::test]
async fn gate_tolerates_prose_around_the_json() {
    // 推理模型常在 JSON 前后加说明。
    let mock = Mock::new(vec![Mock::text(
        "好的，判断如下：\n{\"retrieve\": true, \"query\": \"alex\", \"reason\": \"person\"}\n以上。",
    )]);
    let d = should_retrieve(&mock, "small", "alex 是谁").await;
    assert!(d.retrieve);
    assert_eq!(d.query, "alex");
}

#[test]
fn extract_json_picks_the_braces() {
    assert_eq!(
        extract_json("前缀 {\"a\":1} 后缀").as_deref(),
        Some("{\"a\":1}")
    );
    assert_eq!(extract_json("没有括号"), None);
    // 嵌套取最外层
    assert_eq!(
        extract_json("{\"a\":{\"b\":2}}").as_deref(),
        Some("{\"a\":{\"b\":2}}")
    );
}

// ---- retrieve_context -----------------------------------------------------

#[tokio::test]
async fn retrieve_context_formats_facts_and_episodes() {
    let (facts, episodes, _chat) = stores().await;
    facts
        .add("alex", "Alex prefers morning meetings", "user")
        .await
        .unwrap();
    episodes
        .add("2026-09-01", "planned the Acme demo with alex")
        .await
        .unwrap();

    let text = retrieve_context(&facts, &episodes, "alex", 4, None)
        .await
        .unwrap();
    assert!(text.contains("**alex**"), "{text}");
    assert!(
        text.contains("(2026-09-01) planned the Acme demo"),
        "{text}"
    );
}

#[tokio::test]
async fn retrieve_context_is_empty_when_nothing_matches() {
    let (facts, episodes, _chat) = stores().await;
    let text = retrieve_context(&facts, &episodes, "??? ", 4, None)
        .await
        .unwrap();
    assert!(text.is_empty(), "没检索到就不该拼标题：{text:?}");
}

// ---- consolidation --------------------------------------------------------

#[tokio::test]
async fn consolidation_waits_until_enough_exchanges() {
    let (facts, episodes, chat) = stores().await;
    chat.append_exchange("hi", "hello", "default", "cli", None)
        .await
        .unwrap();

    let mock = Mock::new(vec![]);
    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 0, "只攒了 1 轮，不该提炼");
    assert!(
        mock.received.lock().unwrap().is_empty(),
        "还没到量就不该调模型"
    );
}

#[tokio::test]
async fn consolidation_distills_facts_and_marks_rows() {
    let (facts, episodes, chat) = stores().await;
    for i in 0..6 {
        chat.append_exchange(
            &format!("msg{i}"),
            &format!("reply{i}"),
            "default",
            "cli",
            None,
        )
        .await
        .unwrap();
    }
    let mock = Mock::new(vec![Mock::text(
        r#"{"facts": [{"subject": "alex", "content": "Alex prefers mornings"}],
            "episode": "talked about scheduling"}"#,
    )]);

    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 1);

    let all = facts.recent(10, 0).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].source, "consolidation", "提炼来的要标明来源");
    assert!(
        chat.unconsolidated().await.unwrap().is_empty(),
        "成功之后才标记"
    );

    let eps = episodes.recent(10).await.unwrap();
    assert_eq!(eps.len(), 1);
}

/// 提炼出来的临时陈述不落库，真事实照落 —— 两者在同一个应答里也要分得清。
#[tokio::test]
async fn consolidation_drops_temporary_statements_but_keeps_facts() {
    let (facts, episodes, chat) = stores().await;
    for i in 0..6 {
        chat.append_exchange(&format!("m{i}"), &format!("r{i}"), "default", "cli", None)
            .await
            .unwrap();
    }
    let mock = Mock::new(vec![Mock::text(
        r#"{"facts": [
             {"subject": "plan", "content": "这个方案本次会话先用 A"},
             {"subject": "alex", "content": "Alex prefers mornings"}
           ],
           "episode": "planned the week"}"#,
    )]);

    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 1, "只该写进一条");

    let all = facts.recent(10, 0).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].subject, "alex");
}

#[tokio::test]
async fn consolidation_failure_keeps_the_log() {
    let (facts, episodes, chat) = stores().await;
    for i in 0..6 {
        chat.append_exchange(
            &format!("msg{i}"),
            &format!("reply{i}"),
            "default",
            "cli",
            None,
        )
        .await
        .unwrap();
    }

    // 模型回了一段没有 JSON 的话。
    let mock = Mock::new(vec![Mock::text("我觉得这段对话没什么值得记的。")]);
    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 0);
    assert_eq!(
        chat.unconsolidated().await.unwrap().len(),
        12,
        "日志一行都不能丢"
    );

    // 空的 subject / content 也不该写进去。
    let mock = Mock::new(vec![Mock::text(
        r#"{"facts": [{"subject": "", "content": "x"}, {"subject": "a", "content": ""}]}"#,
    )]);
    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 0);
    assert!(facts.recent(10, 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn gate_prompt_carries_the_message() {
    // 门的关键是「把用户消息带给模型」—— 忘了拼进去的话，门只会瞎判。
    let mock = Mock::new(vec![Mock::text(
        r#"{"retrieve": true, "query": "q", "reason": "r"}"#,
    )]);
    let _ = should_retrieve(&mock, "small", "阿明喜欢什么").await;
    let sent = mock.received.lock().unwrap()[0].messages[0].text();
    assert!(sent.contains("阿明喜欢什么"), "用户消息没进 prompt：{sent}");
}

// ---- consolidation 的临时陈述过滤 -------------------------------------------

#[test]
fn temporary_statements_are_kept_out_of_long_term_memory() {
    use super::consolidation::temporary_marker;
    // 中英两种标记都认。
    assert!(temporary_marker("这次会话先用这个方案").is_some());
    assert!(temporary_marker("We will use this path for now").is_some());
    assert!(temporary_marker("暂时把会议挪到下午").is_some());
    // 真事实不该误伤。
    assert!(temporary_marker("Alex 喜欢早上的会议").is_none());
    assert!(temporary_marker("The release is on October 15").is_none());
    // 大小写不敏感。
    assert!(temporary_marker("FOR NOW keep it simple").is_some());
}

// ---- skills ----------------------------------------------------------------

const SKILL_MD: &str = "\
---
name: weekly-review
description: Summarize the week and draft the Monday brief
---

1. Pull last week's episodes.
2. Distill into three bullets.
";

#[test]
fn skill_frontmatter_must_carry_name_and_description() {
    let skill = parse_skill_text(SKILL_MD).expect("合法的 SKILL.md");
    assert_eq!(skill.name, "weekly-review");
    assert!(skill.description.contains("Monday brief"));
    assert!(skill.body.starts_with("1. Pull"), "{:?}", skill.body);

    // 没有 frontmatter / 缺字段都不算技能。
    assert!(parse_skill_text("没有 frontmatter 的普通 markdown").is_none());
    assert!(parse_skill_text("---\nname: x\n---\nbody").is_none());
    // 引号包起来的值也认。
    let skill = parse_skill_text("---\nname: 'a'\ndescription: \"b\"\n---\nx\n").unwrap();
    assert_eq!(
        (skill.name.as_str(), skill.description.as_str()),
        ("a", "b")
    );
}

#[test]
fn trigger_is_transparent_keyword_overlap() {
    let dir = tempfile::tempdir().expect("临时目录");
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(skills.join("weekly-review")).unwrap();
    std::fs::write(skills.join("weekly-review").join("SKILL.md"), SKILL_MD).unwrap();

    let mut loader = SkillLoader::new(vec![skills]);

    // 「weekly」「review」「brief」三个词重合 —— 远超 2 的门槛。
    let hit = loader.matching_skills("help me do the weekly review of my inbox");
    assert!(hit.contains("Pull last week's episodes"), "{hit}");

    // 不相关的消息：一个正文都进不来（渐进披露的全部意义）。
    let miss = loader.matching_skills("2+2 等于几");
    assert!(miss.is_empty(), "不该触发：{miss}");
}

#[test]
fn a_skill_created_mid_session_is_live_next_match() {
    let dir = tempfile::tempdir().expect("临时目录");
    let skills = dir.path().join("skills");
    let mut loader = SkillLoader::new(vec![skills.clone()]);
    assert!(loader
        .matching_skills("plan the quarterly roadmap")
        .is_empty());

    // 会话中途写进来一个技能（create_skill 干的事）。
    std::fs::create_dir_all(skills.join("roadmap-planning")).unwrap();
    std::fs::write(
        skills.join("roadmap-planning").join("SKILL.md"),
        "---\nname: roadmap-planning\ndescription: plan the quarterly roadmap and milestones\n---\nStep one.",
    )
    .unwrap();

    let hit = loader.matching_skills("let us plan the quarterly roadmap now");
    assert!(hit.contains("Step one"), "目录变了必须重扫：{hit}");
}

#[tokio::test]
async fn memory_mirror_lands_as_markdown() {
    let (facts, episodes, _chat) = stores().await;
    facts.add("alex", "prefers mornings", "user").await.unwrap();
    episodes
        .add("2026-09-01", "planned the demo")
        .await
        .unwrap();

    let dir = tempfile::tempdir().expect("临时目录");
    super::export_markdown(&facts, &episodes, dir.path())
        .await
        .expect("写镜像");

    let text = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
    assert!(text.contains("**alex** — prefers mornings"), "{text}");
    assert!(text.contains("**2026-09-01** — planned the demo"), "{text}");
    assert!(text.contains("state.db"), "要说清事实来源在哪儿：{text}");
}
