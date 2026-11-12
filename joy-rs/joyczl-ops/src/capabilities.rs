//! The capability center: skills and MCP connectors, as the files they are on this machine.
//!
//! The dashboard's sidebar entry (「能力中心」) lists both and lets a human manage the
//! ones they own. Two rules shape everything here:
//!
//! 1. **Built-in means greyed out.** A skill from `JOY_SKILL_DIRS` or a server from
//!    `JOY_MCP_CONFIG` ships with the install or the machine; deleting it from one
//!    machine's UI would silently unconfigure every machine that shares it. So those
//!    rows carry `source: "builtin"` and every write path refuses them — the user's own
//!    files (`<home>/skills/`, `<home>/mcp.json`) are the only writable ones.
//! 2. **The runtime is the truth.** The listing merges exactly like the app-server does
//!    (user file first, first wins on a same name), so the page cannot show a server
//!    the runtime dropped — or hide one it connected.
//!
//! Ops-local views, like `/api/fs`: neither the SDK nor the terminal needs "browse the
//! skill folders". Reads reuse `joyczl-memory`'s parser and `joyczl-mcp`'s config shape
//! — one definition of SKILL.md and mcp.json, not a second one for the browser.

//! The HTTP handlers for these live in `api.rs` (the HTTP face); this module is the
//! domain: what the rows are, and which writes a human may make.

use std::path::{Path, PathBuf};

use joyczl_mcp::config::{self, Config as McpConfig, ServerSpec};
use joyczl_memory::skills::{self, SkillLoader};
use serde::Deserialize;

/// One skill as the capability center shows it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRow {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    /// Present = also a scheduled job (see `joy schedule`).
    pub schedule: Option<String>,
    /// `false` = explicit `$name` references only, never keyword-triggered.
    pub allow_model_invocation: bool,
    pub dependencies: Vec<String>,
    /// `user` (`<home>/skills`) or `builtin` (`JOY_SKILL_DIRS` packs).
    pub source: String,
    pub folder: String,
}

/// One MCP connector as the capability center shows it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRow {
    pub name: String,
    /// `url` (Streamable HTTP) or `command` (stdio) — the shape decides, per config.rs.
    pub kind: String,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    /// `none` / `auth_env` (a variable name) / `oauth` (browser login).
    pub auth: String,
    pub auth_env: Option<String>,
    /// `user` (`<home>/mcp.json`) or `builtin` (`JOY_MCP_CONFIG` packs).
    pub source: String,
}

/// `GET /api/skills` — the skill list plus why it could not be read, if it could not.
#[derive(Debug, serde::Serialize)]
pub struct SkillsView {
    pub skills: Vec<SkillRow>,
    pub error: Option<String>,
}

/// `GET /api/mcp` — the connector list plus why the user's file could not be read.
#[derive(Debug, serde::Serialize)]
pub struct McpView {
    pub servers: Vec<McpRow>,
    pub error: Option<String>,
}

/// What every write answers: one line a person can read.
#[derive(Debug, serde::Serialize)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillInstallBody {
    /// SKILL.md 原文（粘贴）。
    #[serde(default)]
    pub text: String,
    /// 或者一个网址（GitHub blob / Gist 页面会自动转成 raw 内容）。
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct NameBody {
    #[serde(default)]
    pub name: String,
}

// ---- Reads ------------------------------------------------------------------

/// Every skill the loader would see, with where it came from.
///
/// Same scan and same priority as `SkillLoader` (extras first, home last → home wins on
/// a same name), plus the one thing the loader does not care about: **which directory**
/// a skill came from, because that is what decides "may a human delete this".
pub fn skill_rows(home: &Path) -> (Vec<SkillRow>, Option<String>) {
    skill_rows_in(&SkillLoader::dirs_for(home))
}

/// The pure core: `dirs[0]` is the user's directory, the rest are built-in packs.
pub fn skill_rows_in(dirs: &[PathBuf]) -> (Vec<SkillRow>, Option<String>) {
    let mut rows: std::collections::BTreeMap<String, SkillRow> = std::collections::BTreeMap::new();
    let mut error = None;
    let Some((user_dir, packs)) = dirs.split_first() else {
        return (Vec::new(), None);
    };

    // Extras first, home last: a same-named skill in home overwrites (the loader's rule).
    for (dir, builtin) in packs
        .iter()
        .map(|dir| (dir.as_path(), true))
        .chain(std::iter::once((user_dir.as_path(), false)))
    {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(why) => {
                // A missing pack is not news (it may not be installed here); the user's
                // own directory failing to open is.
                if !builtin && dir.exists() {
                    error = Some(format!("could not read {}: {why}", dir.display()));
                }
                continue;
            }
        };
        let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        for entry in entries.flatten() {
            let path = entry.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // not a skill folder — the loader skips it too
            };
            let Some(skill) = skills::parse_skill_text(&text) else {
                continue;
            };
            let folder = path.parent().unwrap_or(&path).to_path_buf();
            let canon = folder.canonicalize().unwrap_or_else(|_| folder.clone());
            // Same exclusions as the loader's export list: SKILL.md sitting in the root,
            // and the install staging area, are not skills.
            if canon == root || canon.file_name().is_some_and(|name| name == "_incoming") {
                continue;
            }
            rows.insert(
                skill.name.clone(),
                SkillRow {
                    name: skill.name,
                    description: skill.description,
                    version: skill.version,
                    schedule: skill.schedule,
                    allow_model_invocation: skill.allow_model_invocation,
                    dependencies: skill.dependencies,
                    source: if builtin { "builtin" } else { "user" }.to_string(),
                    folder: folder.to_string_lossy().to_string(),
                },
            );
        }
    }
    (rows.into_values().collect(), error)
}

/// Every configured MCP server, in the order the runtime connects them.
pub fn mcp_rows(home: &Path) -> (Vec<McpRow>, Option<String>) {
    let extra = std::env::var_os("JOY_MCP_CONFIG");
    mcp_rows_in(&config::config_paths_for(home, extra.as_deref()))
}

/// The pure core: `paths[0]` is the user's file, the rest are built-in packs.
/// First file wins on a same name — the merge `McpClient::connect_home` performs,
/// so a row shown here is a server that actually connects.
pub fn mcp_rows_in(paths: &[PathBuf]) -> (Vec<McpRow>, Option<String>) {
    let mut rows: Vec<McpRow> = Vec::new();
    let mut error = None;
    for (index, path) in paths.iter().enumerate() {
        let source = if index == 0 { "user" } else { "builtin" };
        // No user file is the default state, not an error; a missing pack likewise.
        if !path.exists() {
            continue;
        }
        match McpConfig::read(path) {
            Ok(config) => {
                for spec in &config.servers {
                    // A nameless entry connects to nothing (the loader skips it too).
                    if spec.name.is_empty() || rows.iter().any(|row| row.name == spec.name) {
                        continue;
                    }
                    rows.push(mcp_row_of(spec, source));
                }
            }
            Err(why) => {
                if index == 0 {
                    error = Some(why);
                } else {
                    // A broken pack must not blank the user's own list — but it must be
                    // said, the same way the app-server says it at connect time.
                    eprintln!("(joy) capability hub: {why}");
                }
            }
        }
    }
    (rows, error)
}

fn mcp_row_of(spec: &ServerSpec, source: &str) -> McpRow {
    let (kind, url, command) = match spec.url.as_deref().filter(|value| !value.is_empty()) {
        Some(url) => ("url".to_string(), Some(url.to_string()), None),
        None => ("command".to_string(), None, spec.command.clone()),
    };
    let auth = if spec.oauth.unwrap_or(false) {
        "oauth"
    } else if spec.auth_env.as_deref().is_some_and(|key| !key.is_empty()) {
        "auth_env"
    } else {
        "none"
    };
    McpRow {
        name: spec.name.clone(),
        kind,
        url,
        command,
        args: spec.args.clone().unwrap_or_default(),
        auth: auth.to_string(),
        auth_env: spec.auth_env.clone(),
        source: source.to_string(),
    }
}

// ---- Writes -----------------------------------------------------------------

/// Remove a skill the human added. Built-in ones are refused by construction: they do
/// not live under `<home>/skills`, and this only ever deletes there.
pub fn remove_skill(home: &Path, name: &str) -> Result<String, String> {
    if !skills::is_slug(name) {
        return Err(format!(
            "'{name}' is not a valid skill name (lowercase letters, digits, hyphens) — refusing to touch any directory"
        ));
    }
    let folder = home.join("skills").join(name);
    if !folder.is_dir() {
        return Err(format!(
            "no skill named '{name}' to delete — built-in skills (shipped with Joy or JOY_SKILL_DIRS) are not managed here"
        ));
    }
    std::fs::remove_dir_all(&folder)
        .map_err(|why| format!("could not delete {}: {why}", folder.display()))?;
    Ok(format!("Deleted skill '{name}'"))
}

/// Validate what the dashboard's form sent before it touches mcp.json.
///
/// `joyczl-mcp`'s `resolve` judges the shape; this adds what a **form** gets wrong that
/// a hand-written file usually doesn't: an empty name, a name with spaces or path
/// characters (it becomes every tool name's prefix), a url that is not http(s), an
/// `auth_env` that is not a variable name (it holds a name, never the key itself).
pub fn validate_server(spec: &ServerSpec) -> Result<(), String> {
    let name = spec.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err("the name must be 1-64 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("the name may only use letters, digits, hyphens and underscores — it becomes the tool-name prefix".to_string());
    }
    let url = spec
        .url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let command = spec
        .command
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (url, command) {
        (Some(_), Some(_)) => {
            return Err("set url or command, not both — one server, one transport".to_string());
        }
        (None, None) => {
            return Err(
                "set either url (a remote HTTP server) or command (a local stdio server)"
                    .to_string(),
            );
        }
        (Some(url), None) => {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("url must start with http:// or https://".to_string());
            }
        }
        (None, Some(_)) => {}
    }
    if let Some(key) = spec.auth_env.as_deref().filter(|value| !value.is_empty()) {
        if !key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(
                "the auth variable name may only use uppercase letters, digits and underscores — it is the env var's name, not the key itself"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// Add or replace one server in `<home>/mcp.json`. Never touches a `JOY_MCP_CONFIG`
/// pack — and says so when a pack already owns that name, because the runtime's
/// first-wins would make the user's edit silently do nothing.
pub fn save_server(home: &Path, incoming: &ServerSpec) -> Result<String, String> {
    let extra = std::env::var_os("JOY_MCP_CONFIG");
    let packs: Vec<PathBuf> = config::config_paths_for(home, extra.as_deref())
        .into_iter()
        .skip(1)
        .collect();
    save_server_among(home, incoming, &packs)
}

/// The pure core: `packs` are the read-only configs to check against.
pub fn save_server_among(
    home: &Path,
    incoming: &ServerSpec,
    packs: &[PathBuf],
) -> Result<String, String> {
    validate_server(incoming)?;
    let name = incoming.name.trim().to_string();
    for pack in packs {
        if let Ok(config) = McpConfig::read(pack) {
            if config.servers.iter().any(|spec| spec.name == name) {
                return Err(format!(
                    "'{name}' is already provided by the built-in config {} — a second copy in user config has no effect (first one wins)",
                    pack.display()
                ));
            }
        }
    }

    let path = home.join("mcp.json");
    let mut config = if path.exists() {
        McpConfig::read(&path)?
    } else {
        McpConfig::default()
    };
    // Normalize exactly what validation saw, so the file holds the trimmed values.
    let mut spec = incoming.clone();
    spec.name = name.clone();
    spec.url = spec
        .url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    spec.command = spec
        .command
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match config.servers.iter_mut().find(|row| row.name == name) {
        Some(existing) => *existing = spec,
        None => config.servers.push(spec),
    }
    write_config(&path, &config)?;
    Ok(format!("Saved MCP connector '{name}' → {}", path.display()))
}

/// Remove one server from `<home>/mcp.json`. A name that only exists in a built-in
/// pack is refused — that file is not ours to edit.
pub fn remove_server(home: &Path, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("the name may only use letters, digits, hyphens and underscores".to_string());
    }
    let path = home.join("mcp.json");
    if !path.exists() {
        return Err(format!(
            "there is no {} yet, so there is no connector to delete",
            path.display()
        ));
    }
    let mut config = McpConfig::read(&path)?;
    let before = config.servers.len();
    config.servers.retain(|spec| spec.name != name);
    if config.servers.len() == before {
        return Err(format!(
            "no connector named '{name}' in mcp.json — one from the built-in config cannot be deleted, and should not be"
        ));
    }
    write_config(&path, &config)?;
    Ok(format!("Deleted MCP connector '{name}'"))
}

/// Write mcp.json atomically: a half-written config disables **every** server on the
/// next boot (a broken file connects nothing at all). tmp + rename, same filesystem.
fn write_config(path: &Path, config: &McpConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|why| why.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &text)
        .map_err(|why| format!("could not write {}: {why}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|why| format!("could not switch to {}: {why}", path.display()))?;
    Ok(())
}
