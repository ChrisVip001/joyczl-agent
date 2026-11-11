//! `joyczl-eval` — evaluation, the diamond before release.
//!
//! Deterministic evals run the same `run_turn` in-process with a scripted model —
//! offline, 0/1, reproducible, the same infrastructure as unit tests; judge uses real
//! models; the release gate is guarded by the deterministic evals' exit code.
//!
//! Three tiers, cheap to expensive:
//!
//!   * deterministic: scripted model + assertions (what the reply contains, which tools, how many turns). Offline, deterministic, must pass 100% — they are the release gate.
//!   * judge: a real model answers one turn; a referee small model scores 0-10 by rubric. Needs a key; scores don't block release.
//!   * arena: dual-model racing is **not done** — more show than engineering value, see README.
//!
//! Reports land in `<home>/eval_report.json`, history appends to `<home>/eval_runs.jsonl`
//! — the same "ledger on disk" rule as trace/usage.

pub mod judge;
pub mod report;
pub mod scenario;

use std::path::{Path, PathBuf};

/// `{{home}}` in cases always expands to a **forward-slash** path.
///
/// Fixture files often embed it inside JSON (mcp.json args, tool arguments), while a
/// Windows home looks like `C:\Users\…\Temp\.tmp42`: backslashes are illegal JSON
/// escapes, so the fixture would be judged "not valid JSON" and the case fails on its
/// own fixture. Forward slashes are legal separators in the Windows file API — both read.
fn expand_home(text: &str, home: &Path) -> String {
    text.replace("{{home}}", &home.display().to_string().replace('\\', "/"))
}
use std::sync::{Arc, Mutex};

use anyhow::Result;
use joyczl_app_server::{handle, open as open_server, run_turn, EventSink, Frame, Server};
use joyczl_protocol::{
    GateDecisionKind, JsonRpcRequest, RequestId, ServerNotification, TurnStartParams,
};
use joyczl_provider::{CreateRequest, CreateResponse, Resolved};

use scenario::{Expectations, Scenario};

/// The harvest after one case runs (shared by deterministic and judge).
#[derive(Debug, Default)]
pub struct TurnOutput {
    pub reply: String,
    pub tools: Vec<String>,
    pub iterations: i32,
    pub gate: Option<String>,
    pub interrupted: bool,
    pub consolidation_new_facts: i32,
    /// Raw output of each tool execution — for asserting "the tool really succeeded".
    pub tool_outputs: Vec<String>,
    /// Full text of each model request, in order — raw material for prompt assertions.
    pub prompts: Vec<String>,
    /// The task's final status, when the case created one (0.14).
    pub task_status: Option<String>,
    /// Whether every acceptance condition passed (only set when there were conditions).
    pub task_checks_passed: Option<bool>,
    /// How many deliverables the task recorded.
    pub task_artifacts: usize,
}

/// The verdict on one assertion: empty on pass, "why" on failure.
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
    if let Some(want) = &expect.task_status {
        if out.task_status.as_deref() != Some(want.as_str()) {
            fails.push(format!("任务状态应为 {want:?}，实际 {:?}", out.task_status));
        }
    }
    if let Some(want) = expect.task_checks_passed {
        if out.task_checks_passed != Some(want) {
            fails.push(format!(
                "验收结论应为 passed={want}，实际 {:?}",
                out.task_checks_passed
            ));
        }
    }
    if let Some(want) = expect.task_artifacts {
        if out.task_artifacts != want {
            fails.push(format!(
                "交付物应为 {want} 个，实际 {} 个",
                out.task_artifacts
            ));
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

/// A provider wrapper: carbon-copies each request's full text — raw material for
/// prompt assertions. `inner` is the scripted mock; zero changes beyond the copy.
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
                joyczl_provider::ContentBlock::Image { media_type, data } => {
                    text.push_str(&format!("[image {media_type} {}b]", data.len()));
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

/// Starts a clean home + server, installs the scripted model, and runs every turn of
/// the case.
///
/// Each case gets a brand-new state.db: consolidation counts, history, and facts stay
/// separate — no ordering between evals, so a failure has exactly one cause.
async fn run_scenario(home: &Path, scenario: &Scenario) -> Result<TurnOutput> {
    let settings = joyczl_config::Settings {
        home: home.to_path_buf(),
        // Fake key: lets boot's resolve succeed quietly; the whole thing is swapped for the mock later.
        api_key: Some("eval-dummy".to_string()),
        history_turns: scenario.settings.history_turns.unwrap_or(12),
        consolidate_every: scenario.settings.consolidate_every.unwrap_or(6),
        retrieval_top_k: scenario.settings.retrieval_top_k.unwrap_or(4),
        max_iterations: scenario.settings.max_iterations.unwrap_or(10),
        max_tokens: scenario.settings.max_tokens.unwrap_or(8192),
        context_window: scenario.settings.context_window,
        compact_threshold: scenario.settings.compact_threshold.unwrap_or(0.8),
        graph_workflows: scenario.settings.graph_workflows.unwrap_or(false),
        exec_enabled: scenario.settings.exec_enabled.unwrap_or(false),
        write_enabled: scenario.settings.write_enabled.unwrap_or(false),
        delegate_enabled: scenario.settings.delegate_enabled.unwrap_or(false),
        plan_enabled: scenario.settings.plan_enabled.unwrap_or(false),
        hooks_enabled: scenario.settings.hooks_enabled.unwrap_or(false),
        exec_allow: scenario.settings.exec_allow.clone().unwrap_or_default(),
        tool_result_total_chars: scenario.settings.tool_result_total_chars.unwrap_or(200_000),
        tool_result_max_chars: scenario.settings.tool_result_max_chars.unwrap_or(30_000),
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
        std::fs::write(&path, expand_home(content, home))?;
    }

    let mut server = open_server(&settings).await?;
    let recording = Arc::new(RecordingProvider {
        inner: Arc::new(scenario.build_mock(home)),
        prompts: Mutex::new(Vec::new()),
    });
    server.install_provider(Resolved {
        provider_id: "eval-mock".to_string(),
        client: recording.clone(),
        model: "eval-model".to_string(),
        small_model: "eval-small".to_string(),
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        // Mock has no pixels; vision on keeps the attachment path testable in principle.
        vision: false,
    });

    // A case may create a long task before its turns (0.14). It goes through the real
    // `task/create` path, so the case tests what the dashboard and the CLI do — not a
    // test-only shortcut into the table.
    let mut task_id: Option<String> = None;
    if let Some(spec) = &scenario.task {
        task_id = Some(create_case_task(&server, spec, scenario, home).await?);
    }

    let mut output = TurnOutput::default();
    for turn in scenario.effective_turns() {
        // P1 recovery benchmarks: drop this process and boot a new one on the same home. What
        // runs next is a genuinely fresh start — the ledger reconciliation really happened, so
        // "the task survived a restart" is measured, not assumed.
        if turn.restart {
            drop(server);
            server = open_server(&settings).await?;
            server.install_provider(Resolved {
                provider_id: "eval-mock".to_string(),
                client: recording.clone(),
                model: "eval-model".to_string(),
                small_model: "eval-small".to_string(),
                context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
                vision: false,
            });
            eprintln!("(joy) eval：模拟进程重启（同一个 home），接着跑下一轮");
        }
        let session_id = turn.session_id.unwrap_or_else(|| "eval".to_string());
        let (sink, mut rx) = EventSink::channel();
        // {{home}} in messages: lets the case point at its own files inside home.
        let message = expand_home(&turn.message, home);
        let params = TurnStartParams {
            session_id: Some(session_id),
            message,
            stream: Some(true),
            cwd: turn.cwd.as_deref().map(|text| expand_home(text, home)),
            attachments: None,
            tier: None,
        };
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            run_turn(&task_server, params, RequestId::Number(0), &sink).await
        });

        // Notification loop: interrupts and consolidation counts are collected here.
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

                    // Flip the token exactly at the Nth increment (is_some_and fires once).
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
            // config/write re-resolves the provider per patch — reinstall the scripted
            // mock, or the next turn hits the real endpoint.
            server.install_provider(Resolved {
                provider_id: "eval-mock".to_string(),
                client: recording.clone(),
                model: "eval-model".to_string(),
                small_model: "eval-small".to_string(),
                context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
                vision: false,
            });
        }
    }

    output.prompts = recording.prompts.lock().expect("锁").clone();

    // Read the task back through the protocol: the assertions are about what a human sees.
    if let Some(id) = &task_id {
        let view = protocol_call(&server, "task/get", serde_json::json!({ "taskId": id })).await?;
        let task = &view["task"];
        output.task_status = task["status"].as_str().map(str::to_string);
        let checks = task["checks"].as_array().cloned().unwrap_or_default();
        if !checks.is_empty() {
            output.task_checks_passed = Some(
                checks
                    .iter()
                    .all(|check| check["passed"] == serde_json::json!(true)),
            );
        }
        let artifacts = protocol_call(
            &server,
            "task/artifacts",
            serde_json::json!({ "taskId": id }),
        )
        .await?;
        output.task_artifacts = artifacts["artifacts"]
            .as_array()
            .map(|list| list.len())
            .unwrap_or(0);
    }
    Ok(output)
}

/// One JSON-RPC round trip through the real dispatch, returning the result body.
async fn protocol_call(
    server: &Server,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let (sink, mut rx) = EventSink::channel();
    let request = JsonRpcRequest {
        jsonrpc: joyczl_protocol::JSONRPC_VERSION.to_string(),
        id: RequestId::Number(0),
        method: method.to_string(),
        params: Some(params),
    };
    handle(server, request, sink).await;
    while let Some(frame) = rx.recv().await {
        if let Frame::Response(message) = frame {
            return match message {
                joyczl_protocol::JsonRpcMessage::Response(response) => Ok(response.result),
                joyczl_protocol::JsonRpcMessage::Error(error) => {
                    anyhow::bail!("{method} 被拒：{}", error.error.message)
                }
            };
        }
    }
    anyhow::bail!("{method} 没有应答")
}

/// Create the case's task through `task/create` and hand back its id.
async fn create_case_task(
    server: &Server,
    spec: &scenario::ScenarioTask,
    scenario: &Scenario,
    home: &Path,
) -> Result<String> {
    let mut params = serde_json::json!({
        "title": spec.title,
        "sessionId": spec.session_id.clone().unwrap_or_else(|| "eval".to_string()),
    });
    if let Some(goal) = &spec.goal_condition {
        params["goalCondition"] = serde_json::json!(goal);
    }
    if let Some(rounds) = spec.max_rounds {
        params["maxRounds"] = serde_json::json!(rounds);
    }
    // The task's own working directory: the case's `cwd`, so the write tools and the
    // acceptance checks look at the same folder.
    if let Some(cwd) = &scenario.cwd {
        params["cwd"] = serde_json::json!(expand_home(cwd, home));
    }
    if !spec.acceptance.is_empty() {
        params["acceptance"] = serde_json::to_value(&spec.acceptance)?;
    }
    if let Some(budget) = &spec.budget {
        params["budget"] = serde_json::to_value(budget)?;
    }
    let created = protocol_call(server, "task/create", params).await?;
    created["task"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("task/create 没给出 id：{created}"))
}

/// Writes config through the real protocol — evals and the dashboard share the same
/// `config/write` path, so hot-reload verification is truly end-to-end.
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

/// Deterministic evals: run all cases, print a table, write the report, **return the failure count** (0 = gate opens).
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
        let missing = prereq_missing(scenario);
        if let Some(why) = missing {
            skipped += 1;
            println!(
                "  ⊙ {} {}（跳过：{why}）",
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

/// Long-task benchmarks (P1): the same scripted harness, but the question is different.
///
/// Deterministic evals ask "is this behavior still correct" and block releases. This suite
/// asks "did autonomous task work get better or worse" — so it **records and never blocks**
/// (exit 0 even when cases fail). Two numbers come out separately:
///
///   * 任务成功率: cases that reached the outcome their own case file declares (all of them
///     declare `done` — acceptance passed on facts);
///   * 中断恢复率: the subset tagged `recovery` (a turn is cut short, a fresh process boots on
///     the same home, a later turn finishes the task) that still reached that outcome.
///
/// Mixing the two would hide the thing worth watching: a change that keeps the happy path
/// working but breaks restarts would still look green.
pub async fn run_long(paths: &[PathBuf], report_home: &Path) -> Result<i32> {
    let scenarios = scenario::load_all(paths)?;
    if scenarios.is_empty() {
        anyhow::bail!("没有加载到任何长任务用例 —— 检查路径（evals/long/）");
    }

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut recovery_total = 0usize;
    let mut recovered = 0usize;
    let mut results: Vec<serde_json::Value> = Vec::new();

    for scenario in &scenarios {
        if let Some(why) = prereq_missing(scenario) {
            skipped += 1;
            println!(
                "  ⊙ {} {}（跳过：{why}）",
                scenario.id, scenario.description
            );
            results.push(serde_json::json!({"id": scenario.id, "skipped": true}));
            continue;
        }
        if scenario.recovery {
            recovery_total += 1;
        }
        let dir = tempfile::tempdir()?;
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
            if scenario.recovery {
                recovered += 1;
            }
            println!("  ✓ {} {}", scenario.id, scenario.description);
        } else {
            println!("  ✗ {} {}", scenario.id, scenario.description);
            for reason in &verdict {
                println!("      {reason}");
            }
        }
        results.push(serde_json::json!({
            "id": scenario.id,
            "passed": ok,
            "recovery": scenario.recovery,
            "reasons": verdict,
        }));
    }

    let scored = scenarios.len() - skipped;
    let failed = scored - passed;
    let rate = if scored == 0 {
        0.0
    } else {
        passed as f64 / scored as f64
    };
    let recovery_rate = if recovery_total == 0 {
        0.0
    } else {
        recovered as f64 / recovery_total as f64
    };
    println!(
        "长任务基准：{} 达成 / {} 未达成 / {} 跳过 / 共 {}（成功率 {:.0}%）",
        passed,
        failed,
        skipped,
        scored,
        rate * 100.0
    );
    println!(
        "中断恢复：{}/{}（{:.0}%）—— 打断后重启进程，再跑下一轮是否仍然收尾",
        recovered,
        recovery_total,
        recovery_rate * 100.0
    );
    println!("长任务基准只记录，不拦发版（exit 0）。");
    report::write(
        report_home,
        "long",
        serde_json::json!({
            "passed": passed, "failed": failed, "skipped": skipped, "total": scored,
            "success_rate": rate,
            "recovery": {
                "cases": recovery_total, "recovered": recovered, "rate": recovery_rate,
            },
            "cases": results,
        }),
    )?;
    Ok(0)
}

/// Why this case cannot run here (`None` = it can). A skip is honest, not a pass.
fn prereq_missing(scenario: &Scenario) -> Option<&'static str> {
    match scenario.prereq.as_deref() {
        Some("python3") if !runnable("python3", &["--version"]) => {
            Some("这台机器上没有能跑的 python3")
        }
        Some("sh") if !runnable("sh", &["-c", "exit 0"]) => {
            Some("这台机器上没有 sh（钩子用例跑的是 shell 命令）")
        }
        Some("sandbox") if !joyczl_tools::exec::sandbox_available() => {
            Some("这台机器上没有可用的沙箱（sandbox-exec / bubblewrap）")
        }
        _ => None,
    }
}

/// Prerequisites must **actually run**, not just exist under the same PATH name.
///
/// Windows' `python3.exe` is often just the app-store placeholder: the file exists,
/// running it exits immediately, so "has python3" cases pass the check and then fail
/// inexplicably when starting the MCP server. Probing once is more honest than betting.
fn runnable(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The real-model path: the provider resolves from the environment (None when the key is missing; the caller decides to skip).
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
        cwd: None,
        attachments: None,
        tier: None,
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

/// judge: a real model answers one turn; a small model referees, scoring 0-10 by rubric.
/// The referee is not a contestant — the main model is tested, the cheap tier scores.
/// Needs a key; without one the whole group skips with an honest note (judge scores, doesn't block release).
/// Returns 0 always (unless no cases loaded at all).
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
