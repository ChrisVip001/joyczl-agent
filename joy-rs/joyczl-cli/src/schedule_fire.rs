//! One claimed schedule execution; output delivery never reruns the model turn.
use super::{append_run_log, run_log_line, Delivery, Job};
use anyhow::Result;
use chrono::NaiveDateTime;
use joyczl_app_server::{run_turn, EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_state::schedule_completions::{ScheduleCompletion, ScheduleCompletions};

pub(super) async fn fire(server: &Server, job: &Job, at: NaiveDateTime) -> Result<()> {
    let minute = at.format("%Y-%m-%dT%H:%M").to_string();
    let home = server.settings().home;
    let schedules = joyczl_state::Schedules::new(server.state_pool());
    let delivery = match job.delivery {
        Delivery::Outbox => "outbox",
        Delivery::Notify => "notify",
        Delivery::Quiet => "quiet",
    };
    schedules.ensure(&job.name, job.catch_up, delivery).await?;
    if !schedules.claim_once(&job.name, &minute).await? {
        eprintln!(
            "(joy) schedule '{}' trigger retained or already handled; no new turn submitted",
            job.name
        );
        return Ok(());
    }
    let notifier = server.notifier();
    let target = notifier
        .as_ref()
        .and_then(|notifier| notifier.delivery_target());
    let notice = job.delivery == Delivery::Notify && notifier.is_some();
    let rows = ScheduleCompletions::new(server.state_pool());
    let base = ScheduleCompletion {
        name: &job.name,
        minute: &minute,
        status: "produced",
        detail: "",
        output: "",
        preview: "",
        output_path: None,
        notify_requested: notice,
        notify_target: target.as_deref(),
    };
    eprintln!("(joy) fired job '{}' ({})", job.name, job.cron);
    let (sink, mut receiver) = EventSink::channel();
    let host = server.clone();
    let params = TurnStartParams {
        session_id: Some(format!("schedule:{}", job.name)),
        message: job.prompt.clone(),
        stream: Some(true),
        ..Default::default()
    };
    let task =
        tokio::spawn(async move { run_turn(&host, params, RequestId::Number(0), &sink).await });
    let mut reply = String::new();
    let mut review = false;
    while let Some(frame) = receiver.recv().await {
        match frame {
            Frame::Notification(ServerNotification::TurnSuspended(_)) => review = true,
            Frame::Notification(ServerNotification::TurnCompleted(done)) => {
                review |= done.meta.interrupted
                    || done
                        .meta
                        .goal_status
                        .as_deref()
                        .is_some_and(|status| !matches!(status, "satisfied" | "verified"));
                reply = done.reply;
                break;
            }
            _ => {}
        }
    }
    let error = match task.await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("Turn error: {}", error.message)),
        Err(error) => Some(format!("Turn task failed: {error}")),
    };
    if error.is_some() || review {
        let reason = error.unwrap_or_else(|| {
            "Turn stopped without a confirmed outcome; reconcile external effects".into()
        });
        rows.settle(ScheduleCompletion {
            status: "needs_attention",
            detail: &reason,
            output: &reply,
            preview: &reason,
            ..base
        })
        .await?;
        server.flush_completion_notices().await;
        append_run_log(
            &home,
            &run_log_line(
                &job.name,
                &minute,
                "needs_attention",
                &reason,
                reply.chars().count(),
                false,
            ),
        );
        anyhow::bail!("Schedule '{}' requires review: {reason}", job.name);
    }
    let trimmed = reply.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("no_reply") {
        rows.settle(ScheduleCompletion {
            status: "empty",
            notify_requested: false,
            ..base
        })
        .await?;
        append_run_log(
            &home,
            &run_log_line(&job.name, &minute, "empty", "", 0, false),
        );
        return Ok(());
    }
    let file_name = (job.delivery != Delivery::Quiet)
        .then(|| format!("schedule-{}-{}.md", job.name, at.format("%Y%m%d-%H%M")));
    let body = if file_name.is_some() {
        format!("# {}\n\n_fired by joy schedule at {} on cron '{}' _\n\n## Request\n\n{}\n\n## Answer\n\n{}\n",job.name,at.format("%Y-%m-%d %H:%M"),job.cron,job.prompt,reply)
    } else {
        String::new()
    };
    rows.settle(ScheduleCompletion {
        output: &body,
        preview: trimmed,
        output_path: file_name.as_deref(),
        ..base
    })
    .await?;
    server.flush_completion_notices().await;
    let delivered = rows.output_delivered(&job.name, &minute).await?;
    if let Some(file_name) = file_name {
        eprintln!(
            "(joy) schedule '{}' output committed; file delivery {} at {}",
            job.name,
            if delivered { "complete" } else { "pending" },
            home.join("outbox").join(file_name).display()
        );
    } else {
        eprintln!("(joy) schedule '{}' output committed (quiet)", job.name);
    }
    append_run_log(
        &home,
        &run_log_line(
            &job.name,
            &minute,
            "ok",
            "",
            trimmed.chars().count(),
            delivered,
        ),
    );
    Ok(())
}
