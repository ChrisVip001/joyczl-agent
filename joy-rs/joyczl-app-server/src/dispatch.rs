//! 方法分发：JSON-RPC method → 一句 state 操作 / 一轮 turn。
//!
//! 每个方法做的事都很少：解析 `*Params` → 调对应的 crate → 装成 `*Response`。
//! turn 是唯一复杂的那条路径，它的流程在 turn.rs 里 —— 它的通知和应答
//! 顺序必须由它自己控制，所以不走「返回值」这条路。

use joyczl_protocol::{
    codes, methods, ConfigReadParams, ConfigReadResponse, ConfigWriteParams, ConfigWriteResponse,
    ErrorObject, JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, MemoryForgetParams,
    MemoryForgetResponse, MemoryListEpisodesParams, MemoryListEpisodesResponse, MemoryListParams,
    MemoryListResponse, MemoryRememberParams, MemoryRememberResponse, MemorySearchParams,
    MemorySearchResponse, MessageRole, ModelInfo, ModelListParams, ModelListResponse, RequestId,
    SessionListParams, SessionListResponse, SessionMessagesParams, SessionMessagesResponse,
    SessionNewParams, SessionNewResponse, TurnInterruptParams, TurnInterruptResponse,
    TurnStartParams, JSONRPC_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{narrow, turn, EventSink, Server};

/// 一个方法的产出：要么是应答体（由 handle 包装送出），
/// 要么声明「我自己送」（turn —— 它要在应答之前发完所有通知）。
enum Outcome {
    Value(Value),
    SelfSent,
}

/// 处理一条请求。**永远会有一个应答送出去**：业务错误走 JSON-RPC 的
/// error 对象，不会让进程退出，也不会让客户端等到超时。
pub async fn handle(server: &Server, request: JsonRpcRequest, sink: EventSink) {
    let id = request.id.clone();
    let message = match dispatch(server, &request.method, request.params.as_ref(), &id, &sink).await
    {
        Ok(Outcome::Value(result)) => JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }),
        Ok(Outcome::SelfSent) => return, // 应答已经发出去了
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
        // ---- 记忆 ----------------------------------------------------------
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
            // 没有游标：recent 就是「最近 N 条」，下一页无从谈起（见协议里的说明）。
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
            let row = server
                .facts
                .add(&p.subject, &p.content, &source)
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

        // ---- 会话 ----------------------------------------------------------
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
            // 会话只是 chat_log 上的一个标签，所以「新建」不建任何东西，
            // 只是发一个新标签。第一条消息写进来时它才真正存在。
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

            // 游标按**取回来的原始行**算，不按过滤后的条数：`to_message` 会
            // 丢掉不认识的角色，若拿过滤后的长度判断「还有没有下一页」，
            // 丢掉一行就会让翻页提前到头。
            let next_cursor = (rows.len() as u32 == limit)
                .then(|| rows.last().map(|row| row.id.to_string()))
                .flatten();

            respond(SessionMessagesResponse {
                data: rows.into_iter().filter_map(to_message).collect(),
                next_cursor,
            })
        }

        // ---- 配置 ----------------------------------------------------------
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

        // ---- 模型 ----------------------------------------------------------
        methods::MODEL_LIST => {
            let p: ModelListParams = parse(params, method)?;
            // 目录就是 PROVIDERS 这张表：主模型是 id，flagship / fast 是
            // 它的两个展示位。没有 cursor —— 目录就 11 行，翻页是无中生有。
            let data = joyczl_provider::PROVIDERS
                .iter()
                .filter(|info| p.provider.as_deref().is_none_or(|want| info.id == want))
                .map(|info| ModelInfo {
                    id: info.model.to_string(),
                    provider: info.id.to_string(),
                    label: None,
                    flagship: Some(info.model.to_string()),
                    fast: Some(info.small_model.to_string()),
                })
                .collect();
            respond(ModelListResponse {
                data,
                next_cursor: None,
            })
        }

        // ---- turn ----------------------------------------------------------
        methods::TURN_START => {
            let p: TurnStartParams = parse(params, method)?;
            // turn 的应答由 run_turn 自己送 —— 它要在应答之前发完所有通知。
            turn::run_turn(server, p, request_id.clone(), sink).await?;
            Ok(Outcome::SelfSent)
        }

        methods::TURN_INTERRUPT => {
            let p: TurnInterruptParams = parse(params, method)?;
            // 拨下取消令牌就立刻应答 —— 真正的收兵（半截回复落库、
            // turnCompleted 通知）由正在跑的那轮自己完成。
            let interrupted = server.interrupt_turn(&p.turn_id);
            respond(TurnInterruptResponse { interrupted })
        }

        other => Err(ErrorObject {
            code: codes::METHOD_NOT_FOUND,
            message: format!("未知方法 '{other}'"),
            data: None,
        }),
    }
}

/// 把 `*Response` 装进 JSON-RPC 应答。
fn respond<T: serde::Serialize>(value: T) -> Result<Outcome, ErrorObject> {
    let encoded = serde_json::to_value(value).map_err(internal)?;
    Ok(Outcome::Value(encoded))
}

// ---- 参数与结果 ------------------------------------------------------------

/// 情景记忆默认返回的条数：比语义记忆少，因为「想起最近发生了什么」
/// 不需要很多条，多了反而稀释上下文。
const EPISODE_HITS: u32 = 3;
/// 列情景记忆的默认条数。跟 `EPISODE_HITS` 不同：那个是「塞进 prompt 的
/// 检索命中」，多了会稀释上下文；这个是给人看的列表，少了才奇怪。
const EPISODE_LIMIT: u32 = 20;
const DEFAULT_TOP_K: u32 = 4;
const DEFAULT_LIMIT: u32 = 50;

/// 没传 params 时按 `{}` 解析，这样全是 Option 的 `*Params` 能走 Default。
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

// ---- 存储行 → 协议类型 -----------------------------------------------------

fn to_fact(row: joyczl_state::FactRow) -> joyczl_protocol::Fact {
    joyczl_protocol::Fact {
        id: narrow(row.id),
        subject: row.subject,
        content: row.content,
        source: row.source,
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

/// `chat_log` 的一行 → 协议里的一条消息。
///
/// 返回 `Option`：`MessageRole` 是个闭集，认不出来的角色**不往协议里搬**
/// （硬塞一个值进去就得撒谎，前端那个穷尽的 `switch` 也就白写了）。
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
        // 解不出来就当没有：这一列是遥测，形状将来变了、或者某行是手写的
        // 脏数据，都不该让整页历史读不出来。
        meta: row
            .meta
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
    })
}

// ---- 时间 ------------------------------------------------------------------

/// SQLite 的 `datetime('now')` → 协议要的 ISO 8601。
///
/// 库里存的是 `2026-09-19 16:46:49`：UTC，但没有时区标记，也不是 ISO 的 `T`
/// 分隔。浏览器拿到它时，`new Date("2026-09-19 16:46:49")` 会当成**本地时间**
/// —— 东八区就整整偏 8 小时。所以在出口这儿补上 `Z`，让它变成一个没有歧义的
/// 时刻。**转换只做在这一处**：库里存的原样不动，出去的一律是 ISO。
///
/// `datetime('now')` 本身就是 UTC，所以补 `Z` 是对的，不是猜的。
///
/// 认不出来就原样返回 —— 这只是一层格式化，不该有能力让整个方法报错。
fn iso(at: String) -> String {
    chrono::NaiveDateTime::parse_from_str(&at, "%Y-%m-%d %H:%M:%S")
        .map(|naive| {
            naive
                .and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or(at)
}

/// 见 `iso`。这几个时间列在库里是可空的。
fn iso_opt(at: Option<String>) -> Option<String> {
    at.map(iso)
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod dispatch_tests;
