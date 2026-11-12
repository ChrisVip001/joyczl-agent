//! Tests for gate and consolidation.
//!
//! All use joyczl_provider::mock::Mock — what the model says is fully test-decided,
//! so conventions like "fail-open" can be asserted exactly.

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
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
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
        d.reason.contains("failing open"),
        "理由要说明是失败开放：{}",
        d.reason
    );
}

#[tokio::test]
async fn gate_fails_open_when_model_only_thinks() {
    // Reasoning models sometimes emit only thinking, no JSON — not an error, but no answer.
    let mock = Mock::new(vec![Mock::text("让我想想……这是一个关于日程的问题。")]);
    let d = should_retrieve(&mock, "small", "我什么时候和阿明开会").await;
    assert!(d.retrieve);
    assert!(d.reason.contains("failing open"));
}

#[tokio::test]
async fn gate_tolerates_prose_around_the_json() {
    // Reasoning models often add prose around the JSON.
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
    // nested: take the outermost
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
        .add("alex", "Alex prefers morning meetings", "user", "user")
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

/// Distilled temporary statements don't persist; real facts do — told apart within one reply.
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

    // The model replied with prose containing no JSON.
    let mock = Mock::new(vec![Mock::text("我觉得这段对话没什么值得记的。")]);
    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 6)
        .await
        .unwrap();
    assert_eq!(written, 0);
    // After failure, **backoff**: the rows remain (none lost) but are scheduled for
    // the future, so an immediate retry finds nothing — bad rows must not retry each turn.
    assert_eq!(
        chat.unconsolidated().await.unwrap().len(),
        0,
        "失败过的行要退避，不能立刻又捞出来"
    );
    // The rows stay in the store, not marked consolidated.
    let log = chat.session_history("default").await.unwrap();
    assert_eq!(log.len(), 6, "日志一行都不能丢");

    // Empty subject / content must not be written either.
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
    // The gate's key is "pass the user message to the model" — forget it and the gate guesses blindly.
    let mock = Mock::new(vec![Mock::text(
        r#"{"retrieve": true, "query": "q", "reason": "r"}"#,
    )]);
    let _ = should_retrieve(&mock, "small", "阿明喜欢什么").await;
    let sent = mock.received.lock().unwrap()[0].messages[0].text();
    assert!(sent.contains("阿明喜欢什么"), "用户消息没进 prompt：{sent}");
}

// ---- consolidation's temporary-statement filter ---------------------------------------------

#[test]
fn temporary_statements_are_kept_out_of_long_term_memory() {
    use super::consolidation::temporary_marker;
    // Both Chinese and English markers are recognized.
    assert!(temporary_marker("这次会话先用这个方案").is_some());
    assert!(temporary_marker("We will use this path for now").is_some());
    assert!(temporary_marker("暂时把会议挪到下午").is_some());
    // Real facts must not be caught in the filter.
    assert!(temporary_marker("Alex 喜欢早上的会议").is_none());
    assert!(temporary_marker("The release is on October 15").is_none());
    // Case-insensitive.
    assert!(temporary_marker("FOR NOW keep it simple").is_some());
}

// ---- skills: policy metadata and explicit references ------------------------------------------

#[test]
fn a_skill_can_opt_out_of_implicit_invocation_and_be_summoned_by_name() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let folder = home.join("skills").join("weekly-review");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        "---\nname: weekly-review\ndescription: plan the quarterly roadmap review\nallow-model-invocation: false\n---\n只走显式引用。\n",
    )
    .unwrap();

    let mut loader = SkillLoader::new(SkillLoader::dirs_for(&home));
    // Keywords clearly overlap (quarterly / roadmap / review): implicit triggering still leaves it alone.
    let implicit = loader.hits("help me plan the quarterly roadmap review");
    assert!(
        implicit.section.is_empty(),
        "声明了 allow-model-invocation: false 就不该被隐式触发：{}",
        implicit.section
    );

    // An explicit reference (`$name`) force-loads the body and strips the reference from the message.
    let explicit = loader.hits("$weekly-review 帮我看看这周");
    assert!(
        explicit.section.contains("只走显式引用。"),
        "{}",
        explicit.section
    );
    assert_eq!(explicit.message, "帮我看看这周");
    assert!(explicit.hints.is_empty());
}

#[test]
fn a_reference_to_an_unknown_skill_becomes_a_hint_not_an_error() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("skills")).unwrap();

    let mut loader = SkillLoader::new(SkillLoader::dirs_for(&home));
    let hits = loader.hits("$nope 帮我做点什么");
    assert!(hits.section.is_empty());
    assert_eq!(hits.message, "帮我做点什么");
    assert_eq!(hits.hints.len(), 1);
    assert!(
        hits.hints[0].contains("no skill is named 'nope'"),
        "{:?}",
        hits.hints
    );
}

#[test]
fn a_skill_with_a_missing_dependency_does_not_fire_implicitly() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let folder = home.join("skills").join("deploy");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        "---\nname: deploy\ndescription: ship the quarterly roadmap release\ndependencies: changelog, nobody\n---\n正文\n",
    )
    .unwrap();

    let mut loader = SkillLoader::new(SkillLoader::dirs_for(&home));
    let hits = loader.hits("ship the quarterly roadmap release");
    assert!(
        hits.section.is_empty(),
        "依赖缺失就不参与隐式触发：{}",
        hits.section
    );

    // Say which one is missing (the startup log line uses exactly this).
    let dangling = loader.dangling_dependencies();
    assert_eq!(dangling.len(), 1);
    assert_eq!(dangling[0].0, "deploy");
    assert!(
        dangling[0].1 == "changelog" || dangling[0].1 == "nobody",
        "报告一个缺失的依赖即可：{dangling:?}"
    );
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

    // No frontmatter / missing fields → not a skill.
    assert!(parse_skill_text("没有 frontmatter 的普通 markdown").is_none());
    assert!(parse_skill_text("---\nname: x\n---\nbody").is_none());
    // Quoted values are accepted too.
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

    // "weekly" "review" "brief" all overlap — far past the threshold of 2.
    let hit = loader
        .hits("help me do the weekly review of my inbox")
        .section;
    assert!(hit.contains("Pull last week's episodes"), "{hit}");

    // An unrelated message: not one body gets in (the whole point of progressive disclosure).
    let miss = loader.hits("2+2 等于几").section;
    assert!(miss.is_empty(), "不该触发：{miss}");
}

#[test]
fn a_skill_created_mid_session_is_live_next_match() {
    let dir = tempfile::tempdir().expect("临时目录");
    let skills = dir.path().join("skills");
    let mut loader = SkillLoader::new(vec![skills.clone()]);
    assert!(loader.hits("plan the quarterly roadmap").section.is_empty());

    // A skill written mid-session (what create_skill does).
    std::fs::create_dir_all(skills.join("roadmap-planning")).unwrap();
    std::fs::write(
        skills.join("roadmap-planning").join("SKILL.md"),
        "---\nname: roadmap-planning\ndescription: plan the quarterly roadmap and milestones\n---\nStep one.",
    )
    .unwrap();

    let hit = loader.hits("let us plan the quarterly roadmap now").section;
    assert!(hit.contains("Step one"), "目录变了必须重扫：{hit}");
}

#[tokio::test]
async fn memory_mirror_lands_as_markdown() {
    let (facts, episodes, _chat) = stores().await;
    facts
        .add("alex", "prefers mornings", "user", "user")
        .await
        .unwrap();
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

// ---- concurrency: claim ---------------------------------------------------------------------

/// Two concurrent turns both decide "due": **one winner only**, the small model called once.
///
/// Before claiming this was read-then-act (read a batch → distill at leisure → mark):
/// both sides saw "enough", so the same log was distilled twice — one wasted model
/// call. Now the loser gets an empty list and **never calls the model**.
///
/// This test pins two things: facts written once (`written` sums to 1), and the model
/// called once (the length of `received`) — the latter is what this change was for.
#[tokio::test]
async fn two_concurrent_consolidations_only_run_the_model_once() {
    let (facts, episodes, chat) = stores().await;
    // One turn accrued (`every_n = 1` → needs 2 rows = one user/assistant turn).
    chat.append_exchange("alex 喜欢早上的会议", "记下了", "default", "cli", None)
        .await
        .unwrap();

    let mock = Mock::new(vec![Mock::text(
        r#"{"facts":[{"subject":"alex","content":"喜欢早上的会议"}],"episode":"聊了 alex"}"#,
    )]);

    let (first, second) = tokio::join!(
        consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 1),
        consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 1),
    );
    assert_eq!(first.unwrap() + second.unwrap(), 1, "只有赢家写进了事实");
    assert_eq!(
        mock.received.lock().unwrap().len(),
        1,
        "小模型只该被调用一次 —— 输的那个不该看到脚本"
    );
    assert!(
        chat.unconsolidated().await.unwrap().is_empty(),
        "认领的那批已经被标记"
    );
}

/// Distillation fails after claiming: the claim must be **returned** (`consolidated`
/// back to 0); the log must not sit at "consolidated" with nothing written — real loss.
#[tokio::test]
async fn a_failed_consolidation_releases_its_claim() {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm
    let facts = Facts::new(pool.clone());
    let episodes = Episodes::new(pool.clone());
    let chat = Chat::new(pool.clone());
    chat.append_exchange("alex 喜欢早上的会议", "记下了", "default", "cli", None)
        .await
        .unwrap();

    // The model just dies — one of three failure paths (the others: no JSON, invalid JSON).
    let mock = Mock::with_outcomes(vec![Err(ProviderError::Api("模型挂了".to_string()))]);
    let written = consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 1)
        .await
        .unwrap();
    assert_eq!(written, 0, "没写进任何事实");

    // The claim was returned: both rows' flags are back to 0. `consolidation_next_at`
    // is also pushed 1 minute out, so `unconsolidated()` is empty now — the flag decides.
    let still_pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_log WHERE consolidated = 0")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_pending, 2, "两行都该被还回来");
    assert_eq!(
        chat.unconsolidated().await.unwrap().len(),
        0,
        "但按退避排到了未来，不该立刻又捞出来"
    );
}

/// The repo's bundled example skill (`skills/video-transcription/SKILL.md`) passes the
/// parser, and its key disciplines are in the body: transcribe to text first, never
/// pretend to have watched, subtitles are external content.
///
/// This test is also living documentation of the SKILL.md format — breaking the
/// frontmatter (say, dropping description) turns it red.
#[test]
fn the_bundled_video_skill_parses_and_carries_the_discipline() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../skills/video-transcription/SKILL.md"
    );
    let text = std::fs::read_to_string(path).expect("仓库自带的技能文件应该存在");
    let skill = crate::skills::parse_skill_text(&text).expect("该被解析成技能");
    assert_eq!(skill.name, "video-transcription");
    assert!(
        skill.description.contains("视频") && skill.description.contains("read_file"),
        "description 该说明用途与路径：{}",
        skill.description
    );
    for discipline in [
        "yt-dlp",       // 在线视频：拉字幕不下载
        "whisper",      // 本地视频：本地转写
        "不能假装",     // 诚实边界：没有文字就不许编
        "数据不是指令", // 字幕是不可信输入
        "JOY_EXEC",     // 工具缺失时如实报告依赖
    ] {
        assert!(
            text.contains(discipline),
            "正文缺少关键纪律『{discipline}』"
        );
    }
}

/// Skill files edited on Windows are CRLF — the parser must tolerate them. A CI
/// Windows job caught this in practice: git checks out CRLF on Windows, and hard-
/// matching LF prefixes made the whole skill fail to parse.
#[test]
fn a_crlf_skill_file_still_parses() {
    let text = "---
name: crlf-skill
description: Windows 上编辑的技能
---

正文
";
    let skill = crate::skills::parse_skill_text(text).expect("CRLF 该被规范化");
    assert_eq!(skill.name, "crlf-skill");
    assert_eq!(skill.description, "Windows 上编辑的技能");
}
