//! `joy skill` —— 技能的查看、携带与安装。
//!
//! `joy skill list`                      看 Joy 装载了哪些技能
//! `joy skill export --to claude,codex`  把技能复制给别的 agent（同一份 SKILL.md 格式）
//! `joy skill install <url|路径>`        装一个别人的技能（校验 frontmatter，重名拒绝）
//!
//! export 的规矩：**对方手里被改过的副本默认保留** —— 那可能是人家在
//! 另一个 agent 里做的修改，--force 才覆盖。install 的规矩：**从不覆盖
/// 已有技能**；技能是 markdown 指令，装之前先读一遍。
use std::path::{Path, PathBuf};

use anyhow::Result;

use joyczl_memory::skills;

pub async fn run(home: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("list") => list(home),
        Some("export") => export(home, &args[1..]),
        Some("install") => {
            let source = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("用法：joy skill install <url 或本地路径>"))?;
            install(home, source).await
        }
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：list（默认）、export、install。");
            Ok(())
        }
    }
}

fn list(home: &Path) -> Result<()> {
    let loaded = skills::loaded_skills(home);
    if loaded.is_empty() {
        println!(
            "还没有技能。放一个 SKILL.md 到 {} 下，或 joy skill install <url>。",
            home.join("skills").display()
        );
        return Ok(());
    }
    for skill in loaded {
        println!("- {}  {}", skill.name, skill.description);
    }
    Ok(())
}

fn export(home: &Path, args: &[String]) -> Result<()> {
    let mut to = "claude".to_string();
    let mut project = false;
    let mut force = false;
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                i += 1;
                to = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--to 后面要跟目标"))?
                    .clone();
            }
            "--project" => project = true,
            "--force" => force = true,
            "--names" => {
                i += 1;
                names = args
                    .get(i)
                    .map(|v| v.split(',').map(str::trim).map(String::from).collect())
                    .ok_or_else(|| anyhow::anyhow!("--names 后面要跟逗号分隔的技能名"))?;
            }
            other => anyhow::bail!("不认识的参数 '{other}'"),
        }
        i += 1;
    }

    // claude → ~/.claude（或 ./.claude），codex → ~/.codex。--project 落在
    // 当前目录下，给「这个项目专用的技能」用。
    let base: PathBuf = if project {
        std::env::current_dir()?
    } else {
        dirs_home()?
    };
    let targets: Vec<(String, PathBuf)> = to
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| match t {
            "claude" => Ok((t.to_string(), base.join(".claude"))),
            "codex" => Ok((t.to_string(), base.join(".codex"))),
            other => Err(anyhow::anyhow!("未知目标 '{other}'。可选：claude, codex")),
        })
        .collect::<Result<Vec<_>>>()?;

    let lines = skills::export_skills(home, &targets, force);
    if names.is_empty() {
        for line in &lines {
            println!("{line}");
        }
        return Ok(());
    }
    let filtered: Vec<&String> = lines
        .iter()
        .filter(|line| names.iter().any(|name| line.contains(name)))
        .collect();
    if filtered.is_empty() {
        println!("没有匹配 {} 的导出记录。", names.join(", "));
    }
    for line in filtered {
        println!("{line}");
    }
    Ok(())
}

async fn install(home: &Path, source: &str) -> Result<()> {
    // GitHub 页面 / Gist 的网页地址转成原始内容地址 —— 人复制来的多半是
    // 浏览器地址栏里那个。
    let text = if source.starts_with("http://") || source.starts_with("https://") {
        let raw = raw_url(source);
        println!("拉取 {raw}");
        reqwest::Client::new()
            .get(&raw)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
    } else {
        std::fs::read_to_string(source)?
    };
    let outcome = skills::install_from_text(home, &text).map_err(anyhow::Error::msg)?;
    println!("{outcome}");
    println!("下次启动即生效。技能是指令 —— 装之前先读一遍。");
    Ok(())
}

/// GitHub blob 页与 Gist 页 → 原始内容地址。别的 URL 原样返回。
fn raw_url(url: &str) -> String {
    if url.contains("github.com") && url.contains("/blob/") {
        return url
            .replace("github.com", "raw.githubusercontent.com")
            .replace("/blob/", "/");
    }
    if url.contains("gist.github.com") && !url.ends_with("/raw") {
        return format!("{}/raw", url.trim_end_matches('/'));
    }
    url.to_string()
}

fn dirs_home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        anyhow::anyhow!("读不到 HOME，--project 或手动指定")
    })?))
}
