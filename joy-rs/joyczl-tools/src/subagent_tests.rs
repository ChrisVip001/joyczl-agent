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
        self.seen.lock().expect("lock").push(Seen {
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
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    super::ToolCtx {
        on_uncertain: None,
        resource_lease: None,
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
    let runner = Fake::ok("meeting at ten.");
    let tool = delegate_task(runner.clone());
    let out = match (tool.handler)(ctx().await, json!({"task": "check today's schedule"})).await {
        Ok(out) => out,
        Err(e) => panic!("must not fail: {e}"),
    };

    assert!(out.contains("A subagent has answered"), "{out}");
    assert!(out.contains("meeting at ten."), "{out}");
    let seen = runner.seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].task, "check today's schedule");
    assert_eq!(
        seen[0].max_iterations, None,
        "no max_iterations means None (the runner picks the default)"
    );
    assert_eq!(seen[0].schema, None, "no schema means None");
}

/// A failed delegation is **text**, not an error — consistent with every tool in this layer.
#[tokio::test]
async fn a_failed_delegation_is_still_text() {
    let tool = delegate_task(Fake::failing("the model call failed: boom"));
    let out = match (tool.handler)(ctx().await, json!({"task": "check it"})).await {
        Ok(out) => out,
        Err(e) => panic!("must not fail: {e}"),
    };
    assert!(
        out.starts_with("Error:"),
        "the Error: prefix has to stay: {out}"
    );
    assert!(out.contains("boom"), "{out}");
}

/// Recursive delegation is **structurally** blocked: the subagent's tool table simply has no `delegate_task`.
#[test]
fn the_child_tool_table_has_no_delegate_task() {
    let mut registry = ToolRegistry::new();
    registry.register(delegate_task(Fake::ok("verdict")));
    assert!(registry.names().contains(&NAME), "the parent turn has it");

    let child = registry.without(NAME);
    assert!(
        !child.names().contains(&NAME),
        "it must not be in the subagent's table: {:?}",
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
    registry.register(delegate_task(Fake::ok("verdict")));
    let out = registry.execute(ctx().await, NAME, json!({})).await;
    assert!(out.starts_with("Error:") && out.contains("task"), "{out}");
}

/// `result_schema` is passed through to the runner untouched (the tool layer doesn't interpret it); conclusions come back with the "paraphrase" banner.
#[tokio::test]
async fn a_result_schema_is_passed_through_and_the_report_is_labelled() {
    let runner = Fake::ok("meeting at ten.");
    let tool = delegate_task(runner.clone());
    let schema = json!({"type": "object", "required": ["answer"]});

    let out = match (tool.handler)(
        ctx().await,
        json!({"task": "check it", "result_schema": schema}),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => panic!("must not fail: {e}"),
    };

    assert_eq!(
        runner.seen.lock().expect("lock")[0].schema,
        Some(schema),
        "the schema goes to the runner unchanged"
    );
    assert!(
        out.contains("reported"),
        "a subagent's words have to be marked reported: {out}"
    );
    assert!(out.contains("meeting at ten."), "{out}");
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
        Box::pin(async move { Ok(format!("verdict: {task}")) })
    }
}

#[tokio::test]
async fn a_batch_runs_every_task_and_merges_in_the_order_asked() {
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(Arc::new(Echo)));
    let out = reg
        .execute(
            ctx().await,
            NAME,
            json!({"tasks": ["alpha", "beta", "gamma"]}),
        )
        .await;

    for (index, task) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let header = format!("### Subtask {}: {task}", index + 1);
        assert!(out.contains(&header), "missing {header}: {out}");
    }
    // Order: each conclusion stays under its own heading, so questions and answers cannot slip
    // out of alignment.
    let first = out.find("### Subtask 1: alpha").expect("the first piece");
    let second = out.find("### Subtask 2: beta").expect("the second piece");
    let third = out.find("### Subtask 3: gamma").expect("the third piece");
    assert!(
        first < second && second < third,
        "the order is scrambled: {out}"
    );
    assert!(out.contains("verdict: alpha"), "{out}");
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
    // Two gates: the schema's maxItems stops it first (and the model can see that), and MAX_BATCH
    // in the handler catches whatever gets past.
    assert!(
        out.contains("more than 4") || out.contains("dispatch at most 4"),
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

/// A single delegation gets every tool (no behaviour change); a batch has to tell the
/// implementation "read-only" explicitly.
#[tokio::test]
async fn only_a_batch_hands_the_child_a_readonly_table() {
    let single = Fake::ok("verdict");
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(single.clone()));
    let _ = reg
        .execute(ctx().await, NAME, json!({"task": "check one thing"}))
        .await;
    {
        let seen = single.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        assert!(
            !seen[0].readonly,
            "a single delegation must not be downgraded to read-only"
        );
    }

    let batch = Fake::ok("verdict");
    let mut reg = ToolRegistry::new();
    reg.register(delegate_task(batch.clone()));
    let _ = reg
        .execute(ctx().await, NAME, json!({"tasks": ["alpha", "beta"]}))
        .await;
    let seen = batch.seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter().all(|call| call.readonly),
        "every subagent in a batch has to be read-only: several loops run at once, and writes to the same directory would overwrite each other"
    );
}
