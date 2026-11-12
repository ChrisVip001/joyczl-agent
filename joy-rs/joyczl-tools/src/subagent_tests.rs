//! Subagent tool contracts: how conclusions come back, how failures are worded, how recursive delegation is structurally blocked.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::subagent::{delegate_task, SubagentRunner, NAME};
use super::{BoxFut, ToolRegistry};

/// The arguments a delegation was called with (for pass-through assertions).
#[derive(Debug, Clone)]
struct Seen {
    task: String,
    max_iterations: Option<i32>,
    schema: Option<serde_json::Value>,
    /// Whether this child was told "read only" (batch members are; single delegations are not).
    readonly: bool,
}

/// A fake delegation: records the task received, returns a canned answer.
struct Fake {
    answer: Result<String, String>,
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn ok(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: Ok(answer.to_string()),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn failing(why: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: Err(why.to_string()),
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl SubagentRunner for Fake {
    fn run(
        &self,
        task: String,
        max_iterations: Option<i32>,
        result_schema: Option<serde_json::Value>,
        _parent_cwd: Option<std::path::PathBuf>,
        readonly: bool,
    ) -> BoxFut {
        self.seen.lock().expect("锁").push(Seen {
            task,
            max_iterations,
            schema: result_schema,
            readonly,
        });
        let answer = self.answer.clone();
        Box::pin(async move { answer.map_err(anyhow::Error::msg) })
    }
}

async fn ctx() -> super::ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    super::ToolCtx {
        on_uncertain: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(crate::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
        on_delta: None,
    }
}

/// Conclusions come back labeled: the parent sees tool output and must know it's "someone else speaking".
#[tokio::test]
async fn a_delegation_returns_the_summary_with_a_label() {
    let runner = Fake::ok("十点有会。");
    let tool = delegate_task(runner.clone());
    let out = match (tool.handler)(ctx().await, json!({"task": "查今天的安排"})).await {
        Ok(out) => out,
        Err(e) => panic!("不该失败：{e}"),
    };

    assert!(out.contains("A subagent has answered"), "{out}");
    assert!(out.contains("十点有会。"), "{out}");
    let seen = runner.seen.lock().expect("锁");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].task, "查今天的安排");
    assert_eq!(
        seen[0].max_iterations, None,
        "没给 max_iterations 就是 None（由 runner 定默认）"
    );
    assert_eq!(seen[0].schema, None, "没给 schema 就是 None");
}

/// A failed delegation is **text**, not an error — consistent with every tool in this layer.
#[tokio::test]
async fn a_failed_delegation_is_still_text() {
    let tool = delegate_task(Fake::failing("模型调用失败：boom"));
    let out = match (tool.handler)(ctx().await, json!({"task": "查一下"})).await {
        Ok(out) => out,
        Err(e) => panic!("不该失败：{e}"),
    };
    assert!(out.starts_with("Error:"), "要保持 Error: 前缀：{out}");
    assert!(out.contains("boom"), "{out}");
}

/// Recursive delegation is **structurally** blocked: the subagent's tool table simply has no `delegate_task`.
#[test]
fn the_child_tool_table_has_no_delegate_task() {
    let mut registry = ToolRegistry::new();
    registry.register(delegate_task(Fake::ok("结论")));
    assert!(registry.names().contains(&NAME), "父轮有它");

    let child = registry.without(NAME);
    assert!(
        !child.names().contains(&NAME),
        "子代理的表里不能有它：{:?}",
        child.names()
    );
    // The parent's copy is untouched (`without` clones, doesn't delete in place).
    assert!(registry.names().contains(&NAME));
    assert!(child.names().iter().all(|name| *name != NAME));
}

/// The task description is required — the schema stops it first (the model can fix it accordingly).
#[tokio::test]
async fn the_task_is_required() {
    let mut registry = ToolRegistry::new();
    registry.register(delegate_task(Fake::ok("结论")));
    let out = registry.execute(ctx().await, NAME, json!({})).await;
    assert!(out.starts_with("Error:") && out.contains("task"), "{out}");
}

/// `result_schema` is passed through to the runner untouched (the tool layer doesn't interpret it); conclusions come back with the "paraphrase" banner.
#[tokio::test]
async fn a_result_schema_is_passed_through_and_the_report_is_labelled() {
    let runner = Fake::ok("十点有会。");
    let tool = delegate_task(runner.clone());
    let schema = json!({"type": "object", "required": ["answer"]});

    let out = match (tool.handler)(
        ctx().await,
        json!({"task": "查一下", "result_schema": schema}),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => panic!("不该失败：{e}"),
    };

    assert_eq!(
        runner.seen.lock().expect("锁")[0].schema,
        Some(schema),
        "schema 该原样交给 runner"
    );
    assert!(
        out.contains("reported"),
        "子代理的话必须标明是reported：{out}"
    );
    assert!(out.contains("十点有会。"), "{out}");
}

// ---- Batch delegation ---------------------------------------------------------------------

/// Echoes each task back, so "answer N belongs to question N" is checkable.
struct Echo;

impl SubagentRunner for Echo {
    fn run(
        &self,
        task: String,
        _max_iterations: Option<i32>,
        _result_schema: Option<serde_json::Value>,
        _parent_cwd: Option<std::path::PathBuf>,
        _readonly: bool,
    ) -> BoxFut {
        Box::pin(async move { Ok(format!("结论：{task}")) })
    }
}

#[tokio::test]
async fn a_batch_runs_every_task_and_merges_in_the_order_asked() {
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(Arc::new(Echo)));
    let out = reg
        .execute(ctx().await, NAME, json!({"tasks": ["甲", "乙", "丙"]}))
        .await;

    for (index, task) in ["甲", "乙", "丙"].iter().enumerate() {
        let header = format!("### Subtask {}: {task}", index + 1);
        assert!(out.contains(&header), "缺了 {header}：{out}");
    }
    // 顺序：结论跟着自己的标题，一问一答不会错位。
    let first = out.find("### Subtask 1: 甲").expect("第一段");
    let second = out.find("### Subtask 2: 乙").expect("第二段");
    let third = out.find("### Subtask 3: 丙").expect("第三段");
    assert!(first < second && second < third, "顺序乱了：{out}");
    assert!(out.contains("结论：甲"), "{out}");
}

#[tokio::test]
async fn a_batch_beyond_the_cap_is_refused() {
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(Arc::new(Echo)));
    let out = reg
        .execute(
            ctx().await,
            NAME,
            json!({"tasks": ["1", "2", "3", "4", "5"]}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    // 两道关：schema 的 maxItems 先拦（模型看得见），handler 里的 MAX_BATCH 兜底。
    assert!(
        out.contains("more than 4") || out.contains("最多派 4"),
        "{out}"
    );
}

#[tokio::test]
async fn an_empty_batch_is_refused() {
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(Arc::new(Echo)));
    let out = reg.execute(ctx().await, NAME, json!({"tasks": []})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("empty array"), "{out}");
}

/// 单件委派拿全部工具（行为零变化）；批量委派必须显式告诉实现方「只读」。
#[tokio::test]
async fn only_a_batch_hands_the_child_a_readonly_table() {
    let single = Fake::ok("结论");
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(single.clone()));
    let _ = reg
        .execute(ctx().await, NAME, json!({"task": "查一件事"}))
        .await;
    {
        let seen = single.seen.lock().expect("锁");
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].readonly, "单件委派不该被降级成只读");
    }

    let batch = Fake::ok("结论");
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(batch.clone()));
    let _ = reg
        .execute(ctx().await, NAME, json!({"tasks": ["甲", "乙"]}))
        .await;
    let seen = batch.seen.lock().expect("锁");
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter().all(|call| call.readonly),
        "批量的每个子代理都必须只读：多个循环同时在跑，写同一目录会互相覆盖"
    );
}
