//! `joy mcp` — viewing, login, and serving of MCP servers.
//!
//! `joy mcp`         lists servers configured in mcp.json and their auth methods
//! `joy mcp login X` runs browser OAuth (discovery → registration → authorize →
//!                   token exchange); the token lands in `<home>/mcp-auth/X.json` (0600)
//! `joy mcp serve`   the reverse: Joy itself acts as an MCP server, exposing memory
//!                   so other agents read the same facts (stdio, see joyczl-mcp/server.rs)
//!
//! Login is the only place a browser opens — at startup a missing token only warns
//! and skips that server, never popping a browser on its own: the same rule as
//! "a turn never executes on its own".

use std::path::Path;

use anyhow::Result;
use joyczl_mcp::oauth;
use joyczl_mcp::server::MemoryServer;

pub async fn run(home: &Path, args: &[String]) -> Result<()> {
    let config_path = home.join("mcp.json");
    match args.first().map(String::as_str) {
        None | Some("list") => list(&config_path),
        Some("login") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("用法：joy mcp login <服务器名>"))?;
            login(&config_path, home, name).await
        }
        Some("serve") => serve(home).await,
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：list（默认）、login <名>、serve。");
            Ok(())
        }
    }
}

/// `joy mcp serve`: opens the same state.db (WAL + busy_timeout; multi-process
/// coexistence is fine by design) and speaks MCP over stdio. Notices go to stderr.
async fn serve(home: &Path) -> Result<()> {
    let pool = joyczl_state::open(&home.join("state.db")).await?;
    eprintln!(
        "(joy) mcp serve：记忆工具已就绪（memory_search / remember / forget / list / episodes）"
    );
    MemoryServer::new(
        joyczl_state::Facts::new(pool.clone()),
        joyczl_state::Episodes::new(pool),
    )
    .run_stdio()
    .await
}

fn list(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        println!(
            "没有配置文件（{}）。MCP 未配置 —— 写一个再试。",
            config_path.display()
        );
        return Ok(());
    }
    let text = std::fs::read_to_string(config_path)?;
    let config: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} 不是合法 JSON：{e}", config_path.display()))?;
    let servers = config
        .get("servers")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if servers.is_empty() {
        println!("mcp.json 里没有配任何服务器。");
        return Ok(());
    }
    for spec in servers {
        let name = spec
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(没名字)");
        let target = spec
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|u| format!("url {u}"))
            .or_else(|| {
                spec.get("command")
                    .and_then(serde_json::Value::as_str)
                    .map(|c| format!("command {c}"))
            })
            .unwrap_or_else(|| "(不知道连什么)".to_string());
        let auth = if spec.get("oauth").and_then(serde_json::Value::as_bool) == Some(true) {
            "oauth（跑 joy mcp login 登录）".to_string()
        } else {
            spec.get("auth_env")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("无")
                .to_string()
        };
        println!("- {name}  {target}  鉴权：{auth}");
    }
    Ok(())
}

async fn login(config_path: &Path, home: &Path, name: &str) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("读不了 {}：{e}", config_path.display()))?;
    let config: serde_json::Value = serde_json::from_str(&text)?;
    let spec = config
        .get("servers")
        .and_then(serde_json::Value::as_array)
        .and_then(|servers| {
            servers
                .iter()
                .find(|s| s.get("name").and_then(serde_json::Value::as_str) == Some(name))
        })
        .ok_or_else(|| anyhow::anyhow!("mcp.json 里没有叫 '{name}' 的服务器"))?;

    let url = spec
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("服务器 '{name}' 不是 url 型的 —— 只有远程服务器能 OAuth 登录")
        })?;
    if spec.get("oauth").and_then(serde_json::Value::as_bool) != Some(true) {
        println!("注意：'{name}' 在 mcp.json 里没写 \"oauth\": true，按 OAuth 流程照常试。");
    }

    println!("  即将在浏览器里打开授权页……");
    let client = reqwest::Client::new();
    let open_browser = |url: &str| {
        println!("  如果浏览器没自己打开，手工去这个地址：\n  {url}\n");
        let browser = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "windows") {
            "cmd /c start"
        } else {
            "xdg-open"
        };
        // Failing to open (headless/SSH) is not an error — the URL above is the only path.
        let mut parts = browser.split_whitespace();
        if let Some(program) = parts.next() {
            let _ = std::process::Command::new(program)
                .args(parts)
                .arg(url)
                .spawn();
        }
    };
    let token = oauth::sign_in(&client, home, name, url, &open_browser)
        .await
        .map_err(anyhow::Error::msg)?;
    println!(
        "登录成功，token 已存到 {}",
        oauth::TokenStore::new(home, name).path().display()
    );
    println!(
        "（access_token 前 8 位：{}…）",
        &token[..token.len().min(8)]
    );
    Ok(())
}
