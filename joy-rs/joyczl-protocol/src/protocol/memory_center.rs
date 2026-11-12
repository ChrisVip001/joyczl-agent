//! The memory center's protocol: what the dashboard reads and what the human can trigger.
//!
//! These are **read surfaces plus one human-only action**. The model has no route here:
//! the ledger is append-only, the memory pages belong to the bundle, and compaction is a
//! decision a person makes when they can see the meter. Kept in its own module so `v2.rs`
//! stays the wire's core and this stays the window onto it.

use serde::{Deserialize, Serialize};

use crate::{JsonSchema, TS};

/// One ledger row as the dashboard shows it: kind, a short preview, and whether the full
/// bytes still exist (`exact`) or only a folded summary came through the backfill.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LedgerEntryView {
    pub kind: String,
    /// First line(s) of the payload, already trimmed for a list row.
    pub preview: String,
    /// `exact` | `folded` — a folded entry is honest about being a summary.
    pub fidelity: String,
    pub bytes: i32,
}

/// One turn in the ledger list: what happened, and how much of it is still recoverable.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LedgerTurnView {
    pub turn_id: String,
    pub at: String,
    /// One line: the user's first sentence.
    pub preview: String,
    pub entries: i32,
    /// `exact` when every row is exact; `folded` when the backfill touched it.
    pub fidelity: String,
    pub consolidated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryLedgerParams {
    pub session_id: String,
    /// Newest-first cap. Defaults to 30.
    pub limit: Option<i32>,
    /// Present = also return this turn's entries.
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryLedgerResponse {
    pub turns: Vec<LedgerTurnView>,
    /// Empty unless `turnId` was asked for.
    pub entries: Vec<LedgerEntryView>,
}

/// One row of the catalog (`index.md`): enough to choose a page, not enough to replace it.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryPageSummary {
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub summary: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryIndexResponse {
    /// The generated catalog, byte-for-byte the text the prompt carries.
    pub catalog: String,
    pub pages: Vec<MemoryPageSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryPageParams {
    /// Page slug, no `.md` (`alex`, not `alex.md`).
    pub page: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryPageView {
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub summary: String,
    pub body: String,
    /// Provenance: the ledger rows this page was distilled from (`ledger:12345`).
    pub sources: Vec<String>,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryPageResponse {
    /// `None` = no such page (the dashboard shows the path and says so, not an empty page).
    pub page: Option<MemoryPageView>,
}

/// One section of the context: soul, memory, skills, tools, history, summary.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextPart {
    pub name: String,
    pub tokens: i32,
}

/// How the window is spent this turn. `ratio` is `used / budget`, already computed: the
/// frontend must not have to guess which denominator was meant.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextUsage {
    pub used_tokens: i32,
    pub budget_tokens: i32,
    pub ratio: f64,
    pub parts: Vec<ContextPart>,
    /// Set once `ratio` crosses `JOY_CONTEXT_WARN_RATIO`: one honest sentence, no dialog.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextUsageParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextUsageResponse {
    /// `None` = no turn has run in this session yet.
    pub usage: Option<ContextUsage>,
}

/// One skill's turn count: how often its body actually entered a prompt.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillStat {
    pub name: String,
    pub hits: i32,
    pub last_hit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillStatsResponse {
    pub data: Vec<SkillStat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionCompactParams {
    pub session_id: String,
    /// What to keep in the summary ("the migration plan"), optional.
    pub focus: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionCompactResponse {
    /// How many turns the new summary covers.
    pub covered_turns: i32,
    pub summary_tokens: i32,
}
