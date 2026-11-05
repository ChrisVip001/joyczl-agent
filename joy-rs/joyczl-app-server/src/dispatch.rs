//! Method dispatch: JSON-RPC method → one state operation / one turn.
//!
//! Each method does very little: parse `*Params` → call the corresponding crate → wrap into `*Response`.
//! The turn is the only complex path, and its flow lives in turn.rs — its notification and response
//! ordering must be controlled by itself, so it does not take the "return value" route.

use joyczl_protocol::{
    codes, methods, ApprovalLogParams, ApprovalRespondParams, ApprovalRespondResponse,
    ConfigReadParams, ConfigReadResponse, ConfigWriteParams, ConfigWriteResponse, ErrorObject,
    GoalSetParams, JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
    MemoryForgetParams, MemoryForgetResponse, MemoryListEpisodesParams, MemoryListEpisodesResponse,
    MemoryListParams, MemoryListResponse, MemoryRememberParams, MemoryRememberResponse,
    MemorySearchParams, MemorySearchResponse, MessageRole, ModelInfo, ModelListParams,
    ModelListResponse, ModelProfilesParams, ModelRemoveParams, ModelSaveParams, RequestId,
    ScheduleListParams, SessionListParams, SessionListResponse, SessionMessagesParams,
    SessionMessagesResponse, SessionNewParams, SessionNewResponse, SessionViewParams,
    TaskActionParams, TaskCreateParams, TurnInterruptParams, TurnInterruptResponse,
    TurnStartParams, UsageSummaryParams, JSONRPC_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{narrow, turn, EventSink, Server};

/// The product of one method: either a response body (wrapped and sent by handle),
/// or a declaration "I send it myself" (turn — it must finish all notifications before the response).
enum Outcome {
    Value(Value),
    SelfSent,
}

/// Handle one request. **A response is always sent out**: business errors go through JSON-RPC's
/// error object; they neither exit the process nor leave the client waiting until timeout.
pub async fn handle(server: &Server, request: JsonRpcRequest, sink: EventSink) {
    let id = request.id.clone();
    let message = match dispatch(server, &request.method, request.params.as_ref(), &id, &sink).await
    {
        Ok(Outcome::Value(result)) => JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }),
        Ok(Outcome::SelfSent) => return, // The response has already been sent
        Err(error) => JsonRpcMessage::Error(JsonRpcError {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id),
            error,
        }),
    };
    sink.response(message);
}

async fn dispatch(
    server: &Server,
    method: &str,
    params: Option<&Value>,
    request_id: &RequestId,
    sink: &EventSink,
) -> Result<Outcome, ErrorObject> {
    match method {
        // ---- Memory ----------------------------------------------------------
        methods::MEMORY_SEARCH => {
            let p: MemorySearchParams = parse(params, method)?;
            let top_k = p.top_k.unwrap_or(DEFAULT_TOP_K);
            let facts = server
                .facts
                .search(&p.query, top_k)
                .await
                .map_err(internal)?;
            let episodes = server
                .episodes
                .search(&p.query, EPISODE_HITS)
                .await
                .map_err(internal)?;
            respond(MemorySearchResponse {
                facts: facts.into_iter().map(to_fact).collect(),
                episodes: episodes.into_iter().map(to_episode).collect(),
            })
        }

        methods::MEMORY_LIST => {
            let p: MemoryListParams = parse(params, method)?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            let offset = p
                .cursor
                .as_deref()
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(0);
            let rows = server.facts.recent(limit, offset).await.map_err(internal)?;
            let next = (rows.len() as u32 == limit).then(|| (offset + limit).to_string());
            respond(MemoryListResponse {
                data: rows.into_iter().map(to_fact).collect(),
                next_cursor: next,
            })
        }

        methods::MEMORY_LIST_EPISODES => {
            let p: MemoryListEpisodesParams = parse(params, method)?;
            let limit = p.limit.unwrap_or(EPISODE_LIMIT);
            let rows = server.episodes.recent(limit).await.map_err(internal)?;
            // No cursor: recent is simply "the most recent N rows"; a next page makes no sense (see the protocol docs).
            respond(MemoryListEpisodesResponse {
                data: rows.into_iter().map(to_episode).collect(),
                next_cursor: None,
            })
        }

        methods::MEMORY_REMEMBER => {
            let p: MemoryRememberParams = parse(params, method)?;
            let source = p
                .source
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "user".to_string());
            let (row, _) = server
                .facts
                .add(&p.subject, &p.content, &source, "fact")
                .await
                .map_err(internal)?;
            respond(MemoryRememberResponse { fact: to_fact(row) })
        }

        methods::MEMORY_FORGET => {
            let p: MemoryForgetParams = parse(params, method)?;
            let removed = server
                .facts
                .forget_subject(&p.subject)
                .await
                .map_err(internal)?;
            respond(MemoryForgetResponse {
                removed: narrow(removed as i64),
            })
        }

        // ---- Sessions ----------------------------------------------------------
        methods::SESSION_LIST => {
            let p: SessionListParams = parse(params, method)?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT) as usize;
            let offset = p
                .cursor
                .as_deref()
                .and_then(|c| c.parse::<usize>().ok())
                .unwrap_or(0);
            let all = server.chat.sessions().await.map_err(internal)?;
            let data: Vec<_> = all
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .map(to_session)
                .collect();
            let next = (offset + limit < all.len()).then(|| (offset + limit).to_string());
            respond(SessionListResponse {
                data,
                next_cursor: next,
            })
        }

        methods::SESSION_NEW => {
            let p: SessionNewParams = parse(params, method)?;
            // A session is just a label on chat_log, so "new" creates nothing,
            // it only issues a new label. It truly exists once the first message is written into it.
            let session_id = p
                .session_id
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(new_session_id);
            respond(SessionNewResponse { session_id })
        }

        methods::SESSION_MESSAGES => {
            let p: SessionMessagesParams = parse(params, method)?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            let before = p.cursor.as_deref().and_then(|c| c.parse::<i64>().ok());
            let rows = server
                .chat
                .messages(&p.session_id, before, limit)
                .await
                .map_err(internal)?;

            // The cursor is computed from the **raw rows fetched**, not the filtered count: `to_message`
            // drops unrecognized roles; if "is there a next page" were judged from the filtered length,
            // one dropped row would end pagination one page early.
            let next_cursor = (rows.len() as u32 == limit)
                .then(|| rows.last().map(|row| row.id.to_string()))
                .flatten();

            respond(SessionMessagesResponse {
                data: rows.into_iter().filter_map(to_message).collect(),
                next_cursor,
            })
        }

        // ---- Config ----------------------------------------------------------
        methods::CONFIG_READ => {
            let _p: ConfigReadParams = parse(params, method)?;
            respond(ConfigReadResponse {
                config: server.settings().view(),
            })
        }

        methods::CONFIG_WRITE => {
            let p: ConfigWriteParams = parse(params, method)?;
            let view = server
                .apply_config_patch(&p.patch)
                .map_err(|message| ErrorObject {
                    code: codes::INVALID_PARAMS,
                    message,
                    data: None,
                })?;
            respond(ConfigWriteResponse { config: view })
        }

        // ---- Models ----------------------------------------------------------
        // ---- Model config (only a human can change it: the model has no such capability) -------
        methods::MODEL_PROFILES => {
            // Parameterless method: clients habitually pass {} anyway, and a unit struct cannot take a
            // map — check here ourselves and reject non-empty input explicitly (instead of an unreadable serde error).
            if let Some(value) = params {
                if value.as_object().is_some_and(|o| !o.is_empty()) {
                    return Err(ErrorObject {
                        code: codes::INVALID_PARAMS,
                        message: format!("{method} 不接受参数"),
                        data: None,
                    });
                }
            }
            let _: ModelProfilesParams = ModelProfilesParams;
            respond(server.model_profiles())
        }

        methods::MODEL_SAVE => {
            let p: ModelSaveParams = parse(params, method)?;
            match server.model_save(p) {
                Ok(response) => respond(response),
                Err(message) => Err(ErrorObject {
                    code: codes::INVALID_PARAMS,
                    message,
                    data: None,
                }),
            }
        }

        methods::MODEL_REMOVE => {
            let p: ModelRemoveParams = parse(params, method)?;
            respond(server.model_remove(p))
        }

        methods::USAGE_SUMMARY => {
            // Parameterless method: clients habitually pass {} anyway (see the same note under model/profiles).
            if let Some(value) = params {
                if value.as_object().is_some_and(|o| !o.is_empty()) {
                    return Err(ErrorObject {
                        code: codes::INVALID_PARAMS,
                        message: format!("{method} 不接受参数"),
                        data: None,
                    });
                }
            }
            let _: UsageSummaryParams = UsageSummaryParams;
            respond(server.usage_summary().await)
        }

        methods::SCHEDULE_LIST => {
            // Parameterless method: see the same note under model/profiles.
            if let Some(value) = params {
                if value.as_object().is_some_and(|o| !o.is_empty()) {
                    return Err(ErrorObject {
                        code: codes::INVALID_PARAMS,
                        message: format!("{method} 不接受参数"),
                        data: None,
                    });
                }
            }
            let _: ScheduleListParams = ScheduleListParams;
            respond(server.schedule_list().await)
        }

        methods::APPROVAL_LOG => {
            // Parameterless method: see the same note under model/profiles.
            if let Some(value) = params {
                if value.as_object().is_some_and(|o| !o.is_empty()) {
                    return Err(ErrorObject {
                        code: codes::INVALID_PARAMS,
                        message: format!("{method} 不接受参数"),
                        data: None,
                    });
                }
            }
            let _: ApprovalLogParams = ApprovalLogParams;
            respond(server.approval_log().await)
        }

        methods::MODEL_LIST => {
            let p: ModelListParams = parse(params, method)?;
            // The catalog is the merged provider table (built-in + the user's override
            // file): the main model is the id, and flagship / fast are its two display
            // slots. No cursor — the catalog is a dozen rows; pagination would be invention.
            let data = server
                .catalog
                .entries()
                .iter()
                .filter(|info| p.provider.as_deref().is_none_or(|want| info.id == want))
                .map(|info| ModelInfo {
                    id: info.model.clone(),
                    provider: info.id.clone(),
                    label: None,
                    flagship: Some(info.model.clone()),
                    fast: info.small_model.clone(),
                })
                .collect();
            respond(ModelListResponse {
                data,
                next_cursor: None,
            })
        }

        // ---- Turn ----------------------------------------------------------
        methods::TURN_START => {
            let p: TurnStartParams = parse(params, method)?;
            // The turn's response is sent by run_turn itself — it must finish all notifications before the response.
            turn::run_turn(server, p, request_id.clone(), sink).await?;
            Ok(Outcome::SelfSent)
        }

        methods::TURN_INTERRUPT => {
            let p: TurnInterruptParams = parse(params, method)?;
            // Flip the cancellation token and answer immediately — the real wind-down (persisting the
            // half-written reply, the turnCompleted notification) is done by the running turn itself.
            let interrupted = server.interrupt_turn(&p.turn_id);
            respond(TurnInterruptResponse { interrupted })
        }

        methods::GOAL_SET => {
            let p: GoalSetParams = parse(params, method)?;
            // Only a human can reach here: the model has no such tool, and no other method touches the goal table.
            respond(server.goal_set(&p.session_id, p.condition.as_deref()).await)
        }

        methods::GOAL_VIEW => {
            let p: SessionViewParams = parse(params, method)?;
            // Read-only: the dashboard's task card asks what the loop is doing. The model
            // has no path here either — its own goal view arrives via the prompt injection.
            respond(server.goal_view(&p.session_id).await)
        }

        // ---- Long tasks: every branch is human-only (the model has no task tool) ----
        methods::TASK_CREATE => {
            let p: TaskCreateParams = parse(params, method)?;
            respond(crate::task::create(server, p).await?)
        }

        methods::TASK_LIST => {
            // Parameterless method: see the same note under model/profiles.
            if let Some(value) = params {
                if value.as_object().is_some_and(|o| !o.is_empty()) {
                    return Err(ErrorObject {
                        code: codes::INVALID_PARAMS,
                        message: format!("{method} 不接受参数"),
                        data: None,
                    });
                }
            }
            respond(crate::task::list(server).await)
        }

        methods::TASK_VIEW => {
            let p: SessionViewParams = parse(params, method)?;
            respond(crate::task::view_of(server, &p.session_id).await)
        }

        methods::TASK_SUSPEND => {
            let p: TaskActionParams = parse(params, method)?;
            respond(crate::task::suspend(server, sink, p).await?)
        }

        methods::TASK_RESUME => {
            let p: TaskActionParams = parse(params, method)?;
            respond(crate::task::resume(server, sink, p).await?)
        }

        methods::TASK_CANCEL => {
            let p: TaskActionParams = parse(params, method)?;
            respond(crate::task::cancel(server, sink, p).await?)
        }

        methods::TODO_VIEW => {
            let p: SessionViewParams = parse(params, method)?;
            // None = the model never wrote a sheet for this session — the card just doesn't render.
            respond(server.todo_view(&p.session_id))
        }

        methods::APPROVAL_RESPOND => {
            let p: ApprovalRespondParams = parse(params, method)?;
            // Delivered too late (timed out, that turn already ended) is not an error — answer an honest
            // accepted: false, so the client knows "nobody heard my answer".
            let accepted =
                server.answer_approval(&p.turn_id, &p.request_id, p.approved, p.remember);
            respond(ApprovalRespondResponse { accepted })
        }

        other => Err(ErrorObject {
            code: codes::METHOD_NOT_FOUND,
            message: format!("未知方法 '{other}'"),
            data: None,
        }),
    }
}

/// Wrap a `*Response` into a JSON-RPC response.
fn respond<T: serde::Serialize>(value: T) -> Result<Outcome, ErrorObject> {
    let encoded = serde_json::to_value(value).map_err(internal)?;
    Ok(Outcome::Value(encoded))
}

// ---- Params and results ------------------------------------------------------------

/// Default number of episodic memories returned: fewer than semantic memory, because "recall what
/// happened recently" does not need many items — more would dilute the context.
const EPISODE_HITS: u32 = 3;
/// Default number of rows for listing episodic memory. Different from `EPISODE_HITS`: that one is
/// "retrieval hits stuffed into the prompt", where more dilutes context; this is a list for humans, where fewer would be odd.
const EPISODE_LIMIT: u32 = 20;
const DEFAULT_TOP_K: u32 = 4;
const DEFAULT_LIMIT: u32 = 50;

/// When params is absent, parse as `{}` so all-Option `*Params` can take the Default path.
fn parse<T: DeserializeOwned>(params: Option<&Value>, method: &str) -> Result<T, ErrorObject> {
    let value = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    serde_json::from_value(value).map_err(|e| ErrorObject {
        code: codes::INVALID_PARAMS,
        message: format!("{method} 的参数不对：{e}"),
        data: None,
    })
}

fn internal(e: impl std::fmt::Display) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message: e.to_string(),
        data: None,
    }
}

fn new_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s{millis}")
}

// ---- Storage rows → protocol types -----------------------------------------------------

fn to_fact(row: joyczl_state::FactRow) -> joyczl_protocol::Fact {
    joyczl_protocol::Fact {
        id: narrow(row.id),
        subject: row.subject,
        content: row.content,
        source: row.source,
        kind: row.kind,
        created_at: iso_opt(row.created_at),
    }
}

fn to_episode(row: joyczl_state::EpisodeRow) -> joyczl_protocol::Episode {
    joyczl_protocol::Episode {
        id: narrow(row.id),
        happened_at: row.happened_at,
        summary: row.summary,
        created_at: iso_opt(row.created_at),
    }
}

fn to_session(row: joyczl_state::SessionRow) -> joyczl_protocol::SessionSummary {
    joyczl_protocol::SessionSummary {
        id: row.id,
        title: row.title,
        messages: narrow(row.messages),
        started_at: iso_opt(row.started_at),
        last_at: iso_opt(row.last_at),
    }
}

/// One row of `chat_log` → one protocol message.
///
/// Returns an `Option`: `MessageRole` is a closed set; roles it cannot recognize are **not carried
/// into the protocol** (forcing a value in would be a lie, and would waste the frontend's exhaustive `switch`).
fn to_message(row: joyczl_state::MessageRow) -> Option<joyczl_protocol::Message> {
    let role = match row.role.as_str() {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => return None,
    };
    Some(joyczl_protocol::Message {
        id: narrow(row.id),
        role,
        content: row.content,
        at: iso(row.at),
        // If it cannot be parsed, treat it as absent: this column is telemetry; whether the shape changed
        // later or a row holds hand-written dirty data, none of it should make the whole history unreadable.
        meta: row
            .meta
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
    })
}

// ---- Time ------------------------------------------------------------------

/// SQLite's `datetime('now')` → the ISO 8601 the protocol wants.
///
/// The db stores `2026-09-19 16:46:49`: UTC, but without a timezone marker and not ISO's `T`
/// separator. When a browser gets it, `new Date("2026-09-19 16:46:49")` is treated as **local time** —
/// in UTC+8 a full 8-hour shift. So `Z` is appended at this outlet, making it an unambiguous instant.
/// **The conversion happens only here**: what is stored stays untouched; everything going out is ISO.
///
/// `datetime('now')` is itself UTC, so appending `Z` is correct, not a guess.
///
/// Unrecognizable input is returned as-is — only a formatting layer; it must not be able to make the whole method error out.
fn iso(at: String) -> String {
    chrono::NaiveDateTime::parse_from_str(&at, "%Y-%m-%d %H:%M:%S")
        .map(|naive| {
            naive
                .and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or(at)
}

/// See `iso`. These time columns are nullable in the db.
fn iso_opt(at: Option<String>) -> Option<String> {
    at.map(iso)
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod dispatch_tests;
