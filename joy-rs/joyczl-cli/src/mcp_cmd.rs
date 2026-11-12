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
                .ok_or_else(|| anyhow::anyhow!("Usage: joy mcp login <server-name>"))?;
            login(&config_path, home, name).await
        }
        Some("serve") => serve(home).await,
        Some(other) => {
            println!("Unknown subcommand '{other}'. Options: list (default), login <name>, serve.");
            Ok(())
        }
    }
}

/// `joy mcp serve`: opens the same state.db (WAL + busy_timeout; multi-process
/// coexistence is fine by design) and speaks MCP over stdio. Notices go to stderr.
async fn serve(home: &Path) -> Result<()> {
    let pool = joyczl_state::open(&home.join("state.db")).await?;
    eprintln!(
        "(joy) mcp serve: memory tools are ready (memory_search / remember / forget / list / episodes)"
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
            "No config file ({}). MCP is not configured — write one and try again.",
            config_path.display()
        );
        return Ok(());
    }
    let text = std::fs::read_to_string(config_path)?;
    let config: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", config_path.display()))?;
    let servers = config
        .get("servers")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if servers.is_empty() {
        println!("No servers are configured in mcp.json.");
        return Ok(());
    }
    for spec in servers {
        let name = spec
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no name)");
        let target = spec
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|u| format!("url {u}"))
            .or_else(|| {
                spec.get("command")
                    .and_then(serde_json::Value::as_str)
                    .map(|c| format!("command {c}"))
            })
            .unwrap_or_else(|| "(no target)".to_string());
        let auth = if spec.get("oauth").and_then(serde_json::Value::as_bool) == Some(true) {
            "oauth (run joy mcp login)".to_string()
        } else {
            spec.get("auth_env")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("none")
                .to_string()
        };
        println!("- {name}  {target}  auth: {auth}");
    }
    Ok(())
}

async fn login(config_path: &Path, home: &Path, name: &str) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Could not read {}: {e}", config_path.display()))?;
    let config: serde_json::Value = serde_json::from_str(&text)?;
    let spec = config
        .get("servers")
        .and_then(serde_json::Value::as_array)
        .and_then(|servers| {
            servers
                .iter()
                .find(|s| s.get("name").and_then(serde_json::Value::as_str) == Some(name))
        })
        .ok_or_else(|| anyhow::anyhow!("There is no server named '{name}' in mcp.json"))?;

    let url = spec
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Server '{name}' is not a url server — only remote servers can sign in over OAuth"
            )
        })?;
    if spec.get("oauth").and_then(serde_json::Value::as_bool) != Some(true) {
        println!("Note: '{name}' does not set \"oauth\": true in mcp.json; trying the OAuth flow anyway.");
    }

    println!("  Opening the authorisation page in the browser…");
    let client = reqwest::Client::new();
    let open_browser = |url: &str| {
        println!("  If the browser did not open, go to this address by hand:\n  {url}\n");
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
        "Signed in; the token is stored in {}",
        oauth::TokenStore::new(home, name).path().display()
    );
    println!(
        "(first 8 characters of access_token: {}…)",
        &token[..token.len().min(8)]
    );
    Ok(())
}
