//! 钩子的契约：装载、匹配、阻断、改写、超时两分法、信任哈希。
//!
//! 需要真起子进程的用例标了 `#[cfg(unix)]`：钩子跑的是 shell 命令，Windows 上
//! 那是另一套东西（见 `docs/limitations.md`），不该假装它在两种平台上一样。

use super::hooks::{HookEvent, Hooks};
use serde_json::json;

fn write_hooks(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("hooks.json"), body).expect("写 hooks.json");
}

// ---- 装载（不需要子进程）--------------------------------------------------

#[test]
fn a_missing_file_means_no_hooks() {
    let dir = tempfile::tempdir().expect("临时目录");
    assert!(Hooks::load(dir.path(), None).is_none(), "没配就一次都不起");
}

#[test]
fn a_broken_file_config_does_not_take_the_process_down() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(dir.path(), "{ 这不是 JSON");
    assert!(Hooks::load(dir.path(), None).is_none(), "解析不了就当没配");
}

#[test]
fn unknown_events_and_entries_without_a_command_are_skipped_with_a_reason() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(
        dir.path(),
        r#"{ "hooks": {
             "NotAnEvent": [ { "command": "true" } ],
             "PreToolUse": [ { "matcher": "x" }, { "command": "true" } ]
           } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("装载");
    assert_eq!(hooks.count, 1, "只有一条是真能跑的");
    assert!(
        hooks.summary().contains("PreToolUse×1"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn disable_all_hooks_wins_over_everything() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(
        dir.path(),
        r#"{ "disableAllHooks": true, "hooks": { "PreToolUse": [ {"command": "true"} ] } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("装载");
    assert_eq!(hooks.count, 0);
    assert!(
        hooks.summary().contains("disableAllHooks"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn no_entries_means_no_hooks_at_all() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(dir.path(), r#"{ "hooks": {} }"#);
    assert!(Hooks::load(dir.path(), None).is_none());
}

// ---- 行为（要被子进程真跑一遍）--------------------------------------------

#[cfg(unix)]
fn hooks_with(dir: &std::path::Path, body: &str) -> Hooks {
    write_hooks(dir, body);
    Hooks::load(dir, None).expect("装载")
}

/// exit 2 是唯一靠退出码阻断的方式，理由从 stderr 来。
#[cfg(unix)]
#[tokio::test]
async fn exit_two_blocks_and_carries_the_reason() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PreToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo 这条不许跑 >&2; exit 2"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(outcome.blocked.as_deref(), Some("这条不许跑"));
}

/// exit 1 是**非阻塞**错误：动作照常，但要在旁注里留下痕迹。
#[cfg(unix)]
#[tokio::test]
async fn exit_one_is_an_error_but_not_a_block() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo 我这个钩子坏了 >&2; exit 1"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(outcome.blocked.is_none(), "exit 1 不该阻断");
    assert!(
        outcome.note.unwrap_or_default().contains("我这个钩子坏了"),
        "但要说出来"
    );
}

/// 决策 JSON：阻断、改写入参、改写结果、补上下文。
#[cfg(unix)]
#[tokio::test]
async fn a_decision_json_can_block_rewrite_and_add_context() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": {
             "PreToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedInput\":{\"command\":\"ls -la\"}}'"] } ],
             "PostToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedOutput\":\"我把它改短了\"}'"] } ],
             "SessionStart": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"additionalContext\":\"这个仓库的规矩在 CONTRIBUTING.md\"}'"] } ]
           } }"#,
    );

    let pre = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(pre.input.unwrap()["command"], "ls -la");

    let post = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(post.output.as_deref(), Some("我把它改短了"));

    let start = hooks
        .fire(HookEvent::SessionStart, json!({"session_id": "s1"}))
        .await;
    assert!(start
        .context
        .unwrap_or_default()
        .contains("CONTRIBUTING.md"));
}

/// `permissionDecision: allow` → 有人替用户拍板了。
#[cfg(unix)]
#[tokio::test]
async fn a_permission_decision_of_allow_counts_as_consent() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PermissionRequest": [ { "command": "sh",
             "args": ["-c", "cat >/dev/null; printf %s '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}'"] } ] } }"#,
    );
    let outcome = hooks
        .fire(
            HookEvent::PermissionRequest,
            json!({"tool_name": "run_command"}),
        )
        .await;
    assert!(outcome.allowed, "该记为已获准");
    assert!(outcome.blocked.is_none());
}

/// 超时的两分法：策略事件 fail-closed，观察事件 fail-open。
#[cfg(unix)]
#[tokio::test]
async fn a_timeout_blocks_policy_events_but_not_observers() {
    let dir = tempfile::tempdir().expect("临时目录");
    // 1 秒超时 + 睡 3 秒：必超时。
    let body = r#"{ "hooks": {
         "PreToolUse": [ { "timeout": 1, "command": "sh", "args": ["-c", "cat >/dev/null; sleep 3"] } ],
         "PostToolUse": [ { "timeout": 1, "command": "sh", "args": ["-c", "cat >/dev/null; sleep 3"] } ]
       } }"#;
    let hooks = hooks_with(dir.path(), body);

    let policy = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(
        policy.blocked.is_some(),
        "策略事件没能在时限内表态 = 不放行"
    );

    let observer = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(
        observer.blocked.is_none(),
        "观察事件超时不该影响已经发生的事"
    );
}

/// 匹配器：`*` 全中、`a|b` 任一、其余整串相等（不做正则）。
#[cfg(unix)]
#[tokio::test]
async fn matchers_pick_which_calls_the_hook_sees() {
    let dir = tempfile::tempdir().expect("临时目录");
    // 命中就往 stdout 写一段纯文本 —— 观察事件的 stdout 会变成旁注。
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "matcher": "save_note|forget_note", "command": "sh", "args": ["-c", "cat >/dev/null; echo 命中了"] }
           ] } }"#,
    );

    let hit = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "forget_note"}))
        .await;
    assert!(hit.note.unwrap_or_default().contains("命中了"));

    let miss = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(miss.note.is_none(), "没匹配就不该跑");
}

/// 运行中改掉 hooks.json：**不执行**并说明原因（对齐 trusted_hash）。
#[cfg(unix)]
#[tokio::test]
async fn a_file_changed_while_running_is_not_executed() {
    let dir = tempfile::tempdir().expect("临时目录");
    let marker = dir.path().join("跑过了");
    let hooks = hooks_with(
        dir.path(),
        &format!(
            r#"{{ "hooks": {{ "PostToolUse": [
                 {{ "command": "sh", "args": ["-c", "cat >/dev/null; touch {}"] }}
               ] }} }}"#,
            marker.display()
        ),
    );

    // 改掉文件（模拟别的进程写了它）。
    write_hooks(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [ { "command": "sh", "args": ["-c", "cat >/dev/null; echo 新内容"] } ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(!marker.exists(), "改过之后不该再执行");
    assert!(
        outcome.note.unwrap_or_default().contains("改过"),
        "要说明为什么没执行"
    );
}
