//! `joy skill` —— 技能的查看、携带与安装。
//!
//! `joy skill list`                      看 Joy 装载了哪些技能（含版本）
//! `joy skill export --to claude,codex`  把技能复制给别的 agent（同一份 SKILL.md 格式）
//! `joy skill install <url|路径>`        装一个别人的技能（校验 frontmatter，重名拒绝）
//! `joy skill update [索引]`             按索引把已装的技能升到新版本
//!                                       （索引默认 <home>/skills/index.json，
//!                                        也可以给本地路径或 http(s) 地址）
//!
//! install 与 update 的分工值得说清楚：**install 从不覆盖**（技能是指令，
//! 装之前先读一遍，重名就拒绝），**update 按版本替换**（老版本先备份进
//! `.backup/`，新版本先落 `.staging/` 再原子换过去）。
//!
//! export 的规矩：**对方手里被改过的副本默认保留** —— 那可能是人家在
//! 另一个 agent 里做的修改，--force 才覆盖。install 的规矩：**从不覆盖
/// 已有技能**；技能是 markdown 指令，装之前先读一遍。
use std::path::{Path, PathBuf};

use anyhow::Result;

use joyczl_memory::install as installer;
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
        Some("update") => update(home, args.get(1).map(String::as_str)).await,
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：list（默认）、export、install、update。");
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
        let version = skill.version.map(|v| format!(" v{v}")).unwrap_or_default();
        println!("- {}{version}  {}", skill.name, skill.description);
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

/// 按索引更新：解析索引 → 逐条取回内容 → 校验 → 版本比对 → 备份 + 原子替换。
/// 取回、校验、替换都在 `joyczl_memory::install`，这里只负责把索引找出来、
/// 把网络接上去。
async fn update(home: &Path, index_arg: Option<&str>) -> Result<()> {
    let default_index = home.join("skills").join("index.json");
    let source = index_arg
        .map(str::to_string)
        .unwrap_or_else(|| default_index.display().to_string());

    let text = if is_http(&source) {
        fetch(&source).await.map_err(anyhow::Error::msg)?
    } else {
        std::fs::read_to_string(&source).map_err(|e| anyhow::anyhow!("读不到索引 {source}：{e}"))?
    };

    let (entries, warnings) = installer::parse_index(&text).map_err(anyhow::Error::msg)?;
    for warning in &warnings {
        eprintln!("(joy) {warning}");
    }
    if entries.is_empty() {
        println!("索引里没有可用的技能条目。");
        return Ok(());
    }

    let outcomes = installer::update_all(home, &entries, |url: String| async move {
        if is_http(&url) {
            fetch(&url).await
        } else {
            std::fs::read_to_string(&url).map_err(|e| e.to_string())
        }
    })
    .await;

    let mut failed = 0;
    for outcome in &outcomes {
        println!("{}", outcome.line());
        if !outcome.ok() {
            failed += 1;
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} 个技能没更新成功（上面逐条写了原因）");
    }
    Ok(())
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 取一份技能内容。网页地址先转成原始内容地址（跟 install 同一条规矩：
/// 人手复制来的多半是浏览器地址栏里那个）。
async fn fetch(url: &str) -> Result<String, String> {
    let raw = raw_url(url);
    let response = reqwest::Client::new()
        .get(&raw)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(format!("{raw}：HTTP {status}"));
    }
    Ok(body)
}
