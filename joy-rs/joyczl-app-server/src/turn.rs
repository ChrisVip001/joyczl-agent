//! 一轮 turn 的装配 —— 白板上"Ephemeral Agent Run"那个框。
//!
//! 这里的每样东西都是**每轮重建、用完就扔**：system prompt、工作记忆、
//! 检索到的上下文。能留下来的都在 state.db 里（joyczl-state）。
//!
//! 流程：检索门 → system prompt → 滑窗历史 → THE LOOP → 落库 → consolidation。
//!
//! 最前面还有一道**前门**：triage 图（`graph_workflows`，默认关）。
//! 开着时它决定这一轮是「快答」（小模型直接回，一次调用）还是「完整」
//! （下面那整条路）。图坏了就掉回完整路径 —— 它只能省时间，不能减能力。
//!
//! 通知**随发生随发**：模型的文本增量一到就往客户端推（dashboard 的
//! 打字机效果），工具一完成也立刻推。`turn/start` 的应答由本函数自己发
//! —— 它必须等所有通知发完之后才轮到，顺序只有它自己清楚。

use std::sync::{Arc, Mutex};

use chrono::Local;
use joyczl_graph::workflows::triage::{
    build_triage_graph, classify_message, todays_events, CalendarFn, ClassifyFn, FullFn, QuickFn,
    QUICK_REPLY_PROMPT,
};
use joyczl_graph::{
    run_graph, GraphEvent, NodeCtx, NodeWrites, Observer, State, DEFAULT_MAX_STEPS,
};
use joyczl_loop::{LoopEvent, LoopResult};
use joyczl_protocol::{
    codes, ConsolidationCompletedNotification, ErrorObject, GateDecidedNotification, GateDecision,
    GateDecisionKind, GraphEndedNotification, GraphInfo, GraphNodeEndedNotification,
    GraphNodeStartedNotification, GraphRouteKind, GraphStartedNotification, JsonRpcMessage,
    JsonRpcResponse, RequestId, ServerNotification, TextDeltaNotification, TokenUsage,
    ToolCallRecord, ToolCompletedNotification, ToolStartedNotification, ToolStatus,
    TurnCompletedNotification, TurnMeta, TurnStartParams, TurnStartResponse,
    TurnStartedNotification, JSONRPC_VERSION,
};
use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved, Role, TextSink, Usage};
use joyczl_tools::ToolCtx;

use crate::{narrow, trace, EventSink, Server};

const DEFAULT_SOUL: &str = "\
你是 Joy，一个跑在用户自己电脑上的本地个人助手。
你简洁、温暖、主动。用户告诉你的事你会记住。

规则：
- 想安排日程、解释「今天」「明天」之前，先用 current_time 确认现在几点。
- 用户说了值得长期记住的事（关于人、项目、偏好），用 save_note 存起来。
- 用户问起过去的事、某个人，用 search_memory 查；想知道你都记了什么，用 list_memory。
- 用户明确说「忘掉…」，用 forget_note。
- 用户教你一套想复用的工作流程时，先征得同意，再用 create_skill 写成 SKILL.md。
- 下面的「相关记忆」来自你自己的记忆库，可以信任。
- 每个工具的输出都写明了结果落在哪儿 —— 如实转述，别声称同步到了别处。
- [tools used: ...] 是你过去几轮实际做过的事；已经做过的不要重复做。\
";

/// SOUL.md 是用户可编辑的人格文件，第一次运行时创建。
/// 改它就改了 Joy 是谁 —— 这是最朴素的程序性记忆。
pub fn load_soul(home: &std::path::Path) -> String {
    let path = home.join("SOUL.md");
    if !path.exists() {
        let _ = std::fs::write(&path, DEFAULT_SOUL);
    }
    std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_SOUL.to_string())
}

pub async fn run_turn(
    server: &Server,
    params: TurnStartParams,
    id: RequestId,
    sink: &EventSink,
) -> Result<(), ErrorObject> {
    let started = std::time::Instant::now();
    let settings = server.settings();

    // 模型没配好：把「怎么办」说清楚，而不是丢一句 no key。
    let resolved: Resolved = server.resolved().ok_or_else(|| ErrorObject {
        code: codes::PROVIDER_ERROR,
        message: "模型还没配好：缺 API key。设好环境变量再重启 joy app-server —— \
                  例如 export ANTHROPIC_API_KEY=…（Joy 不读 .env，要用就先 \
                  `set -a; source .env; set +a`）。"
            .to_string(),
        data: None,
    })?;

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    let turn_id = new_turn_id();

    // 登记取消令牌：turn/interrupt 靠 turn_id 找到它。无论这轮怎么退出
    // （成功、报错、被打断），守卫的 Drop 都会把它摘掉。
    let interrupt = server.register_turn(&turn_id);
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
    };

    sink.notification(ServerNotification::TurnStarted(TurnStartedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        user_message: params.message.clone(),
        ts: Local::now().to_rfc3339(),
    }));

    // ---- 前门：triage 图（默认关着）。
    //
    // 图只可能让这一轮**更快**，绝不可能让它更差：任何一步出问题都掉回下面
    // 那条普通的完整 loop —— 跟检索门同一条「失败开放」的规矩。
    let (graph, full) = match graph_route(
        server,
        &resolved,
        &params.message,
        &session_id,
        &turn_id,
        sink,
        Some(interrupt.clone()),
    )
    .await?
    {
        Some(routed) => (Some(routed.info), routed.turn),
        None => (
            None,
            full_turn(
                server,
                &resolved,
                &params.message,
                &session_id,
                &turn_id,
                sink,
                None,
                Some(interrupt.clone()),
            )
            .await?,
        ),
    };
    let FullTurn { result, gate } = full;

    for call in &result.tool_calls {
        sink.notification(ServerNotification::ToolCompleted(
            ToolCompletedNotification {
                turn_id: turn_id.clone(),
                tool: call.name.clone(),
                output: call.output.clone(),
                status: if call.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(call.duration_ms)),
            },
        ));
    }

    // 工具活动折进 assistant 文本。没有这一行，模型会忘了自己已经做过、
    // 下一轮再做一次（三重预定会议就是这么来的）。
    let reply = fold_tool_activity(&result.reply, &result.tool_calls);

    // 快答是小模型答的。如实记下来 —— "你是什么模型" 才不会被自己答错。
    let quick = graph
        .as_ref()
        .is_some_and(|info| info.route == GraphRouteKind::Quick);
    let meta = TurnMeta {
        gate,
        graph,
        iterations: result.iterations,
        latency_ms: started.elapsed().as_millis() as i32,
        tools: result
            .tool_calls
            .iter()
            .map(|c| ToolCallRecord {
                tool: c.name.clone(),
                status: if c.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(c.duration_ms)),
            })
            .collect(),
        model: if quick {
            resolved.small_model.clone()
        } else {
            resolved.model.clone()
        },
        provider: resolved.provider_id.clone(),
        usage: Some(TokenUsage {
            input_tokens: narrow(result.usage.input_tokens),
            output_tokens: narrow(result.usage.output_tokens),
        }),
        interrupted: result.interrupted,
        guard_hits: result.guard.hits,
        guard_note: result.guard.note.clone(),
    };

    let meta_json = serde_json::to_string(&meta).ok();

    // ---- 落盘：trace 一行 + usage 一行。观测不是可选项，但也不能连累对话
    // —— 两个函数内部写失败只往 stderr 喊。
    trace::record_turn(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "userMessage": params.message,
            "reply": reply,
            "meta": meta,
        }),
    );
    trace::record_usage(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "provider": meta.provider,
            "model": meta.model,
            "inputTokens": result.usage.input_tokens,
            "outputTokens": result.usage.output_tokens,
            "iterations": result.iterations,
            "interrupted": result.interrupted,
        }),
    );

    server
        .chat
        .append_exchange(
            &params.message,
            &reply,
            &session_id,
            "app-server",
            meta_json.as_deref(),
        )
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("写对话日志失败：{e}"),
            data: None,
        })?;

    // ---- 攒够了就提炼。失败不丢数据，consolidation 内部保证。
    let new_facts = joyczl_memory::consolidation::consolidate_if_due(
        &server.chat,
        &server.facts,
        &server.episodes,
        resolved.client.as_ref(),
        &resolved.small_model,
        settings.consolidate_every,
    )
    .await
    .unwrap_or(0);
    if new_facts > 0 {
        sink.notification(ServerNotification::ConsolidationCompleted(
            ConsolidationCompletedNotification {
                new_facts: new_facts as i32,
            },
        ));
    }

    // ---- MEMORY.md 镜像：每轮刷新一份人类可读的视图。
    // 它是生成的视图而不是事实来源，写失败只警告 —— 不能因为一面镜子
    // 让整轮对话报错。
    if let Err(e) =
        joyczl_memory::export_markdown(&server.facts, &server.episodes, &settings.home).await
    {
        eprintln!("(joy) MEMORY.md 镜像失败（记忆本身不受影响）：{e}");
    }

    sink.notification(ServerNotification::TurnCompleted(Box::new(
        TurnCompletedNotification {
            turn_id: turn_id.clone(),
            reply: result.reply.clone(),
            iterations: result.iterations,
            usage: Some(TokenUsage {
                input_tokens: narrow(result.usage.input_tokens),
                output_tokens: narrow(result.usage.output_tokens),
            }),
            meta,
        },
    )));

    // 全部通知发完，才轮到应答。
    sink.response(JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result: serde_json::to_value(TurnStartResponse { turn_id }).expect("序列化"),
    }));
    Ok(())
}

/// 一轮完整 loop 的产出，外加这轮检索门的决定。
///
/// 门是**完整路径内部**的细节：快答那条路连记忆都不翻，这正是它便宜的原因，
/// 所以它没有门。
struct FullTurn {
    result: LoopResult,
    gate: Option<GateDecision>,
}

/// turn 的注销守卫：无论这轮怎么退出（成功、报错、被打断），
/// 取消令牌都从表里摘掉 —— 跑完的 turn 没有可打断的东西。
struct TurnGuard<'a> {
    server: &'a Server,
    turn_id: String,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.server.finish_turn(&self.turn_id);
    }
}

/// 图的前门这一趟的收成：这一轮怎么走的，以及答案本身。
struct Routed {
    info: GraphInfo,
    turn: FullTurn,
}

/// 一轮完整 loop：检索门 → system prompt → 滑窗历史 → THE LOOP。
///
/// 图的前门打开时，`full_agent` 节点调的就是这个函数 —— 不带图的默认路径与
/// 图里那条路径**共用同一个实现**，于是「loop 作为节点」不可能跟「loop 作为
/// 默认」走偏。
///
/// `inner` 是节点留给 loop 的事件出口：图里跑的时候事件得从那儿出去，
/// 引擎才好补上 `node=full_agent`。不带图时是 `None`。
///
/// `interrupt` 一路传给 loop：模型调用与工具执行都跟「取消」竞速。
#[allow(clippy::too_many_arguments)]
async fn full_turn(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    session_id: &str,
    turn_id: &str,
    sink: &EventSink,
    inner: Option<joyczl_loop::Observer>,
    interrupt: Option<Arc<joyczl_loop::Interrupt>>,
) -> Result<FullTurn, ErrorObject> {
    let settings = server.settings();

    // ---- 检索门：这轮要不要翻记忆？
    let decision = joyczl_memory::gate::should_retrieve(
        resolved.client.as_ref(),
        &resolved.small_model,
        message,
    )
    .await;

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

    // 混合检索的那条腿：开关开着才装配 embedder。装配失败（没配模型、
    // provider 没有默认端点）只警告一次，检索退回纯关键词 —— 一个配错的
    // 旋钮不该让整轮对话起不来。
    let embedder = if settings.embeddings_enabled {
        match joyczl_provider::embed::Embedder::from_settings(&settings) {
            Ok(embedder) => Some(embedder),
            Err(why) => {
                eprintln!("(joy) 向量检索没配好，这次只用关键词：{why}");
                None
            }
        }
    } else {
        None
    };

    let memory_context = if decision.retrieve {
        joyczl_memory::retrieve_context(
            &server.facts,
            &server.episodes,
            &decision.query,
            settings.retrieval_top_k.max(1) as u32,
            embedder.as_ref(),
        )
        .await
        .unwrap_or_default()
    } else {
        String::new()
    };

    // ---- 过程记忆：SKILL.md 渐进披露。frontmatter 永远扫（便宜），
    // 正文只在消息匹配上时才进 prompt。
    let mut skill_loader = joyczl_memory::skills::SkillLoader::new(
        joyczl_memory::skills::SkillLoader::dirs_for(&settings.home),
    );
    let skills = skill_loader.matching_skills(message);

    // ---- 工作记忆：滑窗 + 滚动摘要。滑窗外被挤出去的老轮次折进摘要，
    // 摘要是往前滚的，存 state.db（见 joyczl-memory 的 compaction.rs）。
    let (history, summary) =
        load_history(&server.chat, resolved, session_id, settings.history_turns).await;

    let system = build_system(
        &load_soul(&settings.home),
        &resolved.model,
        &resolved.provider_id,
        &memory_context,
        &skills,
        summary.as_deref(),
    );

    // 流式出口：模型的文本增量一到就推给客户端。
    let delta_sink = sink.clone();
    let delta_turn_id = turn_id.to_string();
    let on_text: TextSink = Arc::new(move |delta: &str| {
        delta_sink.notification(ServerNotification::TextDelta(TextDeltaNotification {
            turn_id: delta_turn_id.clone(),
            delta: delta.to_string(),
        }));
    });

    // ---- THE LOOP
    let ctx = ToolCtx {
        facts: server.facts.clone(),
        episodes: server.episodes.clone(),
        chat: server.chat.clone(),
        calendar: server.calendar.clone(),
        home: settings.home.clone(),
    };

    // 工具**开始**的通知得在执行前发出去：客户端才能画出"正在调用 X"。
    // 图里的节点事件出口（inner）若也在，就两个都叫 —— 互不挡道。
    // 注意这是 joyczl_loop 的 Observer（收 LoopEvent），不是 graph 的那个。
    let tool_started: joyczl_loop::Observer = {
        let sink = sink.clone();
        let turn_id = turn_id.to_string();
        Arc::new(move |event| {
            if let LoopEvent::ToolStart { name, args } = event {
                sink.notification(ServerNotification::ToolStarted(ToolStartedNotification {
                    turn_id: turn_id.clone(),
                    tool: name,
                    args,
                }));
            }
        })
    };
    let observer: Option<joyczl_loop::Observer> = match inner {
        // 图里跑：工具开始的通知先发，节点事件再交给引擎补上 node=。
        Some(inner) => {
            let combined: joyczl_loop::Observer = Arc::new(move |event| {
                tool_started(event.clone());
                inner(event);
            });
            Some(combined)
        }
        // 不带图：工具开始的通知就是唯一的观察者。
        None => Some(tool_started),
    };

    let result: LoopResult = joyczl_loop::run(joyczl_loop::Turn {
        client: resolved.client.as_ref(),
        model: &resolved.model,
        system,
        history,
        user_message: message.to_string(),
        tools: &server.tools,
        ctx,
        max_iterations: settings.max_iterations,
        max_tokens: settings.max_tokens,
        observer,
        on_text: Some(on_text),
        interrupt,
    })
    .await
    .map_err(|e| ErrorObject {
        code: codes::PROVIDER_ERROR,
        message: format!("模型调用失败：{e}"),
        data: None,
    })?;

    Ok(FullTurn {
        result,
        gate: Some(gate_decision),
    })
}

/// 图的前门。
///
/// flag 关着 → `Ok(None)`，调用方走原来的路径；flag 开着而图没交出答案
/// （引擎出错、分类器坏了、节点抛错）→ 同样 `Ok(None)`，掉回普通的完整 loop。
/// 「图坏了」的代价必须是延迟，绝不能是能力。
///
/// 唯一的例外是 loop 自己失败：那个错原样返回，**不再重试一遍** —— 重试
/// 只会用同样的方式再失败一次，白等一倍时间。
async fn graph_route(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    session_id: &str,
    turn_id: &str,
    sink: &EventSink,
    interrupt: Option<Arc<joyczl_loop::Interrupt>>,
) -> Result<Option<Routed>, ErrorObject> {
    if !server.settings().graph_workflows {
        return Ok(None);
    }

    // state 只装 JSON，而一轮 loop 的产物（`LoopResult`）不是 JSON ——
    // 所以 full_agent 的**真**结果从这条侧信道递回来，写进 state 的只是
    // 「我跑过了」。
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
        Arc::new(move |state: State| {
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move {
                let prompt = QUICK_REPLY_PROMPT
                    .replace("{calendar}", state.str("calendar").unwrap_or(""))
                    .replace("{message}", state.str("message").unwrap_or(""));
                let response = client
                    .create(CreateRequest {
                        model,
                        system: None,
                        messages: vec![Message::user_text(prompt)],
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
        let slot = slot.clone();
        Arc::new(move |ctx: NodeCtx| {
            let (server, resolved, sink) = (server.clone(), resolved.clone(), sink.clone());
            let (turn_id, session_id, slot) = (turn_id.clone(), session_id.clone(), slot.clone());
            let interrupt = interrupt.clone();
            Box::pin(async move {
                // 用户这一轮说了什么就在黑板上 —— run_graph 拿到的初始 state。
                let message = ctx.state.str("message").unwrap_or_default().to_string();
                // loop 的事件从节点出口走，引擎会补上 node= 再转出来。
                let inner = Some(ctx.inner.clone());
                let outcome = full_turn(
                    &server,
                    &resolved,
                    &message,
                    &session_id,
                    &turn_id,
                    &sink,
                    inner,
                    interrupt,
                )
                .await;
                // 先记下来，再决定怎么回引擎：错只在引擎那边留个记录，
                // 真话从侧信道出去（那边的 ErrorObject 带着 code）。
                let why = outcome.as_ref().err().map(|error| error.message.clone());
                *slot.lock().expect("侧信道锁不该中毒") = Some(outcome);
                match why {
                    Some(why) => Err(why),
                    None => Ok(NodeWrites::new()),
                }
            })
        })
    };

    // 图是写死在这个文件旁边的，走到 Err 说明我们把它写坏了 —— 这轮照旧
    // 走普通路径，别拿用户的这一轮去赌一张搭不起来的图。
    let graph = match build_triage_graph(classify, calendar, quick, full) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("(joy) triage 图没搭起来，这轮走普通路径 —— {error}");
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
        // 撞键是图的 bug（并行节点写了同一个键），报出来比悄悄丢一次写好。
        Err(error) => {
            eprintln!("(joy) triage 图跑挂了，这轮走普通路径 —— {error}");
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

    // full_agent 跑过没有，看侧信道；跑过而失败，就把那个失败如实报出去。
    // 先把锁放掉再分派（这段是函数的尾表达式，守卫活不到那儿）。
    let landed = slot.lock().expect("侧信道锁不该中毒").take();
    match landed {
        Some(Err(error)) => Err(error),
        Some(Ok(turn)) => Ok(Some(Routed {
            info: info(GraphRouteKind::Full),
            turn,
        })),
        // 快答把那句话直接写在黑板上。
        None => match report.state.str("reply").filter(|reply| !reply.is_empty()) {
            Some(reply) => Ok(Some(Routed {
                info: info(GraphRouteKind::Quick),
                turn: FullTurn {
                    // 一次模型调用、没有工具 —— 就是它的全部。
                    result: LoopResult {
                        reply: reply.to_string(),
                        tool_calls: Vec::new(),
                        iterations: 1,
                        usage: Usage::default(),
                        messages: Vec::new(),
                        interrupted: false,
                        guard: joyczl_loop::guard::GuardReport::default(),
                    },
                    gate: None,
                },
            })),
            // 图什么也没交出来 —— 调用方掉回普通的完整 loop。
            None => Ok(None),
        },
    }
}

/// 图的事件翻成协议通知。
///
/// 两种事件**不**在这儿发：
///   - `Inner`：loop 那一层已经发过了（文本增量、工具完成）。再发一遍，
///     客户端会看见两次。
///   - `Route`：走了哪条路最后落在 turn 的 meta 里（`TurnMeta.graph`），
///     单独一个「选边」通知对用户没有信息量。
fn graph_event(sink: &EventSink, event: GraphEvent) {
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

fn build_system(
    soul: &str,
    model: &str,
    provider: &str,
    memory: &str,
    skills: &str,
    summary: Option<&str>,
) -> String {
    let now = Local::now();
    let mut parts = vec![
        soul.to_string(),
        // Agent 跑在用户的电脑上，就该知道用户电脑的钟 ——
        // 带时区名，"30 分钟后" 才有解。
        format!(
            "\nRight now it is {} ({}, UTC{})",
            now.format("%A, %Y-%m-%d %H:%M"),
            now.format("%Z"),
            now.format("%:z")
        ),
        // "你是什么模型"是每个好奇用户的第一问。
        format!(
            "Your model: you are running on '{model}' via the '{provider}' provider, \
             inside Joy, a local-first open-source agent harness."
        ),
    ];
    if !memory.is_empty() {
        parts.push(format!("\nRelevant memory:\n{memory}"));
    }
    if !skills.is_empty() {
        parts.push(format!("\nRelevant skill instructions:\n{skills}"));
    }
    if let Some(section) = summary.and_then(joyczl_memory::compaction::summary_section) {
        parts.push(section);
    }
    parts.join("\n")
}

/// 只取最近 N 轮，外加一份滚动摘要。
///
/// 没有滑窗，一个长会话每轮都把全部历史塞进 prompt，直到上下文爆炸；
/// 只有滑窗，被挤出去的那部分就等于失忆。所以：被挤出去的老轮次交给
/// `compaction` 折成一段摘要（失败开放，最差也是截断摘录），随会话存库，
/// 每轮拼进 system prompt。返回 `(工作记忆, 摘要)`。
async fn load_history(
    chat: &joyczl_state::Chat,
    resolved: &Resolved,
    session_id: &str,
    history_turns: i32,
) -> (Vec<Message>, Option<String>) {
    let pairs = chat.session_history(session_id).await.unwrap_or_default();
    let window = history_turns.max(0) as usize;

    let summary = joyczl_memory::compaction::refresh(
        chat,
        resolved.client.as_ref(),
        &resolved.small_model,
        session_id,
        &pairs,
        window,
    )
    .await
    .unwrap_or(None);

    let messages = pairs
        .iter()
        .rev()
        .take(window)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                Message::user_text(user.clone()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: assistant.clone(),
                    }],
                },
            ]
        })
        .collect();
    (messages, summary)
}

fn fold_tool_activity(reply: &str, calls: &[joyczl_loop::ToolOutcome]) -> String {
    if calls.is_empty() {
        return reply.to_string();
    }
    let summary = calls
        .iter()
        .map(|c| format!("{}({})", c.name, truncate(&c.output, 200)))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{reply}\n[tools used: {summary}]")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

fn new_turn_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("t{nanos}")
}
