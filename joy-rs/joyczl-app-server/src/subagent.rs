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
}

impl SubagentRunner for Delegated {
    fn run(&self, task: String, max_iterations: Option<i32>) -> BoxFut {
        let tools = self.tools.clone();
        let settings = self.settings.clone();
        let resolved = self.resolved.clone();
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

            let soul = crate::turn::load_soul(&home);
            let system = crate::turn::build_system(
                &soul,
                &resolved.model,
                &resolved.provider_id,
                "",
                "",
                None,
            ) + "\n\nYou are a subagent: do the one task below and answer with the \
                 conclusion only. Whoever asked cannot see your steps, and you cannot ask \
                 follow-up questions.";

            // 工具表用**父轮的副本**，只是其中没有 delegate_task —— 于是递归
            // 派生不是「被拒绝」，而是根本不存在的选项。
            let child_tools = tools.without(joyczl_tools::subagent::NAME);
            // 子代理**没有批准通道**（`None`）：它不该阻塞在人类身上，所以
            // 需要批准的动作在它那儿直接按拒绝处理 —— 见 limitations.md。
            let ctx = crate::tool_ctx(&facts, &episodes, &chat, &calendar, &home, None);

            let result = joyczl_loop::run(joyczl_loop::Turn {
                client: resolved.client.as_ref(),
                model: &resolved.model,
                system,
                // 空历史：委派的意义就是把上下文的重担留在父轮那边。
                history: Vec::new(),
                user_message: task,
                tools: &child_tools,
                ctx,
                max_iterations: iterations,
                max_tokens: SUBAGENT_MAX_TOKENS,
                observer: None,
                on_text: None,
                interrupt: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("模型调用失败：{e}"))?;

            let mut summary = result.reply.trim().to_string();
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
