//! `joy gather` —— 晨报，把 gather 图接到这台机器上。
//!
//! 每一路的绑定：
//!
//!   * github：`gh` CLI 自己问（不是模型问），有 `JOY_GH_REPO` 就查，没有就诚实地说这一路没配。
//!   * web：search_web 工具当**库**调 —— JOY_* 工具开关管的是「模型能不能搜」，这里是本家的代码在问，不走开关。
//!   * calendar：复用 triage 的 calendar.ics 读取 ——「今天有什么」只有一个解析器，两张图才不会打架。
//!   * memory：直接查 facts，**不过检索门** —— 门要花一次小模型调用去判断要不要查记忆，而晨报早就知道答案是「要」。
//!   * synthesize：唯一的模型调用，**无 tools** —— 只提议、绝不行动的结构保证。
//!   * draft：outbox 里的一个 markdown 文件，人去读。
//!
//! 每个 scan 的失败都翻译成诚实文字（图那层的包装负责），CLI 只管把最终
//! state 里有什么念出来。

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

/// 跑一趟晨报并打印。唯一的写是 outbox 里的草稿文件。
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
                // "(no calendar)" / "(nothing today)" 都是「没有」。
                let count = if text.starts_with('(') {
                    0
                } else {
                    text.matches(';').count() as i64 + 1
                };
                Ok((text, count))
            })
        })
    };

    // 直接查 facts，不过检索门 —— 晨报早知道答案就是「要」。
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

/// github scan：`gh` CLI 自己问（async 子进程，不阻塞 worker）。
/// 有 repo 就查，没有就如实报缺配 —— 不装作「没有 open PR」。
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

/// web scan：search_web 工具当库调。工具失败时返回 Err —— 图那层会把它
/// 翻译成诚实文字，跟模型自己调工具时的「文本化错误」是同一个约定。
async fn bind_web(home: &std::path::Path) -> WebScanFn {
    // search_web 的 handler 需要 ToolCtx（它只用 home）；开一个独立的连接池
    // 是可接受的 —— 这一路每天就跑一次，池随闭包活到进程结束。
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let ctx = ToolCtx {
        approval: None,
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
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
