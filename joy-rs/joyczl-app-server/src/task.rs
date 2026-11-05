//! Long tasks: the goal loop's condition with an identity — and a pause button.
//!
//! A goal used to be a string in process memory. Here it becomes a row (`agent_tasks`,
//! migration 0014): named, addressable, suspendable, surviving restarts. The goal loop is
//! still the engine that drives rounds; this module is the ledger and the human's switches.
//!
//! Discipline:
//! * **One active task per session.** A second `task/create` is refused with the existing
//!   task named — the human should see what they already started, not a stack of ghosts.
//! * **Every method here is human-only.** The model has no task tool; "the model assigning
//!   itself long work" is not forbidden — it does not exist.
//! * **Suspending keeps the checkpoint.** A suspension is not a stop: the turn's working
//!   memory stays `suspended` in state.db, and `task/resume` continues from it — finished
//!   tools are not re-run. `task/cancel` is the stop, and it abandons the checkpoint.
//! * **Nothing is silent.** Every transition sends `TaskUpdated` (the hub folds it into a ping)
//!   and the reason is stored, not dropped.

use joyczl_protocol::{
    codes, AgentTaskView, ErrorObject, ServerNotification, TaskActionParams, TaskCreateParams,
    TaskEnvelope, TaskListResponse, TaskUpdatedNotification, TaskViewResponse,
};
use joyczl_state::TaskRow;

use crate::{EventSink, Server};

/// The protocol view of a row — one modelling, shared by every method.
fn view(row: &TaskRow) -> AgentTaskView {
    AgentTaskView {
        id: row.id.clone(),
        session_id: row.session_id.clone(),
        title: row.title.clone(),
        goal_condition: row.goal_condition.clone(),
        status: row.status.clone(),
        rounds_used: row.rounds_used,
        max_rounds: row.max_rounds,
        current_turn_id: row.current_turn_id.clone(),
        suspend_reason: row.suspend_reason.clone(),
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
        status: row.status.clone(),
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

pub(crate) async fn create(
    server: &Server,
    params: TaskCreateParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let session = params.session_id.trim();
    if session.is_empty() {
        return Err(invalid("任务要挂在某个会话上（sessionId 不能为空）"));
    }
    let title = params.title.trim();
    if title.is_empty() {
        return Err(invalid("任务得有个名字"));
    }
    if let Some(existing) = server
        .tasks
        .active(session)
        .await
        .map_err(|e| invalid(format!("查不了活跃任务：{e}")))?
    {
        return Err(invalid(format!(
            "这个会话已经有一个未结束的任务「{}」（{}）——先结束它再开新的",
            existing.title, existing.status
        )));
    }
    let max_rounds = params
        .max_rounds
        .unwrap_or_else(|| server.settings().goal_max_rounds)
        .max(1);
    let goal = params
        .goal_condition
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let id = format!("task{}", chrono::Local::now().timestamp_millis());
    server
        .tasks
        .create(&id, session, title, goal, max_rounds)
        .await
        .map_err(|e| invalid(format!("任务没建成：{e}")))?;
    // The goal loop reads its condition from the in-memory mirror; a task created with one
    // starts driving rounds on the **next** turn of this session.
    if let Some(condition) = goal {
        crate::goal::set(&server.goals, session, Some(condition));
    }
    let row = reload(server, &id).await?;
    Ok(TaskEnvelope { task: view(&row) })
}

pub(crate) async fn list(server: &Server) -> TaskListResponse {
    let tasks = server
        .tasks
        .list(100)
        .await
        .unwrap_or_default()
        .iter()
        .map(view)
        .collect();
    TaskListResponse { tasks }
}

pub(crate) async fn view_of(server: &Server, session_id: &str) -> TaskViewResponse {
    let task = server
        .tasks
        .active(session_id)
        .await
        .ok()
        .flatten()
        .map(|row| view(&row));
    TaskViewResponse { task }
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
    if row.status == "suspended" {
        return Ok(TaskEnvelope { task: view(&row) });
    }
    if row.status != "running" && row.status != "pending" {
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
    Ok(TaskEnvelope { task: view(&row) })
}

/// Continue: a suspended task goes back to running. If its turn left a `suspended` checkpoint,
/// that turn continues from where it stopped (finished tools are not re-run); otherwise the
/// task waits for the next human message to drive it.
pub(crate) async fn resume(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    if row.status == "running" {
        return Ok(TaskEnvelope { task: view(&row) });
    }
    if row.status != "suspended" {
        return Err(invalid(format!(
            "只有挂起的任务能继续（当前 {}）",
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
    Ok(TaskEnvelope { task: view(&row) })
}

/// Stop: interrupt whatever is running and close the row as `cancelled`. The checkpoint goes
/// `abandoned` — a cancelled task must never auto-continue on the next boot.
pub(crate) async fn cancel(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    if row.status == "done" || row.status == "failed" || row.status == "cancelled" {
        return Ok(TaskEnvelope { task: view(&row) });
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
        .finish(&row.id, "cancelled")
        .await
        .map_err(|e| invalid(format!("取消没记上：{e}")))?;
    // The goal loop must stop too: the condition lived on this task.
    crate::goal::set(&server.goals, &row.session_id, None);
    let row = reload(server, &row.id).await?;
    notify(sink, &row, Some(reason));
    Ok(TaskEnvelope { task: view(&row) })
}
