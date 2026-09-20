//! 确定性用例的形状。一份用例 = 脚本化的模型应答 + 对 harness 行为的断言。
//!
//! 单轮用例直接写 `message` / `gate` / `responses`；跨轮行为（滑窗、折叠、
//! consolidation、config 生效、session 隔离）用 `turns` 数组，每轮一段脚本。

use joyczl_provider::mock::Mock;
use joyczl_provider::CreateResponse;
use serde::Deserialize;
use serde_json::Value;

use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// 单轮便捷字段：与 `turns` 二选一。
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub gate: Option<Value>,
    #[serde(default)]
    pub responses: Vec<ScriptedResponse>,
    /// 单轮便捷字段：收到第 N 个流式增量后打断。
    #[serde(default)]
    pub interrupt_after_deltas: Option<usize>,
    /// 多轮：每轮一段脚本（gate / responses / 会话 id / 打断 / 配置补丁）。
    #[serde(default)]
    pub turns: Option<Vec<TurnScript>>,
    #[serde(default)]
    pub expect: Expectations,
    /// 覆盖若干 Settings 旋钮（滑窗、consolidate 频率……）。
    #[serde(default)]
    pub settings: SettingsOverrides,
    /// 开跑前写进 home 的文件（相对路径）—— mcp.json 与假服务器用。
    #[serde(default)]
    pub files: Option<std::collections::BTreeMap<String, String>>,
    /// 前置依赖缺失时跳过本用例（目前仅认 "python3"）。跳过不算失败。
    #[serde(default)]
    pub prereq: Option<String>,
}

/// 多轮中一轮的脚本。
#[derive(Debug, Clone, Deserialize)]
pub struct TurnScript {
    pub message: String,
    /// 会话标签。不填 = "eval"。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 检索门的脚本。不给 = 门收到非 JSON（顺手测失败开放）。
    #[serde(default)]
    pub gate: Option<Value>,
    #[serde(default)]
    pub responses: Vec<ScriptedResponse>,
    /// 本轮结束后写入的 `config/write` 补丁（验证配置热生效）。
    #[serde(default)]
    pub apply_patch: Option<Value>,
    /// 收到第 N 个流式增量后打断这一轮（验证 turn/interrupt）。
    #[serde(default)]
    pub interrupt_after_deltas: Option<usize>,
}

/// Settings 旋钮的用例级覆盖。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsOverrides {
    #[serde(default)]
    pub history_turns: Option<i32>,
    #[serde(default)]
    pub consolidate_every: Option<i32>,
    #[serde(default)]
    pub retrieval_top_k: Option<i32>,
    #[serde(default)]
    pub max_iterations: Option<i32>,
    #[serde(default)]
    pub graph_workflows: Option<bool>,
}

impl Scenario {
    /// 展开成逐轮脚本：`turns` 优先；否则从单轮字段合成一轮。
    pub fn effective_turns(&self) -> Vec<TurnScript> {
        if let Some(turns) = &self.turns {
            return turns.clone();
        }
        vec![TurnScript {
            message: self.message.clone().unwrap_or_default(),
            session_id: None,
            gate: self.gate.clone(),
            responses: self.responses.clone(),
            apply_patch: None,
            interrupt_after_deltas: self.interrupt_after_deltas,
        }]
    }

    /// mock 应答队列 = 各轮脚本按执行顺序展平（gate 先、应答后）。
    /// consolidation 之类的额外模型调用也按时间序插在队列里 ——
    /// 用例作者按 run_turn 的真实顺序排即可。统一流式：interrupt 用例
    /// 依赖增量，其余用例不受影响（断言看的是拼起来的完整文本）。
    /// gate 缺省时自动补一句非 JSON 的应答 —— 门失败开放，且不吞
    /// loop 的应答（这正是缺省语义："懒得写 gate 就测失败开放"）。
    pub fn build_mock(&self) -> Mock {
        let mut responses: Vec<CreateResponse> = Vec::new();
        for turn in self.effective_turns() {
            let gate = match &turn.gate {
                Some(gate) => serde_json::to_string(gate).expect("gate 序列化"),
                None => "（场景没给 gate 脚本 —— 门会失败开放）".to_string(),
            };
            responses.push(Mock::text(&gate));
            for (index, response) in turn.responses.iter().enumerate() {
                responses.push(response.to_create_response(index));
            }
        }
        Mock::streaming(responses)
    }
}

/// 模型的下一句：要么纯文本，要么一次工具调用。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedResponse {
    Text(String),
    ToolUse { name: String, args: Value },
}

impl ScriptedResponse {
    pub(crate) fn to_create_response(&self, index: usize) -> CreateResponse {
        match self {
            ScriptedResponse::Text(text) => Mock::text(text),
            ScriptedResponse::ToolUse { name, args } => {
                Mock::tool_use(&format!("tu_{index}"), name, args.clone())
            }
        }
    }
}

/// 断言集。全是「应包含 / 不应包含 / 序列相等」级别的检查 ——
/// 确定性 eval 的每个判定都必须可复现，不靠另一个模型。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expectations {
    #[serde(default)]
    pub reply_contains: Vec<String>,
    #[serde(default)]
    pub reply_not_contains: Vec<String>,
    /// 工具名的**有序**序列（空 vec = 不检查；多轮时按轮累计）。
    #[serde(default)]
    pub tools_used: Vec<String>,
    /// 任一工具输出必须包含的片段 —— 区分「调了但失败」与「真跑通了」。
    #[serde(default)]
    pub tool_output_contains: Vec<String>,
    /// `"retrieve"` 或 `"skip"`（每轮都查；任一轮不符即失败）。
    #[serde(default)]
    pub gate: Option<String>,
    #[serde(default)]
    pub iterations: Option<i32>,
    /// 发出的 consolidation 通知里的新事实总数。
    #[serde(default)]
    pub consolidation_new_facts: Option<i32>,
    /// 最后一轮被打断过。
    #[serde(default)]
    pub interrupted: Option<bool>,
    // ---- prompt 断言（harness 到底给模型看了什么）----
    /// 所有模型请求的全文里出现过。
    #[serde(default)]
    pub prompt_contains: Vec<String>,
    /// 所有模型请求的全文里都不出现。
    #[serde(default)]
    pub prompt_not_contains: Vec<String>,
    /// **最后一个**模型请求里出现过（查"最新一轮看见了什么"）。
    #[serde(default)]
    pub last_prompt_contains: Vec<String>,
    /// 最后一个模型请求里不出现。
    #[serde(default)]
    pub last_prompt_not_contains: Vec<String>,
}

/// judge 用例：真模型答一轮，裁判按 criteria 打分。没有脚本应答 ——
/// 被考的就是真的 Joy。
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScenario {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub message: String,
    pub judge: JudgeSpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeSpec {
    /// 这条用例最看重什么（裁判 rubric 里的定制段）。
    pub criteria: String,
}

/// 读 judge 用例（与确定性用例同一份 JSONL 形状，目录/文件都认）。
pub fn load_judge(paths: &[PathBuf]) -> anyhow::Result<Vec<JudgeScenario>> {
    let mut scenarios = Vec::new();
    for path in paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![path.clone()]
        };
        for file in files {
            let text = std::fs::read_to_string(&file)?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                scenarios.push(serde_json::from_str(line)?);
            }
        }
    }
    Ok(scenarios)
}

/// 读全部确定性用例：每个路径是 JSONL 文件或目录（目录则吃它下面的 *.jsonl）。
pub fn load_all(paths: &[PathBuf]) -> anyhow::Result<Vec<Scenario>> {
    let mut scenarios = Vec::new();
    for path in paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![path.clone()]
        };
        for file in files {
            let text = std::fs::read_to_string(&file)?;
            for (line_number, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let scenario: Scenario = serde_json::from_str(line).map_err(|e| {
                    anyhow::anyhow!(
                        "{} 第 {} 行不是合法用例：{e}",
                        file.display(),
                        line_number + 1
                    )
                })?;
                scenarios.push(scenario);
            }
        }
    }
    Ok(scenarios)
}
