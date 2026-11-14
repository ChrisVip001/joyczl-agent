//! Triage stage of the turn lifecycle.

use super::{full_turn, load_soul, thin_soul_for, FullTurn, HistoryStats, TurnFrame};
use crate::{narrow, EventSink, Server};
use joyczl_graph::workflows::triage::{
    build_triage_graph, classify_message, todays_events, CalendarFn, ClassifyFn, FullFn, QuickFn,
    QUICK_REPLY_PROMPT,
};
use joyczl_graph::{
    run_graph, GraphEvent, NodeCtx, NodeWrites, Observer, State, DEFAULT_MAX_STEPS,
};
use joyczl_loop::LoopResult;
use joyczl_protocol::{
    codes, ErrorObject, GraphEndedNotification, GraphInfo, GraphNodeEndedNotification,
    GraphNodeStartedNotification, GraphRouteKind, GraphStartedNotification, ServerNotification,
};
use joyczl_provider::{CreateRequest, Message, Resolved, Usage};
use std::sync::{Arc, Mutex};

/// What one trip through the graph's front door yields: how this turn ran, and the answer itself.
pub(super) struct Routed {
    pub(super) info: GraphInfo,
    pub(super) turn: FullTurn,
}

/// The graph's front door.
///
/// Flag off → `Ok(None)`, the caller takes the original path; flag on but the graph yields no answer
/// (engine error, classifier broken, node throws) → also `Ok(None)`, falling back to the ordinary full loop.
/// The cost of a "broken graph" must be latency, never capability.
///
/// The sole exception is the loop itself failing: that error is returned as-is, **no second retry** —
/// a retry would just fail the same way again, wasting double the time.
pub(super) async fn graph_route(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    frame: TurnFrame<'_>,
) -> Result<Option<Routed>, ErrorObject> {
    let TurnFrame {
        session_id,
        turn_id,
        sink,
        interrupt,
        turn_cwd,
        route_reason,
        // Resumed turns bypass the graph entirely (full_turn directly), so the frame's
        // resume slot never reaches here.
        resume: _,
        checkpoint,
        steering,
    } = frame;
    if !server.settings().graph_workflows {
        return Ok(None);
    }

    // Task execution always uses the durable full loop; quick chat does not create a
    // working-memory checkpoint or load task context.
    match server.runs.for_turn(turn_id).await {
        Ok(Some(_)) => return Ok(None),
        Ok(None) => {}
        Err(e) => {
            return Err(ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("Cannot resolve task route: {e}"),
                data: None,
            })
        }
    }

    // State only holds JSON, and a loop turn's product (`LoopResult`) is not JSON —
    // so full_agent's **real** result comes back through this side channel; what goes into state is
    // just "I ran".
    let slot: Arc<Mutex<Option<Result<FullTurn, ErrorObject>>>> = Arc::new(Mutex::new(None));

    let classify: ClassifyFn = {
        let client = resolved.client.clone();
        let model = resolved.small_model.clone();
        Arc::new(move |message: String| {
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move { Ok(classify_message(client.as_ref(), &model, &message).await) })
        })
    };

    let calendar: CalendarFn = {
        let home = server.settings().home.clone();
        Arc::new(move || todays_events(&home))
    };

    let quick: QuickFn = {
        let client = resolved.client.clone();
        let model = resolved.small_model.clone();
        let server = server.clone();
        let session_id = session_id.to_string();
        let soul = load_soul(
            &server.settings().home,
            thin_soul_for(&server.settings().soul_mode, &resolved.provider_id),
        );
        Arc::new(move |state: State| {
            let server = server.clone();
            let session_id = session_id.clone();
            let soul = soul.clone();
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move {
                let prompt = QUICK_REPLY_PROMPT
                    .replace("{calendar}", state.str("calendar").unwrap_or(""))
                    .replace("{message}", state.str("message").unwrap_or(""));
                let mut history = Vec::new();
                match server.chat.messages(&session_id, None, 12).await {
                    Ok(rows) => {
                        let mut remaining = 8_000usize;
                        // Keep newest context within the cap, then restore chronological order.
                        for row in rows {
                            let text: String =
                                row.content.chars().take(remaining.min(2_000)).collect();
                            remaining = remaining.saturating_sub(text.chars().count());
                            if row.role == "user" {
                                history.push(Message::user_text(text));
                            } else if row.role == "assistant" {
                                history.push(Message {
                                    role: joyczl_provider::Role::Assistant,
                                    content: vec![joyczl_provider::ContentBlock::Text { text }],
                                });
                            }
                            if remaining == 0 {
                                break;
                            }
                        }
                        history.reverse();
                    }
                    Err(e) => eprintln!("(joy) quick reply history unavailable: {e}"),
                }
                match crate::quick_context::personal(&server.facts).await {
                    Ok(Some(context)) => history.insert(0, Message::user_text(context)),
                    Ok(None) => {}
                    Err(error) => eprintln!("(joy) quick personal context unavailable: {error}"),
                }
                history.push(Message::user_text(prompt));
                let operation_context = crate::turn::operations::context(&server, &session_id)
                    .await
                    .map_err(|error| error.message)?;
                let soul = format!(
                    "{}\n{}\n{}{}",
                    soul,
                    super::prompt::CONVERSATION_RULES,
                    crate::task::dispatch::projection(&server, &session_id).await,
                    operation_context.prompt,
                );
                let response = client
                    .create(CreateRequest {
                        cache_prefix: None,
                        model,
                        system: Some(soul),
                        messages: history,
                        tools: Vec::new(),
                        max_tokens: 600,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(response.text())
            })
        })
    };

    let full: FullFn = {
        let server = server.clone();
        let resolved = resolved.clone();
        let sink = sink.clone();
        let (turn_id, session_id) = (turn_id.to_string(), session_id.to_string());
        let turn_cwd = turn_cwd.clone();
        let route_reason = route_reason.clone();
        // The graph's inner loop checkpoints too: its tool rounds are this turn's progress
        // just the same (checkpoint is None only for quick answers, which run no loop).
        let checkpoint = checkpoint.clone();
        let slot = slot.clone();
        Arc::new(move |ctx: NodeCtx| {
            // FullFn is an Fn (the engine may call it multiple times): only clone here; never move the captured values.
            let turn_cwd = turn_cwd.clone();
            let route_reason = route_reason.clone();
            let checkpoint = checkpoint.clone();
            let (server, resolved, sink) = (server.clone(), resolved.clone(), sink.clone());
            let (turn_id, session_id, slot) = (turn_id.clone(), session_id.clone(), slot.clone());
            let interrupt = interrupt.clone();
            let steering = steering.clone();
            Box::pin(async move {
                // Whatever the user said this turn is on the whiteboard — the initial state run_graph receives.
                let message = ctx.state.str("message").unwrap_or_default().to_string();
                // The loop's events go out through the node outlet; the engine attaches node= before forwarding.
                let inner = Some(ctx.inner.clone());
                let outcome = full_turn(
                    &server,
                    &resolved,
                    &message,
                    // Graph-routed turns (triage / gather) are engine-driven continuations:
                    // images already rode on the initial user message.
                    Vec::new(),
                    TurnFrame {
                        session_id: &session_id,
                        turn_id: &turn_id,
                        sink: &sink,
                        interrupt,
                        turn_cwd,
                        route_reason,
                        resume: None,
                        checkpoint,
                        steering,
                    },
                    inner,
                )
                .await;
                // Record first, then decide what to return to the engine: the error only leaves a trace on the
                // engine side; the truth goes out through the side channel (its ErrorObject carries the code).
                let why = outcome.as_ref().err().map(|error| error.message.clone());
                *slot
                    .lock()
                    .expect("the side-channel lock cannot be poisoned") = Some(outcome);
                match why {
                    Some(why) => Err(why),
                    None => Ok(NodeWrites::new()),
                }
            })
        })
    };

    // The graph is hard-coded next to this file; reaching Err means we wrote it wrong — this turn still
    // takes the ordinary path; don't gamble the user's turn on a graph that cannot be built.
    let graph = match build_triage_graph(classify, calendar, quick, full) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("(joy) the triage graph did not assemble; taking the plain path — {error}");
            return Ok(None);
        }
    };

    let observer: Observer = {
        let sink = sink.clone();
        Arc::new(move |event| graph_event(&sink, event))
    };

    let mut state = State::new();
    state.set("message", serde_json::json!(message));

    let report = match run_graph(graph, state, Some(observer), DEFAULT_MAX_STEPS).await {
        // A key collision is a graph bug (parallel nodes wrote the same key); reporting beats silently dropping a write.
        Err(error) => {
            eprintln!("(joy) the triage graph failed; taking the plain path — {error}");
            return Ok(None);
        }
        Ok(report) => report,
    };

    let reason = report
        .state
        .str("triage_reason")
        .unwrap_or_default()
        .to_string();
    let info = |route: GraphRouteKind| GraphInfo {
        workflow: "triage".to_string(),
        route,
        reason: reason.clone(),
        path: report.path.clone(),
    };

    // Whether full_agent ran, check the side channel; if it ran and failed, report that failure honestly.
    // Release the lock before dispatching (this is the function's tail expression; the guard cannot live that long).
    let landed = slot
        .lock()
        .expect("the side-channel lock cannot be poisoned")
        .take();
    match landed {
        Some(Err(error)) => Err(error),
        Some(Ok(turn)) => Ok(Some(Routed {
            info: info(GraphRouteKind::Full),
            turn,
        })),
        // The quick answer wrote its reply directly on the whiteboard.
        None => match report.state.str("reply").filter(|reply| !reply.is_empty()) {
            Some(reply) => Ok(Some(Routed {
                info: info(GraphRouteKind::Quick),
                turn: FullTurn {
                    // The graph's quick path never loaded history: nothing to report.
                    history: HistoryStats::default(),
                    // One model call, no tools — that is its entirety.
                    result: LoopResult {
                        stop_reason: joyczl_provider::StopReason::EndTurn,
                        round_id: format!("{turn_id}:quick"),
                        reply: reply.to_string(),
                        tool_calls: Vec::new(),
                        iterations: 1,
                        usage: Usage::default(),
                        messages: Vec::new(),
                        interrupted: false,
                        guard: joyczl_loop::guard::GuardReport::default(),
                        // The graph finished on its own (loop not used); there is no local estimate to pair with.
                        estimated_input_tokens: 0,
                        observed_input_tokens: 0,
                    },
                    gate: None,
                },
            })),
            // The graph produced nothing — the caller falls back to the ordinary full loop.
            None => Ok(None),
        },
    }
}

/// Translate graph events into protocol notifications.
///
/// Two event kinds are **not** emitted here:
///   - `Inner`: the loop layer already emitted them (text deltas, tool completions). Emitting again,
///     the client would see them twice.
///   - `Route`: which path was taken lands in the turn's meta at the end (`TurnMeta.graph`);
///     a standalone "picked a side" notification carries no information for the user.
pub(super) fn graph_event(sink: &EventSink, event: GraphEvent) {
    match event {
        GraphEvent::Started { workflow, nodes } => {
            sink.notification(ServerNotification::GraphStarted(GraphStartedNotification {
                workflow,
                nodes,
            }));
        }
        GraphEvent::NodeStarted {
            workflow,
            node,
            visit,
        } => {
            sink.notification(ServerNotification::GraphNodeStarted(
                GraphNodeStartedNotification {
                    workflow,
                    node,
                    visit,
                },
            ));
        }
        GraphEvent::NodeEnded {
            workflow,
            node,
            ms,
            keys,
            error,
        } => {
            sink.notification(ServerNotification::GraphNodeEnded(
                GraphNodeEndedNotification {
                    workflow,
                    node,
                    ms: narrow(ms),
                    keys,
                    error,
                },
            ));
        }
        GraphEvent::Ended {
            workflow,
            ms,
            steps,
            path,
            error,
        } => {
            sink.notification(ServerNotification::GraphEnded(GraphEndedNotification {
                workflow,
                ms: narrow(ms),
                steps,
                path,
                error,
            }));
        }
        GraphEvent::Route { .. } | GraphEvent::Inner { .. } => {}
    }
}
