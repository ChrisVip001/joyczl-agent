//! Persisted execution-plan loading and bounded revision.

use crate::Server;
use joyczl_provider::Resolved;
use joyczl_state::TaskRow;

pub(super) async fn load_plan(
    server: &Server,
    resolved: &Resolved,
    task: Option<&TaskRow>,
    acceptance: &[joyczl_agent::verify::Acceptance],
) -> Option<joyczl_agent::plan::Plan> {
    let mut plan = None;
    if let Some(row) = task {
        match joyczl_agent::plan::restore(&server.steps, &row.id).await {
            Ok(saved) => {
                plan = saved;
                if plan.is_none() && server.settings().plan_enabled {
                    let labels: Vec<String> = acceptance.iter().map(|item| item.label()).collect();
                    match joyczl_agent::plan::make(
                        resolved,
                        &row.title,
                        row.goal_condition.as_deref(),
                        &labels,
                        None,
                        1,
                    )
                    .await
                    {
                        Ok(made) => {
                            match joyczl_agent::plan::record(&server.steps, &row.id, &made).await {
                                Ok(()) => plan = Some(made),
                                Err(why) => eprintln!(
                                    "(joy) plan not committed; continuing without it: {why}"
                                ),
                            }
                        }
                        Err(why) => {
                            eprintln!("(joy) no plan this round (the turn runs anyway): {why}")
                        }
                    }
                }
            }
            Err(why) => eprintln!("(joy) saved plan unavailable; no replacement generated: {why}"),
        }
    }

    plan
}

pub(super) async fn revise_plan(
    server: &Server,
    resolved: &Resolved,
    task: Option<&TaskRow>,
    acceptance: &[joyczl_agent::verify::Acceptance],
    mut plan: Option<joyczl_agent::plan::Plan>,
    reason: &str,
) -> Option<joyczl_agent::plan::Plan> {
    let task_id = task.map(|row| row.id.as_str());
    if let (Some(id), Some(current)) = (task_id, plan.as_ref()) {
        if server.settings().plan_enabled && current.version < joyczl_agent::plan::MAX_VERSIONS {
            let labels: Vec<String> = acceptance.iter().map(|item| item.label()).collect();
            let title = task
                .as_ref()
                .map(|row| row.title.as_str())
                .unwrap_or("this task");
            let goal = task.as_ref().and_then(|row| row.goal_condition.as_deref());
            match joyczl_agent::plan::make(
                resolved,
                title,
                goal,
                &labels,
                Some(reason),
                current.version + 1,
            )
            .await
            {
                Ok(revised) => {
                    match joyczl_agent::plan::record(&server.steps, id, &revised).await {
                        Ok(()) => {
                            eprintln!("(joy) task {id} moved to plan v{}", revised.version);
                            plan = Some(revised);
                        }
                        Err(why) => eprintln!(
                            "(joy) revised plan not committed; keeping the previous version: {why}"
                        ),
                    }
                }
                Err(why) => {
                    eprintln!("(joy) re-planning failed (continuing with the old plan): {why}")
                }
            }
        }
    }
    plan
}
