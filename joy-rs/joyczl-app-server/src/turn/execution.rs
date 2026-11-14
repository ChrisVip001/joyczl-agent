//! Execution stage of the turn lifecycle.

use std::sync::Arc;

use super::{
    budget_tokens, build_system, capability_note, interruptible, is_context_overflow, load_history,
    load_soul, retrieve_memory, thin_soul_for, HistoryStats, Loaded, PromptParts, TurnFrame,
    FORCED_RETRY_TURNS, PRELOOP_TIMEOUT, SUMMARY_RESERVE_TOKENS,
};
use crate::Server;
use joyczl_loop::{LoopEvent, LoopResult};
use joyczl_protocol::{
    codes, ErrorObject, GateDecision, ServerNotification, TextDeltaNotification,
    ToolDeltaNotification, ToolStartedNotification,
};
use joyczl_provider::{Resolved, TextSink};

/// The output of one full loop, plus this turn's retrieval-gate decision.
///
/// The gate is a detail **inside the full path**: the quick-answer path does not even consult memory —
/// that is exactly why it is cheap, and why it has no gate.
pub(crate) struct FullTurn {
    pub(crate) result: LoopResult,
    pub(crate) gate: Option<GateDecision>,
    /// What history/compaction looked like for this turn — straight into the trace.
    pub(crate) history: HistoryStats,
}

pub(crate) async fn full_turn(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    user_images: Vec<joyczl_provider::ContentBlock>,
    frame: TurnFrame<'_>,
    inner: Option<joyczl_loop::Observer>,
) -> Result<FullTurn, ErrorObject> {
    let TurnFrame {
        session_id,
        turn_id,
        sink,
        interrupt,
        turn_cwd,
        // route_reason is not used here: TurnMeta is assembled by run_turn (which holds turn_route).
        resume,
        checkpoint,
        steering,
        ..
    } = frame.clone();
    let settings = server.settings();
    let chat_operations = crate::turn::operations::context(server, session_id).await?;
    let task_handoff = crate::task::handoff::load(server, turn_id, session_id).await?;

    // The vector leg is assembled once per turn and shared: the gate's retrieval and
    // `search_memory` must see the same embedder, or the two disagree about what "similar"
    // means. Assembly failure (no model configured, provider lacks a default endpoint)
    // warns once and both fall back to keywords — one misconfigured knob must not keep the
    // whole conversation from starting.
    let embedder = if settings.embeddings_enabled {
        match joyczl_provider::embed::Embedder::from_settings(&settings) {
            Ok(embedder) => Some(Arc::new(embedder)),
            Err(why) => {
                eprintln!(
                    "(joy) vector search is not configured; this turn uses keywords only: {why}"
                );
                None
            }
        }
    } else {
        None
    };

    let (gate_decision, memory_context) = retrieve_memory(
        server,
        resolved,
        message,
        &settings,
        &frame,
        embedder.as_deref(),
    )
    .await;

    // ---- Procedural memory: SKILL.md progressive disclosure. Frontmatter is always scanned (cheap),
    // the body enters the prompt only when the message matches.
    let mut skill_loader = joyczl_memory::skills::SkillLoader::new(
        joyczl_memory::skills::SkillLoader::dirs_for(&settings.home),
    );
    // The body cap is a prompt-shape knob: a 6 KB SKILL.md dropped in whole because the
    // message said "deploy" is the same context problem as an unpruned memory catalog.
    let skill_hits = skill_loader.hits_capped(message, settings.skill_body_tokens.max(1) as usize);
    let skills = skill_hits.section;
    // The catalog is the other half of progressive disclosure: names and one-liners ride
    // along every turn, bodies only when chosen (by keyword here, by `read_skill` when the
    // model decides for itself).
    let skill_index = skill_loader.index_text(settings.skill_index_tokens.max(1) as usize);
    // Explicit references (`$skill-name`) are stripped before sending to the model — references are for
    // the loader; the body text is enough for it. A reference to a nonexistent skill appends a hint, so it
    // can reply instead of pretending not to see. The persisted history remains **the user's original words** (run_turn's business).
    let prompt_message = if skill_hits.hints.is_empty() {
        skill_hits.message.clone()
    } else {
        format!("{}\n\n{}", skill_hits.hints.join("\n"), skill_hits.message)
    };

    let memory_context = format!(
        "{}{}{}",
        memory_context,
        crate::task::dispatch::projection(server, session_id).await,
        task_handoff
    );

    // ---- Working memory: sliding window + rolling summary + token budget.
    //
    // The turn count is the **ceiling**, tokens are the gate: one long tool output can blow past the
    // window while the turn count still looks "safe". The budget is "context window × JOY_COMPACT_THRESHOLD"
    // minus the parts already committed to spend (system prefix, tool declarations, this turn's answer, the summary section).
    // Estimation is approximate (cl100k), so slack is deliberately left wide.
    let soul = load_soul(
        &settings.home,
        thin_soul_for(&settings.soul_mode, &resolved.provider_id),
    );
    // Per-session tool table: deferred activation is a property of this conversation, and
    // the loop re-reads `schemas()` every round, so an activation lands on the next round.
    // Eager mode (`JOY_TOOL_POLICY=eager`, the default) leaves the table exactly as built.
    let mut turn_tools = server
        .tools
        .session_view_with(server.activation_set(session_id));
    // Declarative tools are read **here, every turn**. That is the whole feature: `define_tool`
    // writes a JSON definition and the next turn can call it — no restart, no rebuild, no code.
    // The table stays honest in both directions: with exec off there is nothing to read (a
    // definition can only ever run a command), and a definition may never displace a built-in
    // (`register` refuses the name and says so).
    if settings.exec_enabled {
        joyczl_tools::declarative::attach(
            &mut turn_tools,
            &settings.home,
            crate::exec_policy(&settings, server.exec_session_allow.clone()),
        );
    }
    if chat_operations.unresolved {
        crate::turn::operations::restrict(&mut turn_tools);
        eprintln!(
            "(joy) earlier chat operations need review; this loop permits only proven read tools"
        );
    }
    if settings.tool_policy == "deferred" {
        turn_tools.defer_all_but_always();
    }
    let reserve = joyczl_provider::tokens::estimate_text(&soul)
        + joyczl_provider::tokens::estimate_text(&memory_context)
        + joyczl_provider::tokens::estimate_text(&skills)
        + joyczl_provider::tokens::estimate_tools(&turn_tools.schemas())
        + settings.max_tokens.max(0) as usize
        + SUMMARY_RESERVE_TOKENS;
    // Last turn's measured/estimated ratio: use it to calibrate this estimate (no record, no calibration).
    let factor = server.chat.context_factor(session_id).await.unwrap_or(None);
    let history_budget = budget_tokens(resolved, &settings, factor).saturating_sub(reserve);

    // ---- Compaction may happen here (compaction::refresh inside load_history), and it belongs
    // to this session's write section: serialised against another concurrent turn's write
    // section. The lock is held only while reading history/compacting — THE LOOP and interrupt
    // never wait on it.
    let session_write_lock = server.session_write_lock(session_id);
    // A resumed turn skips history assembly: its working memory IS the checkpoint — history,
    // user message and finished tool rounds are all already inside it. Compaction (a model
    // call) must not run just to produce a summary that gets discarded.
    let Loaded {
        messages: mut history,
        mut summary,
        stats: mut history_stats,
    } = if resume.is_some() {
        Loaded::empty()
    } else {
        {
            let _compact_guard = session_write_lock.lock().await;
            // Compaction calls the model too: it is raced against the stop and bounded, so
            // "stop" never queues behind a summary.
            let loaded = interruptible(
                interrupt.as_ref(),
                "compaction",
                tokio::time::timeout(
                    PRELOOP_TIMEOUT,
                    load_history(
                        &server.chat,
                        resolved,
                        session_id,
                        settings.history_turns,
                        history_budget,
                        None,
                        &server.hooks,
                    ),
                ),
            )
            .await;
            match loaded {
                Some(Ok(loaded)) => loaded,
                Some(Err(_)) => {
                    eprintln!(
                        "(joy) compaction took over {}s; this turn goes without history",
                        PRELOOP_TIMEOUT.as_secs()
                    );
                    Loaded::empty()
                }
                // Stopped while compacting: the loop will not run, and handing it a
                // half-built history would be worse than an empty one.
                None => Loaded::empty(),
            }
        }
    };
    // The catalog a new session carries (index layer). Read once per turn — it is a small
    // regenerated file, and the cost of a stat + read is far below a single prompt token.
    let memory_index = joyczl_memory::mirror::recall_catalog(
        &server.facts,
        &joyczl_memory::bundle::Bundle::new(&settings.home),
    )
    .await
    .unwrap_or_else(|error| {
        eprintln!("(joy) memory catalog unavailable: {error}");
        String::new()
    });

    // The repository's own conventions, read from the folder the human picked. No folder ⇒
    // nothing to read (and no guessing at the process cwd, which is wherever the server
    // happened to start — that is not the human's choice).
    let instructions = turn_cwd.as_deref().and_then(crate::agents_md::discover);
    let mut prompt = build_system(PromptParts {
        soul: &soul,
        model: &resolved.model,
        provider: &resolved.provider_id,
        memory: &memory_context,
        memory_index: Some(memory_index.as_str()),
        skill_index: Some(skill_index.as_str()),
        skills: &skills,
        todo: server.todo.render(session_id).as_deref(),
        summary: summary.as_deref(),
        instructions: instructions.as_deref(),
        capabilities: capability_note(&settings, !turn_tools.schemas().is_empty()).as_deref(),
    });

    prompt.tail.push_str(&chat_operations.prompt);

    // ---- The meter: what this turn's prompt actually costs, section by section.
    //
    // Measured here because this is the only place all six sections exist at once; read by
    // the memory center. It is an estimate (cl100k) — the same one the budget uses, so the
    // meter and the compaction trigger never disagree about how full the window is.
    let usage = crate::memory_center::usage_of(
        vec![
            (
                "soul",
                joyczl_provider::tokens::estimate_text(&prompt.stable),
            ),
            (
                "memory",
                joyczl_provider::tokens::estimate_text(&memory_context)
                    + joyczl_provider::tokens::estimate_text(&memory_index),
            ),
            (
                "skills",
                joyczl_provider::tokens::estimate_text(&skill_index)
                    + joyczl_provider::tokens::estimate_text(&skills),
            ),
            (
                "tools",
                joyczl_provider::tokens::estimate_tools(&turn_tools.schemas()),
            ),
            (
                "history",
                joyczl_provider::tokens::estimate_messages(&history),
            ),
            (
                "summary",
                summary
                    .as_deref()
                    .map(joyczl_provider::tokens::estimate_text)
                    .unwrap_or(0),
            ),
        ],
        budget_tokens(resolved, &settings, factor),
        settings.context_warn_ratio,
    );
    if let Ok(mut map) = server.context_usage.lock() {
        map.insert(session_id.to_string(), usage);
    }

    // Streaming outlet: push model text deltas to the client as soon as they arrive.
    let delta_sink = sink.clone();
    let delta_turn_id = turn_id.to_string();
    let on_text: TextSink = Arc::new(move |delta: &str| {
        delta_sink.notification(ServerNotification::TextDelta(TextDeltaNotification {
            turn_id: delta_turn_id.clone(),
            delta: delta.to_string(),
        }));
    });

    // ---- THE LOOP
    // The tool environment is constructed in exactly one place (see tool_ctx in lib.rs) — the subagent uses
    // the same one (it passes None). The approval channel is built per turn: None in `never` mode, actions needing approval are rejected.
    let mut ctx = server.tool_ctx(session_id, turn_cwd, server.approval_bridge(turn_id, sink));
    ctx.task_run_id = server
        .runs
        .for_turn(turn_id)
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("Cannot resolve task execution identity: {e}"),
            data: None,
        })?
        .map(|run| run.id);
    ctx.activate = Some(turn_tools.activator());
    ctx.embedder = embedder;

    // The tool **start** notification must go out before execution: the client can then draw "calling X".
    // If the graph's node event outlet (inner) is also present, call both — neither blocks the other.
    // Note this is joyczl_loop's Observer (receiving LoopEvent), not the graph's one.
    let tool_started: joyczl_loop::Observer = {
        let sink = sink.clone();
        let turn_id = turn_id.to_string();
        Arc::new(move |event| match event {
            LoopEvent::ToolStart {
                tool_use_id,
                name,
                args,
            } => {
                sink.notification(ServerNotification::ToolStarted(ToolStartedNotification {
                    turn_id: turn_id.clone(),
                    tool_use_id,
                    tool: name,
                    args,
                }));
            }
            // Live progress from a running tool: forwarded as-is, never buffered — the
            // `ToolCompleted` that follows is the real result.
            LoopEvent::ToolDelta { tool_use_id, chunk } => {
                sink.notification(ServerNotification::ToolDelta(ToolDeltaNotification {
                    turn_id: turn_id.clone(),
                    tool_use_id,
                    chunk,
                }));
            }
            _ => {}
        })
    };
    let observer: Option<joyczl_loop::Observer> = match inner {
        // Running inside the graph: the tool-start notification goes out first, then node events go to the engine to attach node=.
        Some(inner) => {
            let combined: joyczl_loop::Observer = Arc::new(move |event| {
                tool_started(event.clone());
                inner(event);
            });
            Some(combined)
        }
        // No graph: the tool-start notification is the only observer.
        None => Some(tool_started),
    };

    // Run one turn; **only** context overflow is worth a retry after compaction — retrying other errors
    // just yields the same error again (same rule as that comment in graph_route).
    let mut retried = false;
    let result: LoopResult = loop {
        let attempt = joyczl_loop::run(joyczl_loop::Turn {
            system_prefix: Some(prompt.stable.clone()),
            client: resolved.client.as_ref(),
            model: &resolved.model,
            system: prompt.tail.clone(),
            // Fresh start (history + user message) or resume (the checkpointed working memory
            // as-is, `user_message` ignored): both end up as the same `joyczl_loop::Turn`.
            history: history.clone(),
            user_message: prompt_message.clone(),
            user_images: user_images.clone(),
            tools: &turn_tools,
            ctx: ctx.clone(),
            // A continuation carries its own ceiling: it resumes with the steps already spent, so
            // the session cap would leave it zero room. `None` = the session setting, as always.
            max_iterations: resume
                .as_ref()
                .and_then(|point| point.max_iterations)
                .unwrap_or(settings.max_iterations),
            max_tokens: settings.max_tokens,
            // 0 = off; the loop treats `None` and 0 the same, but converting here keeps the
            // "off" decision in one place (the config).
            token_budget: Some(settings.turn_token_budget)
                .filter(|budget| *budget > 0)
                .map(|budget| budget as usize),
            tool_result_budget: crate::tool_result_budget(&settings),
            observer: observer.clone(),
            on_text: Some(on_text.clone()),
            interrupt: interrupt.clone(),
            checkpoint: checkpoint.clone(),
            steering: steering.clone(),
            resume: resume.clone(),
        })
        .await;

        match attempt {
            Ok(result) => break result,
            Err(e) if !retried && is_context_overflow(&e) => {
                eprintln!("(joy) context overflowed; compacting and retrying once");
                retried = true;
                let forced = {
                    let _compact_guard = session_write_lock.lock().await;
                    load_history(
                        &server.chat,
                        resolved,
                        session_id,
                        settings.history_turns,
                        0,
                        Some(FORCED_RETRY_TURNS),
                        &server.hooks,
                    )
                    .await
                };
                history = forced.messages;
                summary = forced.summary;
                history_stats = forced.stats;
                // Only the tail changes after a forced compaction — the stable prefix is the
                // same bytes as before, so the cache still serves it.
                prompt = build_system(PromptParts {
                    soul: &soul,
                    model: &resolved.model,
                    provider: &resolved.provider_id,
                    memory: &memory_context,
                    memory_index: Some(memory_index.as_str()),
                    skill_index: Some(skill_index.as_str()),
                    skills: &skills,
                    todo: server.todo.render(session_id).as_deref(),
                    summary: summary.as_deref(),
                    instructions: instructions.as_deref(),
                    capabilities: capability_note(&settings, !turn_tools.schemas().is_empty())
                        .as_deref(),
                });
            }
            Err(e) => {
                return Err(ErrorObject {
                    code: codes::PROVIDER_ERROR,
                    message: format!("Model call failed: {e}"),
                    data: None,
                })
            }
        }
    };

    Ok(FullTurn {
        result,
        gate: Some(gate_decision),
        history: history_stats,
    })
}
