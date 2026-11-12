//! The memory center: the ledger, the memory bundle, the context meter, skill hits, and
//! one human-triggered compaction.
//!
//! Everything here is a **window**, not a second brain. The ledger is append-only and this
//! module never writes it; the bundle belongs to `joyczl-memory` and this module only reads
//! pages; the meter is whatever the last turn measured. The one action — `session/compact` —
//! is human-only by construction: no tool reaches it, and the model has no route to it.

use serde::de::DeserializeOwned;
use serde_json::Value;

use joyczl_protocol::{
    codes, methods, ContextPart, ContextUsage, ContextUsageParams, ContextUsageResponse,
    ErrorObject, LedgerEntryView, LedgerTurnView, MemoryIndexResponse, MemoryLedgerParams,
    MemoryLedgerResponse, MemoryPageParams, MemoryPageResponse, MemoryPageSummary, MemoryPageView,
    SessionCompactParams, SessionCompactResponse, SkillStat, SkillStatsResponse,
};

use crate::Server;

/// `/compact` keeps the newest few turns in the prompt: "compact now" means "fold the old
/// part in", never "forget what we were just doing".
const MANUAL_KEEP_TURNS: usize = 2;

/// The ledger list default: enough to look like a history, small enough to render at once.
const LEDGER_DEFAULT_LIMIT: i32 = 30;

/// Try one of this module's methods. `None` = not ours (`dispatch` falls through).
pub(crate) async fn handle(
    server: &Server,
    method: &str,
    params: Option<&Value>,
) -> Option<Result<Value, ErrorObject>> {
    let outcome = match method {
        methods::MEMORY_LEDGER => ledger(server, params).await,
        methods::MEMORY_INDEX => index(server),
        methods::MEMORY_PAGE => page(server, params),
        methods::CONTEXT_USAGE => context(server, params),
        methods::SKILL_STATS => skill_stats(server),
        methods::SESSION_COMPACT => compact(server, params).await,
        _ => return None,
    };
    Some(outcome)
}

// ---- Ledger ---------------------------------------------------------------------------

async fn ledger(server: &Server, params: Option<&Value>) -> Result<Value, ErrorObject> {
    let p: MemoryLedgerParams = parse(params, methods::MEMORY_LEDGER)?;
    let limit = p.limit.unwrap_or(LEDGER_DEFAULT_LIMIT).clamp(1, 200) as u32;
    let turns = server
        .ledger
        .turns(&p.session_id, limit)
        .await
        .map_err(internal)?;
    let entries = match p.turn_id.as_deref() {
        Some(turn_id) => server
            .ledger
            .entries(&p.session_id, turn_id)
            .await
            .map_err(internal)?,
        None => Vec::new(),
    };
    encode(MemoryLedgerResponse {
        turns: turns
            .into_iter()
            .map(|row| LedgerTurnView {
                turn_id: row.turn_id,
                at: row.at,
                preview: row.preview,
                entries: crate::narrow(row.events),
                fidelity: row.fidelity,
                consolidated: row.consolidated,
            })
            .collect(),
        entries: entries
            .into_iter()
            .map(|row| LedgerEntryView {
                kind: row.kind,
                preview: preview_of(&row.payload),
                fidelity: row.fidelity,
                bytes: crate::narrow(row.payload.len() as i64),
            })
            .collect(),
    })
}

/// One line for a list row: the payload's `preview`/`text`/`content` when it has one,
/// otherwise the raw bytes trimmed. A ledger row must be readable without being opened.
pub(crate) fn preview_of(payload: &str) -> String {
    let preview = serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| {
            ["preview", "text", "content", "name", "query"]
                .iter()
                .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| payload.to_string());
    let first_line = preview.lines().next().unwrap_or_default().trim();
    let clipped: String = first_line.chars().take(160).collect();
    clipped
}

// ---- Memory bundle --------------------------------------------------------------------

fn index(server: &Server) -> Result<Value, ErrorObject> {
    let home = server.settings().home;
    let bundle = joyczl_memory::bundle::Bundle::new(&home);
    let pages = bundle
        .pages()
        .map_err(|e| failure(format!("记忆库读不了（{}）：{e}", bundle.root().display())))?;
    let catalog = bundle.index_text().unwrap_or_default();
    encode(MemoryIndexResponse {
        catalog,
        pages: pages
            .into_iter()
            .map(|page| MemoryPageSummary {
                slug: page.slug.clone(),
                title: page.title.clone(),
                kind: page.kind.clone(),
                summary: page.summary.clone(),
                updated: page.updated.clone(),
            })
            .collect(),
    })
}

fn page(server: &Server, params: Option<&Value>) -> Result<Value, ErrorObject> {
    let p: MemoryPageParams = parse(params, methods::MEMORY_PAGE)?;
    let home = server.settings().home;
    let bundle = joyczl_memory::bundle::Bundle::new(&home);
    let found = bundle
        .read(&p.page)
        .map_err(|e| failure(format!("记忆页读不了（{}）：{e}", bundle.root().display())))?;
    encode(MemoryPageResponse {
        page: found.map(|page| MemoryPageView {
            slug: page.slug,
            title: page.title,
            kind: page.kind,
            summary: page.summary,
            body: page.body,
            sources: page.sources,
            updated: page.updated,
        }),
    })
}

// ---- Context meter --------------------------------------------------------------------

fn context(server: &Server, params: Option<&Value>) -> Result<Value, ErrorObject> {
    let p: ContextUsageParams = parse(params, methods::CONTEXT_USAGE)?;
    let usage = server
        .context_usage
        .lock()
        .ok()
        .and_then(|map| map.get(&p.session_id).cloned());
    encode(ContextUsageResponse { usage })
}

/// Assemble one breakdown from the pieces the turn already measured.
///
/// Public inside the crate because `turn.rs` is the only place that holds all six sections
/// at once; anything else would be re-estimating from files and disagreeing with the prompt
/// the model actually received.
pub(crate) fn usage_of(
    parts: Vec<(&str, usize)>,
    budget_tokens: usize,
    warn_at: i32,
) -> ContextUsage {
    let parts: Vec<ContextPart> = parts
        .into_iter()
        .map(|(name, tokens)| ContextPart {
            name: name.to_string(),
            tokens: crate::narrow(tokens as i64),
        })
        .collect();
    let used: usize = parts.iter().map(|p| p.tokens.max(0) as usize).sum();
    let ratio = if budget_tokens == 0 {
        0.0
    } else {
        used as f64 / budget_tokens as f64
    };
    let notice = (ratio * 100.0 >= warn_at as f64).then(|| {
        format!(
            "上下文已用 {:.0}%（{} / {} token）：再长就该压缩了。",
            ratio * 100.0,
            used,
            budget_tokens
        )
    });
    ContextUsage {
        used_tokens: crate::narrow(used as i64),
        budget_tokens: crate::narrow(budget_tokens as i64),
        ratio,
        parts,
        notice,
    }
}

// ---- Skill hits -----------------------------------------------------------------------

/// Aggregate `<home>/skill-hits.jsonl`.
///
/// Per line `{"name":…,"at":…}`. A hand-edited or truncated line is skipped, not fatal:
/// this is telemetry, and one bad line must not take the panel down.
fn skill_stats(server: &Server) -> Result<Value, ErrorObject> {
    let path = server.settings().home.join("skill-hits.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut ordered: Vec<SkillStat> = Vec::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(name) = value.get("name").and_then(Value::as_str) else {
            continue;
        };
        let at = value.get("at").and_then(Value::as_str).map(str::to_string);
        match ordered.iter_mut().find(|stat| stat.name == name) {
            Some(stat) => {
                stat.hits += 1;
                stat.last_hit = at;
            }
            None => ordered.push(SkillStat {
                name: name.to_string(),
                hits: 1,
                last_hit: at,
            }),
        }
    }
    ordered.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.name.cmp(&b.name)));
    encode(SkillStatsResponse { data: ordered })
}

// ---- Manual compaction ----------------------------------------------------------------

async fn compact(server: &Server, params: Option<&Value>) -> Result<Value, ErrorObject> {
    let p: SessionCompactParams = parse(params, methods::SESSION_COMPACT)?;
    let response = compact_session(server, &p.session_id, p.focus.as_deref()).await?;
    encode(response)
}

/// Fold the older turns in now. Shared by the protocol method and the REPL's `/compact`:
/// one body, so the terminal and the drawer cannot drift apart in what "compact" means.
pub async fn compact_session(
    server: &Server,
    session_id: &str,
    focus: Option<&str>,
) -> Result<SessionCompactResponse, ErrorObject> {
    let p = SessionCompactParams {
        session_id: session_id.to_string(),
        focus: focus.map(str::to_string),
    };
    let pairs = server
        .chat
        .session_history(&p.session_id)
        .await
        .map_err(internal)?;
    let previous = server.chat.load_rollup(&p.session_id).await.ok().flatten();
    let covered = previous.as_ref().map(|(covered, _)| *covered).unwrap_or(0);
    let fresh = joyczl_memory::compaction::newly_evicted(&pairs, MANUAL_KEEP_TURNS, covered);
    if fresh.is_empty() {
        // Nothing to fold: honest answer instead of a model call that would only rewrite
        // the same summary. The dashboard shows "nothing older than the window".
        return Ok(SessionCompactResponse {
            covered_turns: covered,
            summary_tokens: previous
                .as_ref()
                .map(|(_, summary)| {
                    crate::narrow(joyczl_provider::tokens::estimate_text(summary) as i64)
                })
                .unwrap_or(0),
        });
    }
    let Some(resolved) = server.resolved() else {
        return Err(failure(
            "压缩要一次模型调用，但模型还没配好（先 model/save 或设 JOY_PROVIDER）".to_string(),
        ));
    };
    let (covered_now, summary) = joyczl_memory::compaction::roll_forward_with(
        resolved.client.as_ref(),
        &resolved.model,
        previous,
        &fresh,
        p.focus.as_deref(),
    )
    .await;
    match server
        .chat
        .save_rollup(&p.session_id, covered_now, &summary)
        .await
    {
        Ok(true) => {}
        // Blocked by the monotonic guard: a newer summary already exists. Say so — silently
        // returning the old coverage would look like the button did nothing.
        Ok(false) => {
            let why = format!(
                "压缩结果没写进去：库里已有一份覆盖更多轮次的摘要（并发压缩）。当前覆盖 {covered} 轮"
            );
            return Err(failure(why));
        }
        Err(e) => return Err(internal(format!("摘要落库失败：{e}"))),
    }
    Ok(SessionCompactResponse {
        covered_turns: covered_now,
        summary_tokens: crate::narrow(joyczl_provider::tokens::estimate_text(&summary) as i64),
    })
}

// ---- Small helpers (dispatch's are private to that module) ------------------------------

fn parse<T: DeserializeOwned>(params: Option<&Value>, method: &str) -> Result<T, ErrorObject> {
    let value = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    serde_json::from_value(value).map_err(|e| ErrorObject {
        code: codes::INVALID_PARAMS,
        message: format!("{method} 的参数不对：{e}"),
        data: None,
    })
}

fn encode<T: serde::Serialize>(value: T) -> Result<Value, ErrorObject> {
    serde_json::to_value(value).map_err(internal)
}

fn internal(e: impl std::fmt::Display) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message: e.to_string(),
        data: None,
    }
}

/// A read that failed for a reason the user can act on (path, permission, config).
fn failure(message: String) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message,
        data: None,
    }
}
