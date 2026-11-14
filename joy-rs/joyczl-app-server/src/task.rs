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
pub(crate) async fn view(server: &Server, row: &TaskRow) -> AgentTaskView {
    let run_count = server
        .runs
        .count_by_task(&row.id)
        .await
        .map(narrow)
        .unwrap_or(0);
    let dispatch = match joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
        .get(&row.id)
        .await
    {
        Ok(dispatch) => dispatch,
        Err(error) => {
            eprintln!("(joy) task dispatch view unavailable: {error}");
            None
        }
    };
    let input_condition =
        match joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
            .get(&row.id)
            .await
        {
            Ok(condition) => condition.map(crate::task_inputs::view),
            Err(error) => {
                eprintln!("(joy) task input view unavailable: {error}");
                None
            }
        };
    AgentTaskView {
        input_condition,
        execution_session_id: dispatch.as_ref().map(|d| d.session_id.clone()),
        dispatch_state: dispatch.map(|d| d.state),
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
        .map_err(|e| invalid(format!("the task could not be read back: {e}")))?
        .ok_or_else(|| invalid("no such task"))
}

/// Normalise + validate the acceptance list. Refusing at creation is deliberate: a condition
/// nobody can check would sit on the card looking authoritative while never failing.
fn normalise_acceptance(views: &[AcceptanceView]) -> Result<String, ErrorObject> {
    let mut normalised = Vec::with_capacity(views.len());
    for (index, view) in views.iter().enumerate() {
        let acceptance = Acceptance::from_view(view)
            .map_err(|why| invalid(format!("acceptance check {} is invalid: {why}", index + 1)))?;
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
    serde_json::to_string(&normalised).map_err(|e| {
        invalid(format!(
            "the acceptance criteria could not be written as JSON: {e}"
        ))
    })
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
                return Err(invalid(format!(
                    "budget {name} must be greater than 0 (got {value})"
                )));
            }
        }
    }
    serde_json::to_string(budget)
        .map_err(|e| invalid(format!("the budget could not be written as JSON: {e}")))
}

pub(crate) async fn create(
    server: &Server,
    params: TaskCreateParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let title = params.title.trim();
    if title.is_empty() {
        return Err(invalid("a task needs a name"));
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
        .map_err(|e| invalid(format!("the task was not created: {e}")))?;

    let row = reload(server, &id).await?;
    Ok(TaskEnvelope {
        task: view(server, &row).await,
    })
}

/// Explicit human authorization for one durable initial execution.
pub(crate) async fn start(
    server: &Server,
    sink: &EventSink,
    params: joyczl_protocol::TaskStartParams,
) -> Result<TaskEnvelope, ErrorObject> {
    if server.resolved().is_none() {
        return Err(invalid("Configure a model before starting a task"));
    }
    let lock = server.session_write_lock(&format!("task-control:{}", params.task_id));
    let _guard = lock.lock().await;
    let row = reload(server, &params.task_id).await?;
    joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
        .enqueue(&row.id, &params.message)
        .await
        .map_err(|e| invalid(format!("Task was not queued: {e}")))?;
    notify(sink, &row, Some("queued for independent execution"));
    crate::task_dispatch::kick(server.clone(), sink.clone());
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
            eprintln!("(joy) no task found for this session (treating it as none): {e}");
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
    let control = server.session_write_lock(&format!("task-control:{}", row.id));
    let _control = control.lock().await;
    let row = reload(server, &params.task_id).await?;
    let waits = joyczl_state::waits::JobWaits::new(server.pool.clone());

    let job_wait = waits
        .get(&row.id)
        .await
        .ok()
        .flatten()
        .filter(|wait| wait.state == "waiting");
    let inputs = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone());
    let input_wait = inputs
        .waiting(&row.id)
        .await
        .map_err(|error| invalid(format!("Cannot read input wait: {error}")))?
        .filter(|wait| wait.state == "waiting");
    if row.status == TaskStatus::Suspended && job_wait.is_none() && input_wait.is_none() {
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    if !matches!(
        row.status,
        TaskStatus::Running
            | TaskStatus::Pending
            | TaskStatus::NeedsAttention
            | TaskStatus::Suspended
    ) {
        return Err(invalid(format!(
            "the task has already ended ({}), so there is nothing to pause",
            row.status
        )));
    }
    let reason = params
        .reason
        .as_deref()
        .unwrap_or("paused by the user")
        .to_string();
    if let Some(turn_id) = &row.current_turn_id {
        // A live turn is paused (checkpoint stays resumable); an idle one just goes suspended.
        if !server.suspend_turn(turn_id) {
            eprintln!("(joy) task {turn_id} is no longer running; marking it suspended");
        }
    }
    server
        .tasks
        .suspend(&row.id, Some(&reason))
        .await
        .map_err(|e| invalid(format!("the pause was not recorded: {e}")))?;
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
    // Keep the durable pause until the previous actor has finished its finalizer.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let row = reload(server, &params.task_id).await?;
        if !matches!(
            row.status,
            TaskStatus::Suspended | TaskStatus::NeedsAttention
        ) {
            break;
        }
        let checkpoint = crate::task_recovery::checkpoint(server, &row).await?;
        let turn_id = checkpoint
            .as_ref()
            .map(|cp| cp.turn_id.as_str())
            .or(row.current_turn_id.as_deref());
        let active = turn_id.is_some_and(|id| {
            server
                .active_turns(None)
                .iter()
                .any(|(active, _, _)| active == id)
        });
        if !active {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(invalid(
                "Previous execution is still stopping; continue again after it stops",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    resume_ready(server, sink, params).await
}

async fn resume_ready(
    server: &Server,
    sink: &EventSink,
    params: TaskActionParams,
) -> Result<TaskEnvelope, ErrorObject> {
    let row = reload(server, &params.task_id).await?;
    let control = server.session_write_lock(&format!("task-control:{}", row.id));
    let _control = control.lock().await;
    let row = reload(server, &params.task_id).await?;
    let waits = joyczl_state::waits::JobWaits::new(server.pool.clone());
    let pauses = joyczl_state::human_pauses::HumanPauses::new(server.pool.clone());
    let human_paused = pauses
        .is_paused(&row.id)
        .await
        .map_err(|error| invalid(format!("Cannot verify human pause: {error}")))?;

    if !human_paused
        && row.status == TaskStatus::Suspended
        && row.suspend_reason.as_deref() == Some(joyczl_state::waits::JOB_WAIT_REASON)
    {
        if let Ok(Some(wait)) = waits.get(&row.id).await {
            if wait.state == "waiting" {
                crate::task_waits::arm(server.clone(), sink.clone(), wait);
                return Ok(TaskEnvelope {
                    task: view(server, &row).await,
                });
            }
        }
    }
    if !human_paused
        && row.status == TaskStatus::Suspended
        && row.suspend_reason.as_deref() == Some(joyczl_state::external_waits::INPUT_WAIT_REASON)
    {
        let wait = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
            .waiting(&row.id)
            .await
            .map_err(|error| invalid(format!("Cannot read input wait: {error}")))?
            .filter(|wait| wait.state == "waiting")
            .ok_or_else(|| invalid("Input wait is unavailable; reconcile before continuing"))?;
        crate::input_wake::arm(server.clone(), sink.clone(), wait);
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
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
            "only a suspended task, or one waiting for your call, can resume (currently {})",
            row.status
        )));
    }
    let unstarted = joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
        .resume_unstarted(&row.id)
        .await
        .map_err(|e| invalid(format!("Cannot resume queued task: {e}")))?;
    if unstarted {
        let row = reload(server, &row.id).await?;
        notify(sink, &row, Some("queued task resumed by the user"));
        crate::task_dispatch::kick(server.clone(), sink.clone());
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    let mut checkpoint = crate::task_recovery::checkpoint(server, &row).await?;
    if let Some(checkpoint) = &mut checkpoint {
        crate::task_recovery::require_known(checkpoint)?;
        crate::task_inputs::prepare_manual_resume(server, checkpoint)
            .await
            .map_err(|error| invalid(format!("Cannot prepare continuation: {error}")))?;
        pauses
            .resume_checkpoint(&row.id, &checkpoint.turn_id, &checkpoint.messages_json)
            .await
            .map_err(|error| invalid(format!("Cannot commit human continuation: {error}")))?;
    } else {
        if joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
            .get(&row.id)
            .await
            .map_err(|e| invalid(format!("Cannot read task dispatch: {e}")))?
            .is_some()
        {
            return Err(invalid("Task has no safe resumable checkpoint; reconcile the execution before starting another attempt"));
        }
        server
            .tasks
            .resume(&row.id)
            .await
            .map_err(|e| invalid(format!("The resume was not recorded: {e}")))?;
    }
    let row = reload(server, &row.id).await?;
    notify(sink, &row, Some("resumed by the user"));
    crate::task_dispatch::kick(server.clone(), sink.clone());
    if let Some(checkpoint) = checkpoint {
        crate::resume::resume_authorized(server, sink, checkpoint);
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
    let control = server.session_write_lock(&format!("task-control:{}", row.id));
    let _control = control.lock().await;
    let row = reload(server, &params.task_id).await?;
    let waits = joyczl_state::waits::JobWaits::new(server.pool.clone());

    if row.status.is_terminal() {
        return Ok(TaskEnvelope {
            task: view(server, &row).await,
        });
    }
    let reason = params.reason.as_deref().unwrap_or("cancelled by the user");
    if let Some(turn_id) = &row.current_turn_id {
        server.interrupt_turn(turn_id);
        if let Err(e) = server.checkpoints.finish(turn_id, "abandoned").await {
            eprintln!("(joy) the checkpoint was not marked when cancelling (the cancellation itself is unaffected): {e}");
        }
    }
    server
        .tasks
        .finish(&row.id, TaskStatus::Cancelled)
        .await
        .map_err(|e| invalid(format!("the cancellation was not recorded: {e}")))?;
    let _ = waits.cancel(&row.id).await;
    if let Err(error) = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
        .cancel(&row.id)
        .await
    {
        eprintln!("(joy) cancelled task input wait could not be closed: {error}");
    }

    if let (Some(host), Ok(jobs)) = (
        &server.jobs,
        joyczl_state::Jobs::new(server.pool.clone())
            .for_task(&row.id)
            .await,
    ) {
        for job in jobs.iter().filter(|job| job.status == "running") {
            if let Err(e) = host.kill(&job.owner, &job.id).await {
                eprintln!("(joy) task job {} could not be stopped: {e}", job.id);
            }
        }
    }
    // Open runs die with the task; their steps stay as the record of what did happen.
    if let Ok(runs) = server.runs.list_by_task(&row.id, 50).await {
        for run in runs
            .iter()
            .filter(|run| matches!(run.status.as_str(), "running" | "waiting" | "suspended"))
        {
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
