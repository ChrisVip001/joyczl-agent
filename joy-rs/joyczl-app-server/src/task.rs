//! Long tasks: the goal loop's condition with an identity — and a pause button.
//!
//! A goal used to be a string in process memory. Here it becomes a row (`agent_tasks`): named,
//! addressable, suspendable, surviving restarts. Since 0017 the row also stands on its own:
//! it may belong to no session, it carries acceptance conditions, a budget, and what has been
//! spent. The goal loop is still the engine that drives rounds; this module is the ledger and
//! the human's switches.
//!
//! Discipline:
//! * **A task may exist without a session.** "Do this thing" does not require a conversation.
//! * **Every method here is human-only.** The model has no task tool; "the model assigning
//!   itself long work" is not forbidden — it does not exist.
//! * **Suspending keeps the checkpoint.** A suspension is not a stop: the turn's working
//!   memory stays `suspended` in state.db, and `task/resume` continues from it — finished
//!   tools are not re-run. `task/cancel` is the stop, and it abandons the checkpoint.
//! * **Nothing is silent.** Every transition sends `TaskUpdated` (the hub folds it into a ping)
//!   and the reason is stored, not dropped.

use joyczl_protocol::{
    codes, AcceptanceView, AgentTaskView, ArtifactView, BudgetView, CheckView, ErrorObject,
    RunView, ServerNotification, SpentView, TaskActionParams, TaskArtifactsResponse,
    TaskCreateParams, TaskEnvelope, TaskListResponse, TaskRunsResponse, TaskUpdatedNotification,
    TaskViewResponse,
};
use joyczl_state::{TaskRow, TaskStatus};

use crate::{narrow, EventSink, Server};
use joyczl_agent::verify::Acceptance;

/// Parse the stored acceptance conditions for display. Stored JSON was **normalised at create
/// time** (labels filled, kinds validated), so this cannot silently lose a condition.
fn acceptance_of(row: &TaskRow) -> Vec<AcceptanceView> {
    serde_json::from_str(&row.acceptance_json).unwrap_or_default()
}

/// The latest measured verdict per condition.
fn checks_of(row: &TaskRow) -> Vec<CheckView> {
    serde_json::from_str(&row.checks_json).unwrap_or_default()
}

/// The caps, when any were set. `None` = nothing to show (no bar to draw).
fn budget_of(row: &TaskRow) -> Option<BudgetView> {
    let budget: BudgetView = serde_json::from_str(&row.budget_json).unwrap_or_default();
    if budget.wall_ms.is_none() && budget.tokens.is_none() && budget.tools.is_none() {
        return None;
    }
    Some(budget)
}

/// The protocol view of a row — one modelling, shared by every method.
///
/// `async` because the run count is a second read: the card says "executed N times", and that
/// number has to come from the run ledger, not from a guess.
async fn view(server: &Server, row: &TaskRow) -> AgentTaskView {
    let run_count = server
        .runs
        .count_by_task(&row.id)
        .await
        .map(narrow)
        .unwrap_or(0);
    AgentTaskView {
        id: row.id.clone(),
        session_id: row.session_id.clone(),
        title: row.title.clone(),
        goal_condition: row.goal_condition.clone(),
        status: row.status.as_str().to_string(),
        rounds_used: row.rounds_used,
        max_rounds: row.max_rounds,
        tier: row.tier.clone(),
        current_turn_id: row.current_turn_id.clone(),
        suspend_reason: row.suspend_reason.clone(),
        last_stop_reason: row.last_stop_reason.clone(),
        acceptance: acceptance_of(row),
        checks: checks_of(row),
        budget: budget_of(row),
        spent: SpentView {
            wall_ms: narrow(row.spent_wall_ms),
            tokens: narrow(row.spent_tokens),
            tools: narrow(row.spent_tools),
        },
        cwd: row.cwd.clone(),
        run_count,
        created_at: row.created_at.clone(),
        finished_at: row.finished_at.clone(),
    }
}

fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject {
        code: codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

pub(crate) fn notify(sink: &EventSink, row: &TaskRow, reason: Option<&str>) {
    sink.notification(ServerNotification::TaskUpdated(TaskUpdatedNotification {
        task_id: row.id.clone(),
        session_id: row.session_id.clone(),
        status: row.status.as_str().to_string(),
        rounds_used: row.rounds_used,
        reason: reason.map(str::to_string),
    }));
}

/// Read back the row after a write so the caller answers with the authoritative state.
async fn reload(server: &Server, id: &str) -> Result<TaskRow, ErrorObject> {
    server
        .tasks
        .get(id)
        .await
        .map_err(|e| invalid(format!("任务读不回来：{e}")))?
        .ok_or_else(|| invalid("任务不存在"))
}

/// Normalise + validate the acceptance list. Refusing at creation is deliberate: a condition
/// nobody can check would sit on the card looking authoritative while never failing.
fn normalise_acceptance(views: &[AcceptanceView]) -> Result<String, ErrorObject> {
    let mut normalised = Vec::with_capacity(views.len());
    for (index, view) in views.iter().enumerate() {
        let acceptance = Acceptance::from_view(view)
            .map_err(|why| invalid(format!("第 {} 条验收条件不对：{why}", index + 1)))?;
        let mut view = view.clone();
        if view
            .label
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .is_none()
        {
            view.label = Some(acceptance.label());
        }
        // Fill `min` with the default the checker actually uses, so the card and the check agree.
        if view.min.is_none() {
            view.min = Some(1);
        }
        normalised.push(view);
    }
    serde_json::to_string(&normalised).map_err(|e| invalid(format!("验收条件写不成 JSON：{e}")))
}

/// Same for the budget: a zero or negative cap is a typo, not an intention.
fn normalise_budget(budget: &Option<BudgetView>) -> Result<String, ErrorObject> {
    let Some(budget) = budget else {
        return Ok("{}".to_string());
    };
    for (name, value) in [
        ("wallMs", budget.wall_ms),
        ("tokens", budget.tokens),
        ("tools", budget.tools),
    ] {
        if let Some(value) = value {
            if value <= 0 {
                return Err(invalid(format!("预算 {name} 必须大于 0（收到 {value}）")));
            }
        }
    }
    serde_json::to_string(budget).map_err(|e| invalid(format!("预算写不成 JSON：{e}")))
}

pub(crate) async fn create(
    server: &Server,
    params: TaskCreateParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let title = params.title.trim();
    if title.is_empty() {
        return Err(invalid("任务得有个名字"));
    }
    // A session is optional now: a task may be created and inspected on its own. An empty
    // string is the same as absent (`""` is not a session anyone can open).
    let session = params
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let max_rounds = params
        .max_rounds
        .unwrap_or_else(|| server.settings().goal_max_rounds)
        .max(1);
    let goal = params
        .goal_condition
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let tier = crate::route::clean_tier(params.tier.as_deref())?;
    let acceptance_json = normalise_acceptance(&params.acceptance)?;
    let budget_json = normalise_budget(&params.budget)?;
    // The working directory is this task's boundary: verification and writes both stop at it.
    // Fail closed — a typo must not silently become "no boundary".
    let cwd_path = crate::turn::resolve_turn_cwd(&params.cwd)?;
    let cwd_text = cwd_path.as_ref().map(|path| path.display().to_string());
    if let Some(path) = &cwd_path {
        joyczl_config::workspaces::record_workspace(
            &server.settings().home,
            &path.display().to_string(),
        );
    }

    let id = format!("task{}", chrono::Local::now().timestamp_millis());
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: &id,
            session_id: session,
            title,
            goal_condition: goal,
            acceptance_json: &acceptance_json,
            budget_json: &budget_json,
            cwd: cwd_text.as_deref(),
            max_rounds,
            tier: tier.as_deref(),
        })
        .await
        .map_err(|e| invalid(format!("任务没建成：{e}")))?;

    let row = reload(server, &id).await?;
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}

pub(crate) async fn list(server: &Server) -> TaskListResponse {
    let rows = server.tasks.list(100).await.unwrap_or_default();
    let mut tasks = Vec::with_capacity(rows.len());
    for row in &rows {
        tasks.push(view(server, row).await);
    }
    TaskListResponse { tasks }
}

pub(crate) async fn view_of(server: &Server, session_id: &str) -> TaskViewResponse {
    // `latest`, not `active`: a task that just finished is the most informative thing on the
    // screen — its verdicts and artifacts answer "did it actually do it". Open ones still win.
    let task = match server.tasks.latest(session_id).await {
        Ok(Some(row)) => Some(view(server, &row).await),
        Ok(None) => None,
        Err(e) => {
            eprintln!("(joy) 查不到这会话的任务（当没有）：{e}");
            None
        }
    };
    TaskViewResponse { task }
}

/// `task/get`: address one task by id. The only way in for a task with no session.
pub(crate) async fn get(server: &Server, task_id: &str) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, task_id).await?;
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}

/// `task/runs`: every attempt of one task, newest first.
pub(crate) async fn runs(server: &Server, task_id: &str) -> TaskRunsResponse {
    // Reading a missing task's runs is an empty list, not an error: the panel may race a delete.
    let rows = server
        .runs
        .list_by_task(task_id, 50)
        .await
        .unwrap_or_default();
    TaskRunsResponse {
        runs: rows
            .iter()
            .map(|row| RunView {
                id: row.id.clone(),
                status: row.status.clone(),
                rounds: row.rounds,
                tokens_in: narrow(row.tokens_in),
                tokens_out: narrow(row.tokens_out),
                tools_ok: narrow(row.tools_ok),
                tools_failed: narrow(row.tools_failed),
                stop_reason: row.stop_reason.clone(),
                started_at: row.started_at.clone(),
                ended_at: row.ended_at.clone(),
            })
            .collect(),
    }
}

/// `task/artifacts`: the files this task produced.
pub(crate) async fn artifacts(server: &Server, task_id: &str) -> TaskArtifactsResponse {
    let rows = server.artifacts.list(task_id, 50).await.unwrap_or_default();
    TaskArtifactsResponse {
        artifacts: rows
            .iter()
            .map(|row| ArtifactView {
                path: row.path.clone(),
                bytes: narrow(row.bytes),
                at: row.at.clone(),
            })
            .collect(),
    }
}

/// Pause: the running turn is interrupted, but its checkpoint is **kept** (status `suspended`),
/// so resuming continues instead of replaying. The flag tells `finalize_turn` which of the two
/// exits this is — a human pause, not a stop.
pub(crate) async fn suspend(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    if row.status == TaskStatus::Suspended {
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    if !matches!(
        row.status,
        TaskStatus::Running | TaskStatus::Pending | TaskStatus::NeedsAttention
    ) {
        return Err(invalid(format!(
            "任务已经结束了（{}），没什么可暂停的",
            row.status
        )));
    }
    let reason = params
        .reason
        .as_deref()
        .unwrap_or("用户按了暂停")
        .to_string();
    if let Some(turn_id) = &row.current_turn_id {
        // A live turn is paused (checkpoint stays resumable); an idle one just goes suspended.
        if !server.suspend_turn(turn_id) {
            eprintln!("(joy) 任务 {turn_id} 已经没在跑了，直接标挂起");
        }
    }
    server
        .tasks
        .suspend(&row.id, Some(&reason))
        .await
        .map_err(|e| invalid(format!("暂停没记上：{e}")))?;
    let row = reload(server, &row.id).await?;
    notify(sink, &row, Some(&reason));
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}

/// Continue: a suspended (or attention-needing) task goes back to running. If its turn left a
/// `suspended` checkpoint, that turn continues from where it stopped; otherwise the task waits
/// for the next human message to drive it.
pub(crate) async fn resume(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    if row.status == TaskStatus::Running {
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    // `needs_attention` is resumable on purpose: a human looked at the failed condition and
    // says "carry on" — that is exactly the switch this state exists for.
    if !matches!(
        row.status,
        TaskStatus::Suspended | TaskStatus::NeedsAttention
    ) {
        return Err(invalid(format!(
            "只有挂起或需要确认的任务能继续（当前 {}）",
            row.status
        )));
    }
    server
        .tasks
        .resume(&row.id)
        .await
        .map_err(|e| invalid(format!("继续没记上：{e}")))?;
    let row = reload(server, &row.id).await?;
    notify(sink, &row, Some("用户点了继续"));

    // The paused turn's checkpoint is the resumable half: continue it in the background.
    if let Some(turn_id) = &row.current_turn_id {
        crate::resume::resume_suspended(server, sink, turn_id).await;
    }
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}

/// Stop: interrupt whatever is running and close the row as `cancelled`. The checkpoint goes
/// `abandoned` — a cancelled task must never auto-continue on the next boot.
pub(crate) async fn cancel(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    if row.status.is_terminal() {
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    let reason = params.reason.as_deref().unwrap_or("用户取消了任务");
    if let Some(turn_id) = &row.current_turn_id {
        server.interrupt_turn(turn_id);
        if let Err(e) = server.checkpoints.finish(turn_id, "abandoned").await {
            eprintln!("(joy) 取消任务时 checkpoint 没标上（不影响取消）：{e}");
        }
    }
    server
        .tasks
        .finish(&row.id, TaskStatus::Cancelled)
        .await
        .map_err(|e| invalid(format!("取消没记上：{e}")))?;
    // Open runs die with the task; their steps stay as the record of what did happen.
    if let Ok(runs) = server.runs.list_by_task(&row.id, 50).await {
        for run in runs.iter().filter(|run| run.status == "running") {
            let _ = server
                .runs
                .close(&run.id, joyczl_state::RunStatus::Cancelled, Some(reason))
                .await;
        }
    }
    let row = reload(server, &row.id).await?;
    notify(sink, &row, Some(reason));
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}
