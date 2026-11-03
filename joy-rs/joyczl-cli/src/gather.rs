//! `joy gather` — the morning report, wiring the gather graph to this machine.
//!
//! Per-leg bindings:
//!
//!   * github: `gh` CLI asks directly (not the model); `JOY_GH_REPO` set → query, else admits.
//!   * web: search_web is called as a **library** — first-party code, JOY_* toggles don't apply.
//!   * calendar: reuses triage's calendar.ics reader — one parser, two graphs never disagree.
//!   * memory: facts queried directly, **no retrieval gate** — the answer (yes) is known.
//!   * synthesize: the only model call, **no tools** — propose-only, never act, by design.
//!   * draft: a markdown file in the outbox, for a human to read.
//!
//! Each scan's failure is translated into honest text (the graph layer's wrapper
//! handles that); the CLI just reads out what the final state contains.

use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use joyczl_app_server::Server;
use joyczl_graph::workflows::gather::{
    build_gather_graph, synth_request, CalendarScanFn, DraftFn, GithubScanFn, MemoryScanFn,
    SynthFn, WebScanFn,
};
use joyczl_graph::workflows::triage::todays_events;
use joyczl_graph::{run_graph, State, DEFAULT_MAX_STEPS};
use joyczl_tools::ToolCtx;
use serde_json::json;

const DEFAULT_TOPICS: &str = "AI agent harness loop memory eval";

/// Runs one morning report and prints it. The only write is the outbox draft file.
pub async fn run(server: &Server) -> Result<()> {
    println!("gathering — github、web、calendar、memory 一起问……");

    let home = server.settings().home.clone();
    let github = bind_github();
    let web = bind_web(&home).await;

    let calendar: CalendarScanFn = {
        let home = home.clone();
        Arc::new(move || {
            let home = home.clone();
            Box::pin(async move {
                let text = todays_events(&home);
                // "(no calendar)" / "(nothing today)" both mean "nothing".
                let count = if text.starts_with('(') {
                    0
                } else {
                    text.matches(';').count() as i64 + 1
                };
                Ok((text, count))
            })
        })
    };

    // Queries facts directly, bypassing the retrieval gate — the answer is already "yes".
    let mem_rows = Arc::new(
        server
            .facts()
            .search("project repo contributors release", 8)
            .await
            .unwrap_or_default(),
    );
    let memory: MemoryScanFn = Arc::new(move || {
        let mem_rows = mem_rows.clone();
        Box::pin(async move {
            if mem_rows.is_empty() {
                Ok("(nothing relevant)".to_string())
            } else {
                Ok(mem_rows
                    .iter()
                    .map(|f| format!("- {}: {}", f.subject, f.content))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        })
    });

    let synth: SynthFn = match server.resolved() {
        Some(resolved) => {
            let client = resolved.client.clone();
            let model = resolved.model.clone();
            Arc::new(move |state| {
                let client = client.clone();
                let request = synth_request(&model, &state);
                Box::pin(async move {
                    client
                        .create(request)
                        .await
                        .map(|r| r.text())
                        .map_err(|e| e.to_string())
                })
            })
        }
        None => Arc::new(|_state| {
            Box::pin(async {
                Err("模型还没配好：缺 API key。往 .env 里加对应 provider 的 key 再试。".to_string())
                    as Result<String, String>
            })
        }),
    };

    let draft: DraftFn = {
        let home = home.clone();
        Arc::new(move |state| {
            let digest = state.str("digest").unwrap_or_default().to_string();
            let outbox = home.join("outbox");
            Box::pin(async move {
                std::fs::create_dir_all(&outbox).map_err(|e| e.to_string())?;
                let dest = outbox.join(format!("gather-{}.md", Local::now().format("%Y-%m-%d")));
                std::fs::write(&dest, format!("{digest}\n")).map_err(|e| e.to_string())?;
                Ok(dest.display().to_string())
            })
        })
    };

    let graph = build_gather_graph(github, web, calendar, memory, synth, draft)
        .expect("gather 图写死在这个文件旁边，建不起来就是 bug");

    let report = run_graph(graph, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("图的撞键错误属于图的 bug");

    match report.state.str("digest") {
        Some(digest) if !digest.is_empty() => println!("{digest}"),
        _ => println!("（没有摘要 —— 每一路都是空的）"),
    }
    if let Some(path) = report.state.str("draft_path") {
        println!("已存到 {path}");
    }
    for (node, error) in &report.errors {
        println!("{node}: {error}");
    }
    Ok(())
}

/// github scan: the `gh` CLI asks directly (async subprocess, doesn't block workers).
/// With a repo it queries; without, it honestly reports missing config — no fake "no PRs".
fn bind_github() -> GithubScanFn {
    let repo = std::env::var("JOY_GH_REPO").unwrap_or_default();
    Arc::new(move || {
        let repo = repo.trim().to_string();
        Box::pin(async move {
            if repo.is_empty() {
                return Err("JOY_GH_REPO 没设（如 JOY_GH_REPO=owner/repo）".to_string());
            }
            let prs = gh_list(&repo, &["pr", "list", "--state", "open", "--limit", "20"]).await?;
            let issues = gh_list(
                &repo,
                &["issue", "list", "--state", "open", "--limit", "20"],
            )
            .await?;
            let mut lines = Vec::new();
            lines.push(if prs.is_empty() {
                "Open PRs: (none)".to_string()
            } else {
                format!("Open PRs:\n{}", prs.join("\n"))
            });
            lines.push(if issues.is_empty() {
                "Open issues: (none)".to_string()
            } else {
                format!("Open issues:\n{}", issues.join("\n"))
            });
            Ok((lines.join("\n\n"), prs.len() as i64, issues.len() as i64))
        })
    })
}

async fn gh_list(repo: &str, args: &[&str]) -> Result<Vec<String>, String> {
    let output = tokio::process::Command::new("gh")
        .arg("-R")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("gh 跑不起来：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect())
}

/// web scan: the search_web tool called as a library. Failures return Err — the
/// graph layer textualizes them, same contract as when the model calls tools itself.
async fn bind_web(home: &std::path::Path) -> WebScanFn {
    // search_web's handler needs a ToolCtx (it only uses home); opening a separate
    // pool is acceptable — this leg runs once a day, the pool lives with the closure.
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let ctx = ToolCtx {
        approval: None,
        jobs: None,
        hooks: None,
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        cwd: None, // gather is offline aggregation: process cwd semantics.
        home: home.to_path_buf(),
    };
    let topics = std::env::var("JOY_GH_REPO").unwrap_or_else(|_| DEFAULT_TOPICS.to_string());
    let query = format!("{topics} {DEFAULT_TOPICS} this week");

    Arc::new(move || {
        let tool = joyczl_tools::web::search_web();
        let ctx = ctx.clone();
        let query = query.clone();
        Box::pin(async move {
            (tool.handler)(ctx, json!({"query": query, "max_results": 5}))
                .await
                .map_err(|e| e.to_string())
        })
    })
}
