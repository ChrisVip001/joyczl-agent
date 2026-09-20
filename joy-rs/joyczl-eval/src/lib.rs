//! `joyczl-eval` —— 评测，发版前的那颗钻石。
//!
//! 确定性 eval 进程内跑同一个 `run_turn` + scripted 模型——离线、0/1、
//! 可重复，跟单测同一套基建；judge 走真模型；release gate 由确定性
//! eval 的退出码把守。
//!
//! 三层，从便宜到贵：
//!
//!   * deterministic：脚本化模型 + 断言（回复包含什么、哪些工具、几轮）。离线、确定、必须 100% 通过 —— 它们就是发版闸门。
//!   * judge：真模型答一轮，referee 小模型按 rubric 打 0-10 分。需要 key；出分不拦发版。
//!   * arena：双模型竞速**不做** —— 展示性大于工程价值，见 README。
//!
//! 报告落在 `<home>/eval_report.json`，历史追加 `<home>/eval_runs.jsonl`
//! —— 跟 trace/usage 同一条「落盘账本」的规矩。

pub mod judge;
pub mod report;
pub mod scenario;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use joyczl_app_server::{handle, open as open_server, run_turn, EventSink, Frame, Server};
use joyczl_protocol::{
    GateDecisionKind, JsonRpcRequest, RequestId, ServerNotification, TurnStartParams,
};
use joyczl_provider::{CreateRequest, CreateResponse, Resolved};

use scenario::{Expectations, Scenario};

/// 一个用例跑完后的收成（确定性与 judge 共用）。
#[derive(Debug, Default)]
pub struct TurnOutput {
    pub reply: String,
    pub tools: Vec<String>,
    pub iterations: i32,
    pub gate: Option<String>,
    pub interrupted: bool,
    pub consolidation_new_facts: i32,
    /// 每次工具执行的输出原文 —— 断言「工具真的成功跑过」用。
    pub tool_outputs: Vec<String>,
    /// 每次模型请求的全文，按序 —— prompt 断言的原料。
    pub prompts: Vec<String>,
}

/// 一条断言的裁定：通过为空，失败给「为什么」。
type Verdict = Vec<String>;

fn check_contains(where_: &str, text: &str, needles: &[String], fails: &mut Verdict) {
    for needle in needles {
        if !text.contains(needle.as_str()) {
            fails.push(format!("{where_}应包含 {needle:?}，实际：{text:?}"));
        }
    }
}

fn check_not_contains(where_: &str, text: &str, banned: &[String], fails: &mut Verdict) {
    for needle in banned {
        if text.contains(needle.as_str()) {
            fails.push(format!("{where_}不应包含 {needle:?}，实际：{text:?}"));
        }
    }
}

fn score(expect: &Expectations, out: &TurnOutput) -> Verdict {
    let mut fails = Verdict::new();
    check_contains("回复", &out.reply, &expect.reply_contains, &mut fails);
    check_not_contains("回复", &out.reply, &expect.reply_not_contains, &mut fails);
    if !expect.tools_used.is_empty() && out.tools != expect.tools_used {
        fails.push(format!(
            "工具序列应为 {:#?}，实际 {:#?}",
            expect.tools_used, out.tools
        ));
    }
    for needle in &expect.tool_output_contains {
        if !out.tool_outputs.iter().any(|o| o.contains(needle.as_str())) {
            fails.push(format!(
                "没有工具的输出包含 {needle:?}（实际输出 {:#?}）",
                out.tool_outputs
            ));
        }
    }
    if let Some(gate) = &expect.gate {
        if out.gate.as_deref() != Some(gate.as_str()) {
            fails.push(format!("检索门应为 {gate:?}，实际 {:?}", out.gate));
        }
    }
    if let Some(iterations) = expect.iterations {
        if out.iterations != iterations {
            fails.push(format!("应为 {iterations} 轮，实际 {} 轮", out.iterations));
        }
    }
    if let Some(want) = expect.consolidation_new_facts {
        if out.consolidation_new_facts != want {
            fails.push(format!(
                "consolidation 应提炼 {want} 条，实际 {} 条",
                out.consolidation_new_facts
            ));
        }
    }
    if let Some(want) = expect.interrupted {
        if out.interrupted != want {
            fails.push(format!("interrupted 应为 {want}，实际 {}", out.interrupted));
        }
    }
    let all = out.prompts.join("\n---\n");
    check_contains("模型请求", &all, &expect.prompt_contains, &mut fails);
    check_not_contains("模型请求", &all, &expect.prompt_not_contains, &mut fails);
    match out.prompts.last() {
        Some(last) => {
            check_contains("最后请求", last, &expect.last_prompt_contains, &mut fails);
            check_not_contains(
                "最后请求",
                last,
                &expect.last_prompt_not_contains,
                &mut fails,
            );
        }
        None if !expect.last_prompt_contains.is_empty() => {
            fails.push("没有任何模型请求可断言".to_string());
        }
        None => {}
    }
    fails
}

/// 包装 provider：把每次请求的全文抄送一份 —— prompt 断言的原料。
/// inner 是 scripted mock；抄送之外零改动。
struct RecordingProvider {
    inner: Arc<dyn joyczl_provider::Provider>,
    prompts: Mutex<Vec<String>>,
}

fn render(request: &CreateRequest) -> String {
    let mut text = request.system.clone().unwrap_or_default();
    for message in &request.messages {
        text.push('\n');
        for block in &message.content {
            match block {
                joyczl_provider::ContentBlock::Text { text: t } => text.push_str(t),
                joyczl_provider::ContentBlock::ToolUse { name, input, .. } => {
                    text.push_str(&format!("[tool_use {name} {input}]"));
                }
                joyczl_provider::ContentBlock::ToolResult { content, .. } => {
                    text.push_str(&format!("[tool_result {content}]"));
                }
            }
        }
    }
    text
}

type ProviderFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<CreateResponse, joyczl_provider::ProviderError>>
            + Send
            + 'a,
    >,
>;

impl joyczl_provider::Provider for RecordingProvider {
    fn create(&self, request: CreateRequest) -> ProviderFuture<'_> {
        self.prompts.lock().expect("锁").push(render(&request));
        self.inner.create(request)
    }

    fn stream(
        &self,
        request: CreateRequest,
        on_text: joyczl_provider::TextSink,
    ) -> ProviderFuture<'_> {
        self.prompts.lock().expect("锁").push(render(&request));
        self.inner.stream(request, on_text)
    }
}

/// 起一个干净的 home + server，装上 scripted 模型，把用例的每一轮跑完。
///
/// 每个用例一个全新的 state.db：consolidation 计数、历史、事实互不串味
/// —— eval 之间没有顺序依赖，失败才有唯一的原因。
async fn run_scenario(home: &Path, scenario: &Scenario) -> Result<TurnOutput> {
    let settings = joyczl_config::Settings {
        home: home.to_path_buf(),
        // 假 key：让 boot 的 resolve 安静地成功，随后整个被 mock 替换。
        api_key: Some("eval-dummy".to_string()),
        history_turns: scenario.settings.history_turns.unwrap_or(12),
        consolidate_every: scenario.settings.consolidate_every.unwrap_or(6),
        retrieval_top_k: scenario.settings.retrieval_top_k.unwrap_or(4),
        max_iterations: scenario.settings.max_iterations.unwrap_or(10),
        graph_workflows: scenario.settings.graph_workflows.unwrap_or(false),
        ..joyczl_config::Settings::default()
    };
    settings.ensure_home()?;
    for (relative, content) in scenario.files.iter().flatten() {
        if relative.contains("..") {
            anyhow::bail!("用例文件路径不允许越出 home：{relative}");
        }
        let path = home.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            content.replace("{{home}}", &home.display().to_string()),
        )?;
    }

    let server = open_server(&settings).await?;
    let recording = Arc::new(RecordingProvider {
        inner: Arc::new(scenario.build_mock()),
        prompts: Mutex::new(Vec::new()),
    });
    server.install_provider(Resolved {
        provider_id: "eval-mock".to_string(),
        client: recording.clone(),
        model: "eval-model".to_string(),
        small_model: "eval-small".to_string(),
    });

    let mut output = TurnOutput::default();
    for turn in scenario.effective_turns() {
        let session_id = turn.session_id.unwrap_or_else(|| "eval".to_string());
        let (sink, mut rx) = EventSink::channel();
        let params = TurnStartParams {
            session_id: Some(session_id),
            message: turn.message.clone(),
            stream: Some(true),
        };
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            run_turn(&task_server, params, RequestId::Number(0), &sink).await
        });

        // 通知循环：打断与 consolidation 计数都在这里收。
        let mut deltas = 0usize;
        let mut turn_id = String::new();
        loop {
            let Some(frame) = rx.recv().await else {
                break;
            };
            let Frame::Notification(notification) = frame else {
                continue;
            };
            match notification {
                ServerNotification::TurnStarted(started) => turn_id = started.turn_id.clone(),
                ServerNotification::TextDelta(_) => {
                    deltas += 1;

                    // 恰好到第 N 个增量时拨令牌（is_some_and 保证只拨一次）。
                    if turn.interrupt_after_deltas.is_some_and(|n| deltas == n)
                        && !server.interrupt_turn(&turn_id)
                    {
                        anyhow::bail!("打断失败：turn {turn_id} 已经不在跑");
                    }
                }
                ServerNotification::ConsolidationCompleted(done) => {
                    output.consolidation_new_facts += done.new_facts;
                }
                ServerNotification::ToolCompleted(done) => {
                    output.tool_outputs.push(done.output.clone());
                }
                ServerNotification::TurnCompleted(done) => {
                    output.reply = done.reply;
                    output.iterations = done.iterations;
                    output.interrupted = done.meta.interrupted;
                    for call in &done.meta.tools {
                        output.tools.push(call.tool.clone());
                    }
                    if let Some(gate) = &done.meta.gate {
                        output.gate = Some(match gate.decision {
                            GateDecisionKind::Retrieve => "retrieve".to_string(),
                            GateDecisionKind::Skip => "skip".to_string(),
                        });
                    }
                    break;
                }
                _ => {}
            }
        }
        task.await
            .map_err(|e| anyhow::anyhow!("eval turn 任务崩了：{e}"))?
            .map_err(|e| anyhow::anyhow!("这一轮没跑通：{}", e.message))?;

        if let Some(patch) = &turn.apply_patch {
            apply_config(&server, patch).await?;
            // config/write 会按补丁重解析 provider —— 把 scripted mock
            // 重新装回去，否则下一轮打的是真端点。
            server.install_provider(Resolved {
                provider_id: "eval-mock".to_string(),
                client: recording.clone(),
                model: "eval-model".to_string(),
                small_model: "eval-small".to_string(),
            });
        }
    }

    output.prompts = recording.prompts.lock().expect("锁").clone();
    Ok(output)
}

/// 走真协议写配置 —— eval 与 dashboard 走同一条 `config/write` 路，
/// 热生效的验证才是端到端的。
async fn apply_config(server: &Server, patch: &serde_json::Value) -> Result<()> {
    let (sink, mut rx) = EventSink::channel();
    let request = JsonRpcRequest {
        jsonrpc: joyczl_protocol::JSONRPC_VERSION.to_string(),
        id: RequestId::Number(0),
        method: "config/write".to_string(),
        params: Some(serde_json::json!({"patch": patch})),
    };
    handle(server, request, sink).await;
    while let Some(frame) = rx.recv().await {
        if let Frame::Response(joyczl_protocol::JsonRpcMessage::Error(error)) = frame {
            anyhow::bail!("config/write 被拒：{}", error.error.message);
        }
    }
    Ok(())
}

/// 确定性 eval：跑全部用例，打印一张表，写报告，**返回失败数**（0 = 闸门放行）。
pub async fn run_deterministic(paths: &[PathBuf], report_home: &Path) -> Result<i32> {
    let scenarios = scenario::load_all(paths)?;
    if scenarios.is_empty() {
        anyhow::bail!("没有加载到任何确定性用例 —— 检查路径（evals/deterministic/）");
    }

    let mut tempdirs = Vec::new();
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut any_failed = false;

    for scenario in &scenarios {
        if scenario.prereq.as_deref() == Some("python3") && which_python3().is_none() {
            skipped += 1;
            println!(
                "  ⊙ {} {}（跳过：这台机器上没有 python3）",
                scenario.id, scenario.description
            );
            results.push(serde_json::json!({"id": scenario.id, "skipped": true}));
            continue;
        }
        let dir = tempfile::tempdir()?;
        tempdirs.push(dir.path().to_path_buf());
        let outcome = run_scenario(dir.path(), scenario).await;
        let (verdict, ok) = match outcome {
            Ok(output) => {
                let verdict = score(&scenario.expect, &output);
                let ok = verdict.is_empty();
                (verdict, ok)
            }
            Err(error) => (vec![format!("跑不起来：{error}")], false),
        };
        if ok {
            passed += 1;
            println!("  ✓ {} {}", scenario.id, scenario.description);
        } else {
            any_failed = true;
            println!("  ✗ {} {}", scenario.id, scenario.description);
            for reason in &verdict {
                println!("      {reason}");
            }
        }
        results.push(serde_json::json!({
            "id": scenario.id,
            "passed": ok,
            "reasons": verdict,
        }));
    }

    let failed = scenarios.len() - passed - skipped;
    println!(
        "确定性 eval：{} 通过 / {} 失败 / {} 跳过 / 共 {}",
        passed,
        failed,
        skipped,
        scenarios.len()
    );
    report::write(
        report_home,
        "deterministic",
        serde_json::json!({
            "passed": passed, "failed": failed, "skipped": skipped,
            "total": scenarios.len(), "cases": results,
        }),
    )?;
    Ok(if any_failed { failed as i32 } else { 0 })
}

fn which_python3() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    })
}

/// 真模型路径：provider 从环境解析（key 缺失返回 None，调用方决定跳过）。
async fn server_from_env(home: &Path) -> Result<Option<Server>> {
    let mut settings = joyczl_config::Settings::from_env();
    settings.home = home.to_path_buf();
    let server = open_server(&settings).await?;
    Ok(server.resolved().map(|_| server))
}

async fn collect_turn(server: &Server, message: &str) -> Result<TurnOutput> {
    let (sink, mut rx) = EventSink::channel();
    let params = TurnStartParams {
        session_id: Some("eval".to_string()),
        message: message.to_string(),
        stream: Some(true),
    };
    let task_server = server.clone();
    let task =
        tokio::spawn(
            async move { run_turn(&task_server, params, RequestId::Number(0), &sink).await },
        );
    let mut output = TurnOutput::default();
    while let Some(frame) = rx.recv().await {
        let Frame::Notification(notification) = frame else {
            continue;
        };
        if let ServerNotification::TurnCompleted(done) = notification {
            output.reply = done.reply;
            output.iterations = done.iterations;
            break;
        }
    }
    task.await
        .map_err(|e| anyhow::anyhow!("eval turn 任务崩了：{e}"))?
        .map_err(|e| anyhow::anyhow!("这轮没跑通：{}", e.message))?;
    Ok(output)
}

/// judge：真模型答一轮，small model 当裁判按 rubric 打 0-10 分。
/// 裁判不是选手 —— 被考的是主模型，打分的是便宜的那档。
/// 需要 key；没 key 整组跳过并如实说明（judge 出分，不拦发版）。
/// 返回值恒为 0（除非用例根本没加载到）。
pub async fn run_judge(paths: &[PathBuf], report_home: &Path) -> Result<i32> {
    let scenarios = scenario::load_judge(paths)?;
    if scenarios.is_empty() {
        anyhow::bail!("没有加载到任何 judge 用例 —— 检查路径（evals/judge/）");
    }
    let Some(server) = server_from_env(report_home).await? else {
        println!("judge 需要 API key（环境里没配到），整组跳过 —— 不拦发版。");
        return Ok(0);
    };
    let resolved = server.resolved().expect("上面刚验过 resolved 存在");
    let judge_model =
        std::env::var("JOY_JUDGE_MODEL").unwrap_or_else(|_| resolved.small_model.clone());
    println!(
        "judge：选手 {}，裁判 {}（{} 个用例）",
        resolved.model,
        judge_model,
        scenarios.len()
    );

    let mut scores: Vec<serde_json::Value> = Vec::new();
    let mut total = 0i64;
    for scenario in &scenarios {
        let output = collect_turn(&server, &scenario.message).await?;
        let verdict = judge::grade(
            resolved.client.as_ref(),
            &judge_model,
            scenario,
            &output.reply,
        )
        .await
        .map_err(|e| anyhow::anyhow!("用例 {} 的裁判环节失败：{e}", scenario.id))?;
        println!(
            "  {} {} → {}/10  {}",
            scenario.id, scenario.description, verdict.0, verdict.1
        );
        total += verdict.0;
        scores.push(serde_json::json!({
            "id": scenario.id,
            "score": verdict.0,
            "reason": verdict.1,
            "reply": output.reply,
        }));
    }
    if !scores.is_empty() {
        println!("judge 均分：{:.1}/10", total as f64 / scores.len() as f64);
    }
    report::write(report_home, "judge", serde_json::json!({"cases": scores}))?;
    Ok(0)
}
