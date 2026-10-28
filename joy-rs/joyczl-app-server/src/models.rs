//! 多套模型配置的**运行时**：`model/profiles` / `model/save` / `model/remove`。
//!
//! 存储与凭据后端在 `joyczl-config` 的 `models.rs`；这里只管三件事：校验、
//! 套到当前 Settings、重解析 provider。**改完立刻生效**，不用重启 —— 这正是
//! 「多模型切换」的意义。
//!
//! 一条纪律：**这些方法只有人能调**。模型没有对应工具，也拿不到密钥 ——
//! 它若能让 Joy 自己换 provider，就等于能把自己接到别的服务器上。

use joyczl_config::models as cfg;
use joyczl_protocol::{
    ApprovalLogEntry, ApprovalLogResponse, ModelProfileView, ModelProfilesResponse,
    ModelRemoveParams, ModelRemoveResponse, ModelSaveParams, ModelSaveResponse, UsageByModel,
    UsageSummaryResponse,
};
use std::sync::{Mutex, RwLock};

use crate::Server;

/// Server 持有的两份状态：配置本身 + 凭据后端。
pub(crate) type Models = Arc<RwLock<cfg::ModelsFile>>;
pub(crate) type Secrets = Arc<Mutex<cfg::SecretStore>>;

use std::sync::Arc;

pub(crate) fn new_models(file: cfg::ModelsFile) -> Models {
    Arc::new(RwLock::new(file))
}

pub(crate) fn new_secrets() -> Secrets {
    Arc::new(Mutex::new(cfg::SecretStore::auto()))
}

/// 启动时套用：`<home>/models.json` 里若配了当前那套，它就覆盖环境变量给的
/// provider/model —— 界面上选过的，比启动时手敲的环境变量更晚、说话更响。
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
            // 只给指针，不给密钥本身。
            key_source: p.key_source.clone(),
            active: active == Some(p.id.as_str()),
        })
        .collect()
}

/// `provider / model`：界面上「当前是哪个模型」的一句话。
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

/// 补丁字段：给了就用（空串 = 清除），没给就沿用原来的。
fn pick(incoming: Option<String>, existing: Option<String>) -> Option<String> {
    match incoming {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(value),
        None => existing,
    }
}

/// provider 必须是 `joyczl-provider` 表里的一家：表本身就是「预适配主流厂商」
/// 的清单，不在表里的 id 打不出请求（除非另给 base_url）。
fn validate_provider(provider: &str, base_url: Option<&str>) -> Result<(), String> {
    if joyczl_provider::PROVIDERS.iter().any(|p| p.id == provider) {
        return Ok(());
    }
    if base_url.is_some() {
        // 自建/代理端点：provider 只当「用哪种 wire 格式」使。
        return Ok(());
    }
    let known: Vec<&str> = joyczl_provider::PROVIDERS.iter().map(|p| p.id).collect();
    Err(format!(
        "不认识的 provider '{provider}'。可用的：{}；走自建端点就同时给 base_url。",
        known.join("、")
    ))
}

impl Server {
    /// 审批审计（迁移 0011）：最近 100 条「问了才跑」的记录，新的在前。
    /// 读失败返回空列表并说明 —— 同 usage_summary 的口径。
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

    /// 用量汇总（迁移 0010）：per-model 聚合 + 总数。只记账不强制 —— 没有配额、
    /// 没有限速；BYOK 模式下钱记在用户自己的厂商账单上。hub 模式下这就是
    /// per-user 记账（每个用户一个 home 一个台账）。读失败返回空汇总并说明
    /// —— 报错会把一个「看数字」的方法变成「坏一整轮」的方法。
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

    /// 新增/改一套；`make_active`（默认 true）时顺带切过去。只带 `id` +
    /// `make_active` 就是「切换」—— 不该逼用户为了切换重发一次密钥。
    pub fn model_save(&self, p: ModelSaveParams) -> Result<ModelSaveResponse, String> {
        let id = p.id.trim().to_string();
        if id.is_empty() {
            return Err("id 不能为空（给它起个短名，比如 ds、公司-openai）".to_string());
        }

        let mut file = self.models.write().expect("models 锁不该中毒");
        let existing = cfg::find(&file, &id).cloned();

        // provider：新建必填，改已有时不给就沿用。
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

        // 凭据：给了就存（优先钥匙串），没给就保留原来的指针。
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

        // 补丁语义：没给的字段保留原来的（与 config/write 同一个约定），
        // 给空串才是清除。否则「只带 id 切一下」会把模型、端点全洗掉。
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

        // 落盘失败就不动内存 —— 「改了但不记得」比「没改成」更撒谎。
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
            // 凭据跟着走：留一个没人引用的密钥在钥匙串里是垃圾。
            //
            // 这把锁必须**在 reapply 之前放开** —— reapply 自己也要锁
            // secrets，而 std Mutex 不可重入（同一个线程再锁一次就是死锁）。
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

    /// 把当前那套套进 Settings 并**重解析** provider：切换是立刻生效的。
    /// 解析失败不致命 —— 记下原因，turn 会把「缺 key」说清楚。
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
        label
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
