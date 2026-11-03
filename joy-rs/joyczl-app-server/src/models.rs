//! The **runtime** of multiple model profiles: `model/profiles` / `model/save` / `model/remove`.
//!
//! Storage and the credentials backend live in `joyczl-config`'s models.rs; this module handles three
//! things: validation, applying to the current Settings, and re-resolving the provider. **Takes effect
//! immediately**, no restart — that is the whole point of "multi-model switching".
//!
//! One discipline: **only a human can call these methods**. The model has no corresponding tools and
//! cannot reach the keys — if it could make Joy switch providers itself, it could plug itself into another server.

use joyczl_config::models as cfg;
use joyczl_protocol::{
    ApprovalLogEntry, ApprovalLogResponse, ModelProfileView, ModelProfilesResponse,
    ModelRemoveParams, ModelRemoveResponse, ModelSaveParams, ModelSaveResponse,
    ScheduleListResponse, ScheduleStateRow, UsageByModel, UsageSummaryResponse,
};
use std::sync::{Mutex, RwLock};

use crate::Server;

/// The two pieces of state the Server holds: the config itself + the credentials backend.
pub(crate) type Models = Arc<RwLock<cfg::ModelsFile>>;
pub(crate) type Secrets = Arc<Mutex<cfg::SecretStore>>;

use std::sync::Arc;

pub(crate) fn new_models(file: cfg::ModelsFile) -> Models {
    Arc::new(RwLock::new(file))
}

pub(crate) fn new_secrets() -> Secrets {
    Arc::new(Mutex::new(cfg::SecretStore::auto()))
}

/// Apply at boot: if the currently active profile is configured in `<home>/models.json`, it overrides the
/// provider/model from env vars — what was picked in the UI speaks later and louder than startup env vars.
pub(crate) fn apply_at_boot(settings: &mut joyczl_config::Settings, secrets: &Secrets) {
    let file = cfg::load_models(&settings.home);
    let home = settings.home.clone();
    let mut store = secrets.lock().expect("secrets 锁不该中毒");
    cfg::apply_active(&file, &home, &mut store, settings);
}

fn views(file: &cfg::ModelsFile) -> Vec<ModelProfileView> {
    let active = file.active.as_deref();
    file.profiles
        .iter()
        .map(|p| ModelProfileView {
            id: p.id.clone(),
            provider: p.provider.clone(),
            model: p.model.clone(),
            small_model: p.small_model.clone(),
            base_url: p.base_url.clone(),
            // Only the pointer, never the key itself.
            key_source: p.key_source.clone(),
            active: active == Some(p.id.as_str()),
        })
        .collect()
}

/// `provider / model`: the one-liner for "which model is current" in the UI.
fn current_label(settings: &joyczl_config::Settings) -> Option<String> {
    let provider = &settings.provider;
    if provider.is_empty() {
        return None;
    }
    Some(match settings.model.as_deref() {
        Some(model) => format!("{provider} / {model}"),
        None => provider.to_string(),
    })
}

/// Patch semantics for fields: given means use it (empty string = clear), absent means keep the old.
fn pick(incoming: Option<String>, existing: Option<String>) -> Option<String> {
    match incoming {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(value),
        None => existing,
    }
}

/// The provider must be one of `joyczl-provider`'s table: the table itself is the list of
/// "pre-adapted mainstream vendors"; an id outside the table cannot produce a request (unless base_url is given).
fn validate_provider(provider: &str, base_url: Option<&str>) -> Result<(), String> {
    if joyczl_provider::PROVIDERS.iter().any(|p| p.id == provider) {
        return Ok(());
    }
    if base_url.is_some() {
        // Self-hosted / proxy endpoint: the provider is only "which wire format to use".
        return Ok(());
    }
    let known: Vec<&str> = joyczl_provider::PROVIDERS.iter().map(|p| p.id).collect();
    Err(format!(
        "不认识的 provider '{provider}'。可用的：{}；走自建端点就同时给 base_url。",
        known.join("、")
    ))
}

impl Server {
    /// Schedule state snapshot (migration 0012): all tasks registered by `joy schedule`.
    /// The scheduler is a separate process; this is only a read-only view of the same table; a read
    /// failure returns an empty list with a note (a panel must not turn into a broken page over one read error).
    pub async fn schedule_list(&self) -> ScheduleListResponse {
        let schedules = joyczl_state::Schedules::new(self.pool.clone());
        match schedules.list().await {
            Ok(rows) => ScheduleListResponse {
                jobs: rows
                    .into_iter()
                    .map(|row| ScheduleStateRow {
                        name: row.name,
                        last_fired_minute: row.last_fired_minute,
                        claimed_minute: row.claimed_minute,
                        catch_up: row.catch_up,
                        delivery: row.delivery,
                    })
                    .collect(),
            },
            Err(e) => {
                eprintln!("(joy) 调度状态读不了：{e}");
                ScheduleListResponse { jobs: Vec::new() }
            }
        }
    }

    /// Approval audit (migration 0011): the last 100 "asked before running" records, newest first.
    /// A read failure returns an empty list with a note — the same stance as usage_summary.
    pub async fn approval_log(&self) -> ApprovalLogResponse {
        match self.approval_log.recent(100).await {
            Ok(rows) => ApprovalLogResponse {
                entries: rows
                    .into_iter()
                    .map(|row| ApprovalLogEntry {
                        turn_id: row.turn_id,
                        request_id: row.request_id,
                        tool: row.tool,
                        args_preview: row.args_preview,
                        reason: row.reason,
                        outcome: row.outcome,
                        decided_at: row.decided_at,
                    })
                    .collect(),
            },
            Err(e) => {
                eprintln!("(joy) 审批审计读不了：{e}");
                ApprovalLogResponse {
                    entries: Vec::new(),
                }
            }
        }
    }

    /// Usage summary (migration 0010): per-model aggregation + totals. Bookkeeping only, never enforced —
    /// no quota, no rate limit; in BYOK mode the money sits on the user's own vendor bill. In hub mode this
    /// is per-user accounting (one home, one ledger per user). A read failure returns an empty summary with
    /// a note — an error would turn a "look at numbers" method into a "break a whole turn" method.
    pub async fn usage_summary(&self) -> UsageSummaryResponse {
        match self.usage.summary().await {
            Ok(summary) => UsageSummaryResponse {
                turns: summary.turns,
                input_tokens: summary.input_tokens,
                output_tokens: summary.output_tokens,
                by_model: summary
                    .by_model
                    .into_iter()
                    .map(|row| UsageByModel {
                        provider: row.provider,
                        model: row.model,
                        turns: row.turns,
                        input_tokens: row.input_tokens,
                        output_tokens: row.output_tokens,
                    })
                    .collect(),
            },
            Err(e) => {
                eprintln!("(joy) 用量汇总读不了：{e}");
                UsageSummaryResponse {
                    turns: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    by_model: Vec::new(),
                }
            }
        }
    }

    pub fn model_profiles(&self) -> ModelProfilesResponse {
        let file = self.models.read().expect("models 锁不该中毒");
        ModelProfilesResponse {
            active_id: file.active.clone(),
            profiles: views(&file),
        }
    }

    /// Add/update a profile; when `make_active` (default true), switch to it as well. Sending only `id` +
    /// `make_active` means "switch" — the user must not be forced to re-send a key just to switch.
    pub fn model_save(&self, p: ModelSaveParams) -> Result<ModelSaveResponse, String> {
        let id = p.id.trim().to_string();
        if id.is_empty() {
            return Err("id 不能为空（给它起个短名，比如 ds、公司-openai）".to_string());
        }

        let mut file = self.models.write().expect("models 锁不该中毒");
        let existing = cfg::find(&file, &id).cloned();

        // provider: required when creating; when editing, absent means keep the old.
        let provider = match (p.provider.as_deref(), existing.as_ref()) {
            (Some(v), _) => v.trim().to_string(),
            (None, Some(e)) => e.provider.clone(),
            (None, None) => {
                return Err(format!(
                    "第一次配置 '{id}' 要给 provider（model/list 里有各家的 id）"
                ))
            }
        };
        validate_provider(&provider, p.base_url.as_deref())?;

        // Credentials: if given, store them (keychain first); if absent, keep the old pointer.
        let home = self.settings().home.clone();
        let key_stored_in = match (p.api_key.as_deref(), p.key_env.as_deref()) {
            (Some(key), _) if !key.trim().is_empty() => {
                let mut store = self.secrets.lock().expect("secrets 锁不该中毒");
                store
                    .put(&home, &id, key.trim())
                    .map_err(|e| format!("凭据存不进去：{e}"))?
                    .to_string()
            }
            (_, Some(var)) if !var.trim().is_empty() => format!("env:{}", var.trim()),
            _ => existing
                .as_ref()
                .and_then(|e| e.key_source.clone())
                .unwrap_or_else(|| "none".to_string()),
        };
        let key_source =
            (!key_stored_in.is_empty() && key_stored_in != "none").then(|| key_stored_in.clone());

        // Patch semantics: absent fields keep the old values (same convention as config/write);
        // an empty string is what clears. Otherwise "send just the id to switch" would wash away model and endpoint.
        let existing_fields = existing.unwrap_or(cfg::ModelProfile {
            id: id.clone(),
            provider: provider.clone(),
            model: None,
            small_model: None,
            base_url: None,
            key_source: None,
        });
        cfg::upsert(
            &mut file,
            cfg::ModelProfile {
                id: id.clone(),
                provider,
                model: pick(p.model, existing_fields.model),
                small_model: pick(p.small_model, existing_fields.small_model),
                base_url: pick(p.base_url, existing_fields.base_url),
                key_source,
            },
        );

        if p.make_active.unwrap_or(true) {
            file.active = Some(id.clone());
        }
        let active = file.active.as_deref() == Some(id.as_str());

        // If persisting fails, memory is left untouched — "changed but not remembered" lies harder than "not changed".
        cfg::save_models(&home, &file).map_err(|e| format!("写 models.json 失败：{e}"))?;
        let current = self.reapply(&file);
        drop(file);

        Ok(ModelSaveResponse {
            id,
            active,
            active_id: self
                .models
                .read()
                .expect("models 锁不该中毒")
                .active
                .clone(),
            key_stored_in,
            current,
        })
    }

    pub fn model_remove(&self, p: ModelRemoveParams) -> ModelRemoveResponse {
        let home = self.settings().home.clone();
        let mut file = self.models.write().expect("models 锁不该中毒");
        let removed = cfg::remove(&mut file, &p.id);
        if removed {
            // Credentials go with it: leaving an unreferenced key in the keychain is garbage.
            //
            // This lock must be **released before reapply** — reapply itself locks
            // secrets too, and std Mutex is not reentrant (locking it again on the same thread deadlocks).
            {
                let mut store = self.secrets.lock().expect("secrets 锁不该中毒");
                store.remove(&home, &p.id);
            }
            let _ = cfg::save_models(&home, &file);
            self.reapply(&file);
        }
        let active_id = file.active.clone();
        drop(file);
        ModelRemoveResponse { removed, active_id }
    }

    /// Apply the active profile into Settings and **re-resolve** the provider: switching takes effect immediately.
    /// A resolution failure is not fatal — record the reason; the turn will spell out "missing key".
    fn reapply(&self, file: &cfg::ModelsFile) -> Option<String> {
        let mut settings = self.settings();
        {
            let home = settings.home.clone();
            let mut store = self.secrets.lock().expect("secrets 锁不该中毒");
            cfg::apply_active(file, &home, &mut store, &mut settings);
        }
        let resolved = match joyczl_provider::resolve(&settings) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                eprintln!("(joy) 切换模型后还没配好，turn/* 暂不可用 —— {reason}");
                None
            }
        };
        let label = current_label(&settings);
        self.set_settings(settings);
        self.set_resolved(resolved);
        // The profile was just edited/deleted: all tier-cached resolutions are invalidated; re-resolve next turn.
        self.route_cache
            .lock()
            .expect("route 缓存锁不该中毒")
            .clear();
        label
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
