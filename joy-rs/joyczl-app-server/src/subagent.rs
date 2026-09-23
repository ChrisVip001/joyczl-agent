//! 子代理的执行体。
//!
//! 放在这一层是因为它要模型、状态句柄与工具表 —— `joyczl-tools` 那边只认得
//! `SubagentRunner` 这个 trait（依赖方向不能反过来：loop 依赖 tools）。
//!
//! 与父轮的区别，逐条说清：
//!
//! * **自己的上下文**：空历史 + 一份简短 system（人格 + 一句「你只干这一件事」），
//!   不带检索、不带技能、不带摘要。派出去的活应当自成一体。
//! * **独立的迭代上限**：默认 5、硬上限 10。给到几十轮就等于在别处偷偷跑一整轮
//!   长对话，而「省上下文」正是委派的理由。
//! * **不能再派生**：工具表显式去掉 `delegate_task`（递归派生是那种平时看不出来、
//!   一旦有人试就烧穿配额的东西）。
//! * **没有通知、没有打断**：父轮只知道「在调用 delegate_task」，然后拿到结论。
//! * **不留档**：子代理的对话不写 `chat_log`（否则 `session/list` 里会多出一堆
//!   没人认领的会话）。要知道它干了什么，看返回的摘要 —— 里面带着它用过的工具。
//!
//! 模型与设置是**运行时现读**的（共享那两个 Arc，而不是在构造时clone 一份）：
//! `config/write` 换 provider 之后、或测试里 `install_provider` 之后进来的这一轮，
//! 用的必须是**当下**的 client —— 捕获启动那一刻的副本是个静默的坑。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use joyczl_config::Settings;
use joyczl_provider::Resolved;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use joyczl_tools::{subagent::SubagentRunner, BoxFut, ToolRegistry};
use serde_json::Value;

/// 默认迭代上限：委派出去的活应当比一轮对话小。
const DEFAULT_ITERATIONS: i32 = 5;
/// 硬上限：再多就不叫「委派」了。
const MAX_ITERATIONS: i32 = 10;
/// 子代理的答案不该占满一整轮的额度：它给的是结论。
const SUBAGENT_MAX_TOKENS: i32 = 2048;

pub(crate) struct Delegated {
    /// 父轮工具表的副本（其中已经没有 `delegate_task`）。
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) facts: Facts,
    pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,
    pub(crate) calendar: Calendar,
    /// 与 `Server` 共享的设置与模型句柄（运行时现读，见模块文档）。
    pub(crate) settings: Arc<RwLock<Settings>>,
    pub(crate) resolved: Arc<RwLock<Option<Resolved>>>,
    /// 子代理的工具调用也走钩子（工具级事件由 `ToolRegistry::execute` 发）。
    pub(crate) hooks: Option<Arc<joyczl_tools::hooks::Hooks>>,
}

/// 这一次要喂给子代理的东西。
struct Prompt {
    system: String,
    history: Vec<joyczl_provider::Message>,
    user_message: String,
}

/// 跑一次子代理的 loop。抽出来是因为「结构化结果不合规」要**再跑一轮**，
/// 而那一轮除了历史与指令之外和第一轮完全一样。
async fn run_child(
    resolved: &Resolved,
    tools: &ToolRegistry,
    ctx: joyczl_tools::ToolCtx,
    prompt: Prompt,
    max_iterations: i32,
    budget: joyczl_loop::budget::ToolResultBudget,
) -> anyhow::Result<joyczl_loop::LoopResult> {
    joyczl_loop::run(joyczl_loop::Turn {
        client: resolved.client.as_ref(),
        model: &resolved.model,
        system: prompt.system,
        history: prompt.history,
        user_message: prompt.user_message,
        tools,
        ctx,
        max_iterations,
        max_tokens: SUBAGENT_MAX_TOKENS,
        tool_result_budget: budget,
        observer: None,
        on_text: None,
        interrupt: None,
    })
    .await
}

/// 把子代理的结论压成「符合 schema 的 JSON 字符串」。
fn coerce_structured(reply: &str, schema: &Value) -> Result<String, String> {
    let Some(json) = joyczl_memory::gate::extract_json(reply) else {
        return Err("回复里没找到 JSON 对象".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&json) else {
        return Err("那段 JSON 解析不了".to_string());
    };
    joyczl_tools::validate_value(schema, &value)?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

impl SubagentRunner for Delegated {
    fn run(
        &self,
        task: String,
        max_iterations: Option<i32>,
        result_schema: Option<serde_json::Value>,
    ) -> BoxFut {
        let tools = self.tools.clone();
        let settings = self.settings.clone();
        let resolved = self.resolved.clone();
        let hooks = self.hooks.clone();
        let facts = self.facts.clone();
        let episodes = self.episodes.clone();
        let chat = self.chat.clone();
        let calendar = self.calendar.clone();

        Box::pin(async move {
            // 现读：这一轮用的是当下的 provider 与设置。
            let Some(resolved) = resolved.read().expect("resolved 锁不该中毒").clone() else {
                return Err(anyhow::anyhow!("模型还没配好，子代理起不来"));
            };
            let settings = settings.read().expect("settings 锁不该中毒").clone();
            let home: PathBuf = settings.home.clone();

            let iterations = max_iterations
                .unwrap_or(DEFAULT_ITERATIONS)
                .clamp(1, MAX_ITERATIONS);
            // 事件载荷用任务的前一小段：整段可能很长，而钩子要的是「在干什么」。
            let task_preview: String = task.chars().take(400).collect();

            let soul = crate::turn::load_soul(&home);
            let system = crate::turn::build_system(
                &soul,
                &resolved.model,
                &resolved.provider_id,
                "",
                "",
                // 子代理没有待办清单：那张表属于父轮的任务线，它也不该去写。
                None,
                None,
            ) + "\n\nYou are a subagent: do the one task below and answer with the \
                 conclusion only. Whoever asked cannot see your steps, and you cannot ask \
                 follow-up questions.";

            // 工具表用**父轮的副本**，只是其中没有 delegate_task —— 于是递归
            // 派生不是「被拒绝」，而是根本不存在的选项。
            // 子代理的表里既没有 `delegate_task`（递归结构性不存在），
            // 也没有 `todo_write`：清单属于**父轮那条任务线**，子代理写它会
            // 把父轮的计划表覆盖掉。
            let child_tools = tools
                .without(joyczl_tools::subagent::NAME)
                .without("todo_write");
            // 子代理**没有批准通道**（`None`）：它不该阻塞在人类身上，所以
            // 需要批准的动作在它那儿直接按拒绝处理 —— 见 limitations.md。
            //
            // 会话名给一个自己的：日志与 spill 归属查得到「这是谁弄出来的」。
            let child_session = format!(
                "subagent:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let ctx = crate::tool_ctx(
                &facts,
                &episodes,
                &chat,
                &calendar,
                &home,
                crate::Injections {
                    session_id: child_session.clone(),
                    // 子代理没有批准通道（它不该阻塞在人类身上），但有钩子：工具级
                    // 事件照常走（用子代理自己的 session 名）。
                    approval: None,
                    hooks: hooks.clone(),
                },
            );

            let budget = crate::tool_result_budget(&settings);

            // SubagentStart：派出去的活也要显形（审计与「谁在动我的东西」）。
            // 观察事件 —— 钩子的话不会拦住这次委派。
            crate::fire_hook(
                &hooks,
                joyczl_tools::hooks::HookEvent::SubagentStart,
                serde_json::json!({
                    "session_id": child_session,
                    "task": task_preview,
                    "max_iterations": iterations,
                }),
            )
            .await;

            let result = run_child(
                &resolved,
                &child_tools,
                ctx.clone(),
                Prompt {
                    system: system.clone(),
                    // 空历史：委派的意义就是把上下文的重担留在父轮那边。
                    history: Vec::new(),
                    user_message: task,
                },
                iterations,
                budget,
            )
            .await
            .map_err(|e| anyhow::anyhow!("模型调用失败：{e}"))?;

            crate::fire_hook(
                &hooks,
                joyczl_tools::hooks::HookEvent::SubagentStop,
                serde_json::json!({
                    "session_id": child_session,
                    "iterations": result.iterations,
                    "interrupted": result.interrupted,
                }),
            )
            .await;

            let mut summary = result.reply.trim().to_string();

            // ---- 结构化结果：父模型给了 schema 就要求它是合法 JSON 且过校验。
            // 不合规**带着报错重试一次**（把上一轮的 messages 也带上，它看得见自己
            // 写了什么），仍不合规就回落成散文并写明原因 —— 一次没按格式回话，
            // 不该让整件活白跑（与「失败开放」同一条）。
            if let Some(schema) = &result_schema {
                match coerce_structured(&summary, schema) {
                    Ok(json) => summary = json,
                    Err(why) => {
                        let nudge = format!(
                            "上一次的结论不能用作结构化结果：{why}\n\
                             请**只**回复符合下面 schema 的 JSON（不要代码块、不要多余解释）：\n{schema}"
                        );
                        let retry = run_child(
                            &resolved,
                            &child_tools,
                            ctx,
                            Prompt {
                                system,
                                history: result.messages.clone(),
                                user_message: nudge,
                            },
                            iterations,
                            budget,
                        )
                        .await;
                        match retry {
                            Ok(second) => match coerce_structured(second.reply.trim(), schema) {
                                Ok(json) => {
                                    eprintln!("(joy) 子代理的结构化结果重试一次后通过");
                                    summary = json;
                                }
                                Err(again) => {
                                    eprintln!("(joy) 子代理的结构化结果两次都没过校验：{again}");
                                    summary.push_str(&format!(
                                        "\n（结构化结果两次都没通过校验：{again}；上面是它的原始文字）"
                                    ));
                                }
                            },
                            Err(e) => {
                                summary.push_str(&format!("\n（结构化重试没跑成：{e}）"));
                            }
                        }
                    }
                }
            }
            if summary.is_empty() {
                summary = "（子代理没有给出结论。）".to_string();
            }
            // 把「它用了什么」附在结论后面：父轮只看到一次工具调用，这些细节
            // 是它判断「这份结论可不可信」的唯一线索 —— **失败的尤其要标出来**，
            // 否则父轮会以为子代理一路顺利，而它其实是在一次失败的调用上编了
            // 个结论。
            let used: Vec<String> = result
                .tool_calls
                .iter()
                .map(|call| {
                    if call.ok() {
                        call.name.clone()
                    } else {
                        format!("{}（失败）", call.name)
                    }
                })
                .collect();
            if !used.is_empty() {
                summary.push_str(&format!(
                    "\n（子代理跑了 {} 轮，用了：{}）",
                    result.iterations,
                    used.join("、")
                ));
            }
            Ok(summary)
        })
    }
}
