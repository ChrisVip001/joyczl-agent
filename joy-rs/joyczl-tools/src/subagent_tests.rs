//! 子代理工具的契约：结论怎么回来、失败怎么说、递归派生怎么被结构性地挡住。

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::subagent::{delegate_task, SubagentRunner, NAME};
use super::{BoxFut, ToolRegistry};

/// 一次假委派：记下收到的任务，返回写死的答案。
struct Fake {
    answer: Result<String, String>,
    seen: Mutex<Vec<(String, Option<i32>)>>,
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
    fn run(&self, task: String, max_iterations: Option<i32>) -> BoxFut {
        self.seen.lock().expect("锁").push((task, max_iterations));
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
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    super::ToolCtx {
        approval: None,
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
    }
}

/// 结论回来时要带个标签：父轮看到的是工具输出，得知道这是「别人说的」。
#[tokio::test]
async fn a_delegation_returns_the_summary_with_a_label() {
    let runner = Fake::ok("十点有会。");
    let tool = delegate_task(runner.clone());
    let out = match (tool.handler)(ctx().await, json!({"task": "查今天的安排"})).await {
        Ok(out) => out,
        Err(e) => panic!("不该失败：{e}"),
    };

    assert!(out.contains("子代理回话了"), "{out}");
    assert!(out.contains("十点有会。"), "{out}");
    let seen = runner.seen.lock().expect("锁");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "查今天的安排");
    assert_eq!(
        seen[0].1, None,
        "没给 max_iterations 就是 None（由 runner 定默认）"
    );
}

/// 委派失败是**文本**，不是错误 —— 与这一层所有工具一致。
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

/// 递归派生是**结构性**挡住的：子代理拿到的工具表里根本没有 `delegate_task`。
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
    // 父轮那份不受影响（`without` 是复制，不是就地删）。
    assert!(registry.names().contains(&NAME));
    assert!(child.names().iter().all(|name| *name != NAME));
}

/// 任务描述是必填的 —— 缺了它 schema 会先挡下（模型据此能改对）。
#[tokio::test]
async fn the_task_is_required() {
    let mut registry = ToolRegistry::new();
    registry.register(delegate_task(Fake::ok("结论")));
    let out = registry.execute(ctx().await, NAME, json!({})).await;
    assert!(out.starts_with("Error:") && out.contains("task"), "{out}");
}
