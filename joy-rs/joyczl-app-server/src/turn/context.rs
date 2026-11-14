//! Context stage of the turn lifecycle.

use crate::{EventSink, Server};
use joyczl_loop::Steering;
use joyczl_protocol::{
    codes, ErrorObject, GateDecidedNotification, GateDecision, GateDecisionKind, ServerNotification,
};
use joyczl_provider::Resolved;
use std::sync::{Arc, Mutex};

/// Positioning info for one turn: session, turn, event outlet, cancellation token, working directory.
/// The same reason as Injections for bundling into a struct: they **always change together**,
/// and clippy stops at the 8th parameter.
#[derive(Clone)]
pub(crate) struct TurnFrame<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) turn_id: &'a str,
    pub(crate) sink: &'a EventSink,
    pub(crate) interrupt: Option<Arc<joyczl_loop::Interrupt>>,
    pub(crate) turn_cwd: Option<std::path::PathBuf>,
    /// This turn's route decision reason (route.rs); None = no selection happened.
    pub(crate) route_reason: Option<String>,
    /// Resume from a checkpoint instead of assembling history (crash recovery).
    pub(crate) resume: Option<joyczl_loop::Resume>,
    /// The checkpoint outlet: every tool-results boundary lands here (None = no checkpointing).
    pub(crate) checkpoint: Option<joyczl_loop::CheckpointSink>,
    /// Additions accepted while this turn runs (`turn/steer`). Same channel for every round of
    /// the turn, including stop-hook continuations — a message added during round 2 is still
    /// "about this turn".
    pub(crate) steering: Option<Arc<Mutex<Steering>>>,
}

/// The working directory the session remembers (migration 0018). `None` = it never chose one, or
/// it chose one that has since disappeared — a vanished directory must not silently become "the
/// process cwd": that would turn a missing workspace into writes landing somewhere unexpected.
pub(crate) async fn session_cwd(server: &Server, session_id: &str) -> Option<std::path::PathBuf> {
    let row = server.chat.session(session_id).await.ok().flatten()?;
    let cwd = row.cwd?;
    let path = std::path::PathBuf::from(&cwd);
    if !path.is_dir() {
        eprintln!("(joy) the working directory {cwd} recorded for session {session_id} is gone; this turn has no writable scope");
        return None;
    }
    Some(path)
}

/// Admission check for this turn's working directory (fail-closed): when given, it must be an existing
/// absolute path — silent fallback turns "I thought I was working in dir A" into the hardest bug to trace.
/// An empty string counts as not given.
pub(crate) fn resolve_turn_cwd(
    cwd: &Option<String>,
) -> Result<Option<std::path::PathBuf>, ErrorObject> {
    let Some(text) = cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(text);
    if !path.is_absolute() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("cwd '{text}' must be an absolute path"),
            data: None,
        });
    }
    if !path.is_dir() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("cwd '{text}' does not exist or is not a directory"),
            data: None,
        });
    }
    Ok(Some(path))
}

/// The longest a pre-loop step (memory gate, retrieval, compaction) may take before Joy
/// proceeds without it. Cancellation is the real protection — this only keeps a hung
/// provider from holding the turn hostage.
pub(super) const PRELOOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Race a pre-loop step against the user's stop.
///
/// Gate, retrieval and compaction each make a model call *before* the loop starts, and the
/// loop's own `select!` cannot help there: "stop" used to wait behind a summary. `None` means
/// the turn was stopped, and the caller proceeds without that step instead of blocking —
/// the loop sees the same cancellation on its first iteration and answers honestly.
pub(crate) async fn interruptible<T>(
    interrupt: Option<&Arc<joyczl_loop::Interrupt>>,
    what: &str,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match interrupt {
        Some(cancel) => tokio::select! {
            _ = cancel.wait() => {
                eprintln!("(joy) interrupted: no longer waiting for {what}");
                None
            }
            value = future => Some(value),
        },
        None => Some(future.await),
    }
}

pub(super) async fn retrieve_memory(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    settings: &joyczl_config::Settings,
    frame: &TurnFrame<'_>,
    embedder: Option<&joyczl_provider::embed::Embedder>,
) -> (GateDecision, String) {
    let interrupt = &frame.interrupt;
    let sink = frame.sink;
    let turn_id = frame.turn_id;
    match &frame.resume {
        Some(_) => (
            GateDecision {
                decision: GateDecisionKind::Skip,
                reason: "resumed from checkpoint — a resume does not pass the gate again"
                    .to_string(),
                query: None,
            },
            String::new(),
        ),
        None => {
            // The gate is a small-model call; a stop must not wait behind it. Stopped before
            // the answer: skip retrieval and let the loop report the interruption.
            let quick = interruptible(
                interrupt.as_ref(),
                "memory gate",
                tokio::time::timeout(
                    PRELOOP_TIMEOUT,
                    joyczl_memory::gate::should_retrieve(
                        resolved.client.as_ref(),
                        &resolved.small_model,
                        message,
                    ),
                ),
            )
            .await;
            match quick {
                Some(quick) => {
                    let decision = match quick {
                        Ok(decision) => decision,
                        Err(_) => joyczl_memory::gate::Decision::fail_open(
                            format!(
                                "the memory gate did not answer within {}s; failing open",
                                PRELOOP_TIMEOUT.as_secs()
                            ),
                            message,
                        ),
                    };
                    let gate_decision = GateDecision {
                        decision: if decision.retrieve {
                            GateDecisionKind::Retrieve
                        } else {
                            GateDecisionKind::Skip
                        },
                        reason: decision.reason.clone(),
                        query: decision.retrieve.then(|| decision.query.clone()),
                    };
                    sink.notification(ServerNotification::GateDecided(GateDecidedNotification {
                        turn_id: turn_id.to_string(),
                        decision: gate_decision.clone(),
                    }));
                    let memory_context = if decision.retrieve {
                        // Retrieval is the expensive half: stopped mid-way, the turn continues
                        // with no memory and the loop answers "interrupted" on its first round.
                        interruptible(
                            interrupt.as_ref(),
                            "memory retrieval",
                            tokio::time::timeout(
                                PRELOOP_TIMEOUT,
                                joyczl_memory::retrieve_context(
                                    &server.facts,
                                    &server.episodes,
                                    &decision.query,
                                    settings.retrieval_top_k.max(1) as u32,
                                    embedder,
                                ),
                            ),
                        )
                        .await
                        .and_then(|inner| inner.ok())
                        .and_then(|text| text.ok())
                        .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    (gate_decision, memory_context)
                }
                // Stopped before the gate answered: no retrieval, no more model calls. The
                // loop checks the same token on its first iteration and returns `interrupted`.
                None => (
                    GateDecision {
                        decision: GateDecisionKind::Skip,
                        reason: "interrupted: skipped the memory gate and retrieval".to_string(),
                        query: None,
                    },
                    String::new(),
                ),
            }
        }
    }
}
