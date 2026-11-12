//! `joy skill` — viewing, carrying, and installing skills.
//!
//! `joy skill list`                      shows which skills Joy has loaded (with versions)
//! `joy skill export --to claude,codex`  copies skills to other agents (same SKILL.md format)
//! `joy skill install <url|path>`        installs another's skill (frontmatter + name checks)
//! `joy skill update [index]`            upgrades installed skills to new versions per
//!                                       the index (defaults to <home>/skills/index.json,
//!                                       or a local path or http(s) URL)
//!
//! The install/update split is worth stating: **install never overwrites** (skills
//! are instructions — read before installing, reject on name clash); **update
//! replaces by version** (old backed up to `.backup/`, new staged in `.staging/`).
//!
//! export's rule: **a modified copy on the other side is kept by default** — it may
//! be their edits inside another agent; --force overwrites. install's rule: **never
/// overwrite an existing skill**; skills are markdown instructions, read before installing.
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
                .ok_or_else(|| anyhow::anyhow!("Usage: joy skill install <url or local path>"))?;
            install(home, source).await
        }
        Some("update") => update(home, args.get(1).map(String::as_str)).await,
        Some(other) => {
            println!(
                "Unknown subcommand '{other}'. Options: list (default), export, install, update."
            );
            Ok(())
        }
    }
}

fn list(home: &Path) -> Result<()> {
    let loaded = skills::loaded_skills(home);
    if loaded.is_empty() {
        println!(
            "No skills yet. Drop a SKILL.md under {}, or joy skill install <url>.",
            home.join("skills").display()
        );
        return Ok(());
    }
    let loaded_names: Vec<String> = loaded.iter().map(|s| s.name.clone()).collect();
    for skill in loaded {
        let version = skill
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        // Both policy fields print directly: the top question after install is "why
        // wasn't it triggered" — the answer is here (no implicit trigger / deps absent).
        let mut notes = Vec::new();
        if !skill.allow_model_invocation {
            notes.push("only via an explicit $name reference".to_string());
        }
        let missing: Vec<&str> = skill
            .dependencies
            .iter()
            .filter(|dep| !loaded_names.iter().any(|name| name == *dep))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            notes.push(format!("missing dependencies: {}", missing.join(", ")));
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join("；"))
        };
        println!("- {}{version}  {}{note}", skill.name, skill.description);
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
                    .ok_or_else(|| anyhow::anyhow!("--to needs a target"))?
                    .clone();
            }
            "--project" => project = true,
            "--force" => force = true,
            "--names" => {
                i += 1;
                names = args
                    .get(i)
                    .map(|v| v.split(',').map(str::trim).map(String::from).collect())
                    .ok_or_else(|| anyhow::anyhow!("--names needs comma-separated skill names"))?;
            }
            other => anyhow::bail!("Unknown argument '{other}'"),
        }
        i += 1;
    }

    // claude → ~/.claude (or ./.claude), codex → ~/.codex. --project lands under
    // the current directory, for "skills specific to this project".
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
            other => Err(anyhow::anyhow!(
                "Unknown target '{other}'. Options: claude, codex"
            )),
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
        println!("No export record matches {}.", names.join(", "));
    }
    for line in filtered {
        println!("{line}");
    }
    Ok(())
}

async fn install(home: &Path, source: &str) -> Result<()> {
    // 抓取、校验、落盘都在 joyczl_memory::skills 里（驾驶舱的能力中心走同一条路）——
    // 三个入口各写一遍 fetch 规则，迟早对不上。
    let outcome = if source.starts_with("http://") || source.starts_with("https://") {
        println!("Fetching {source}");
        skills::install_from_url(home, source).await
    } else {
        let text = std::fs::read_to_string(source)?;
        skills::install_from_text(home, &text)
    }
    .map_err(anyhow::Error::msg)?;
    println!("{outcome}");
    println!(
        "Effective on the next start. Skills are instructions — read one before installing it."
    );
    Ok(())
}

fn dirs_home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        anyhow::anyhow!("HOME could not be read; use --project or name it explicitly")
    })?))
}

/// Update per index: parse index → fetch each entry → validate → compare versions
/// → backup + atomic replace. Fetching, validating and replacing live in
/// `joyczl_memory::install`; this only finds the index and wires up the network.
async fn update(home: &Path, index_arg: Option<&str>) -> Result<()> {
    let default_index = home.join("skills").join("index.json");
    let source = index_arg
        .map(str::to_string)
        .unwrap_or_else(|| default_index.display().to_string());

    let text = if is_http(&source) {
        skills::fetch_text(&source)
            .await
            .map_err(anyhow::Error::msg)?
    } else {
        std::fs::read_to_string(&source)
            .map_err(|e| anyhow::anyhow!("Could not read the index {source}: {e}"))?
    };

    let (entries, warnings) = installer::parse_index(&text).map_err(anyhow::Error::msg)?;
    for warning in &warnings {
        eprintln!("(joy) {warning}");
    }
    if entries.is_empty() {
        println!("The index has no usable skill entries.");
        return Ok(());
    }

    let outcomes = installer::update_all(home, &entries, |url: String| async move {
        if is_http(&url) {
            skills::fetch_text(&url).await
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
        anyhow::bail!("{failed} skills did not update (the reasons are printed above)");
    }
    Ok(())
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
