//! `joy mcp` —— MCP 服务器的查看与登录。
//!
//! `joy mcp`         列出 mcp.json 里配的服务器和各自的鉴权方式
//! `joy mcp login X` 跑一遍浏览器 OAuth（发现 → 注册 → 授权 → 换 token），
//!                   token 落 `<home>/mcp-auth/X.json`（0600）
//!
//! 登录是唯一会开浏览器的地方 —— app-server 启动时缺 token 只会警告并
//! 跳过该服务器，绝不擅自弹浏览器：「一轮对话绝不擅自执行」的同一条规矩。

use std::path::Path;

use anyhow::Result;
use joyczl_mcp::oauth;

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
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：list（默认）、login <名>。");
            Ok(())
        }
    }
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
        // 打不开（headless/SSH）不是错误 —— 上面打印的 URL 就是唯一的路。
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
