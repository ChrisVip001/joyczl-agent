//! Procedural memory — SKILL.md: how to do things, loaded only when relevant.
//!
//! The official Anthropic Agent Skills format: YAML frontmatter with `name` and
//! `description` (doubles as the trigger — no custom `triggers` field).
//!
//! Progressive disclosure, the parts that really matter:
//!   1. every skill's frontmatter is always scanned (cheap);
//!   2. a skill's **body** enters the prompt only when it matches the message;
//!   3. files a skill references are read only when the model asks for them.
//!
//! Triggering is **transparent** — keyword overlap between the message and
//! name+description; no embedding, no magic, the score is computable by hand.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One parsed skill. `body` is read into the prompt only on a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
    /// Optional five-field cron (`0 8 * * 1-5`): such a skill is also a **scheduled
    /// job**, fired on time by the `joy schedule` resident process (see cli schedule.rs).
    pub schedule: Option<String>,
    /// Optional version (`version: 1.2.0`): `joy skill update` uses it to decide on replacement.
    pub version: Option<String>,
    /// `allow-model-invocation: false` — the author says "don't let the model use me
    /// on its own"; such skills only respond to explicit `$skillname` references. Default true.
    pub allow_model_invocation: bool,
    /// `dependencies: a, b` — which skills must be present to fulfill this one. A skill
    /// with missing deps skips implicit triggering (its body assumes others paved the way).
    pub dependencies: Vec<String>,
}

/// Parses SKILL.md text (loader and the create_skill tool share one validation).
///
/// The shape must be:
/// ```text
/// ---
/// name: weekly-review
/// description: ...
/// ---
/// body text…
/// ```
/// No `name` or `description` in the frontmatter → not a skill.
pub fn parse_skill_text(text: &str) -> Option<Skill> {
    // The header rules (CRLF, quotes, "empty means absent") live in `frontmatter` so the
    // memory pages and the skills cannot drift apart.
    let parsed = crate::frontmatter::split(text)?;
    // Both spellings accepted: an underscore form silently doing nothing is a bug you'd
    // need the source to find.
    let allow_model_invocation =
        parsed.flag("allow-model-invocation", true) && parsed.flag("allow_model_invocation", true);
    Some(Skill {
        name: parsed.get("name")?.to_string(),
        description: parsed.get("description")?.to_string(),
        body: parsed.body.trim().to_string(),
        path: PathBuf::new(),
        schedule: parsed.get("schedule").map(str::to_string),
        version: parsed.get("version").map(str::to_string),
        allow_model_invocation,
        dependencies: parsed.list("dependencies"),
    })
}

fn parse_file(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut skill = parse_skill_text(&text)?;
    skill.path = path.to_path_buf();
    Some(skill)
}

/// Tokenizer shared by messages and skill descriptions: lowercase ASCII alphanumeric
/// runs, ≥3 chars. Chinese excluded — triggers belong in the description, per format.
fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    let mut run = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            run.push(c);
        } else if !run.is_empty() {
            if run.chars().count() >= 3 {
                out.push(std::mem::take(&mut run));
            } else {
                run.clear();
            }
        }
    }
    if run.chars().count() >= 3 {
        out.push(run);
    }
    out
}

/// Scans the skill directories. Rescans automatically on directory changes (new or
/// edited SKILL.md), so a skill written mid-session by create_skill applies next turn.
pub struct SkillLoader {
    dirs: Vec<PathBuf>,
    skills: Vec<Skill>,
    sig: Vec<(PathBuf, SystemTime)>,
}

impl SkillLoader {
    /// Directory list: `home/skills` (installed + agent-written), plus the
    /// colon-separated extra dirs in `JOY_SKILL_DIRS` (bundled / community packs).
    pub fn dirs_for(home: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![home.join("skills")];
        if let Some(extra) = std::env::var_os("JOY_SKILL_DIRS") {
            dirs.extend(std::env::split_paths(&extra));
        }
        dirs
    }

    pub fn new(dirs: Vec<PathBuf>) -> Self {
        let mut loader = Self {
            dirs,
            skills: Vec::new(),
            sig: Vec::new(),
        };
        loader.refresh();
        loader
    }

    /// Directory fingerprint: each SKILL.md's path + mtime.
    fn scan_sig(&self) -> Vec<(PathBuf, SystemTime)> {
        let mut sig = Vec::new();
        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if let Ok(meta) = std::fs::metadata(&path) {
                    sig.push((path, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
        sig.sort();
        sig
    }

    pub fn refresh(&mut self) {
        self.skills.clear();
        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if let Some(skill) = parse_file(&path) {
                    self.skills.push(skill);
                }
            }
        }
        self.skills.sort_by_key(|s| s.name.clone());
        self.sig = self.scan_sig();
    }

    /// Currently loaded skills (the name + description layer, no bodies).
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Transparent triggering: a hit needs keyword overlap ≥ 2 between the message
    /// and name+description; the top `max_skills` by overlap win. Rescans first if
    /// the directories changed.
    ///
    /// Two kinds skip implicit triggering: `allow-model-invocation: false` skills,
    /// and skills with missing dependencies (see `dangling_dependencies`).
    pub fn match_message(&mut self, message: &str, max_skills: usize) -> Vec<&Skill> {
        if self.scan_sig() != self.sig {
            self.refresh();
        }
        let loaded: Vec<String> = self.skills.iter().map(|s| s.name.clone()).collect();
        let msg_words: std::collections::HashSet<String> = tokens(message).into_iter().collect();
        let mut scored: Vec<(usize, &Skill)> = self
            .skills
            .iter()
            .filter(|skill| skill.allow_model_invocation)
            .filter(|skill| missing_dependency(skill, &loaded).is_none())
            .map(|skill| {
                let mut skill_words = tokens(&skill.name);
                skill_words.extend(tokens(&skill.description));
                let overlap = skill_words
                    .iter()
                    .filter(|w| msg_words.contains(*w))
                    .count();
                (overlap, skill)
            })
            .filter(|(overlap, _)| *overlap >= 2)
            .collect();
        scored.sort_by_key(|(overlap, _)| std::cmp::Reverse(*overlap));
        scored
            .into_iter()
            .take(max_skills)
            .map(|(_, s)| s)
            .collect()
    }

    /// The catalog that rides along on every turn: one line per skill, `name — description`,
    /// until the token budget runs out.
    ///
    /// Why a catalog and not keyword triggering alone: overlap scoring only fires when the
    /// user happens to use the words in the description. A model that can *see* the shelf
    /// picks the right folder even when the phrasing is new. The budget is the guard rail —
    /// a catalog nobody prunes becomes the context problem it was meant to solve.
    ///
    /// Two marks worth carrying: a skill whose author said "explicit `$name` only", and one
    /// whose dependencies are missing. Offering a skill the model cannot actually use costs
    /// a turn to discover.
    pub fn index_text(&mut self, max_tokens: usize) -> String {
        if self.scan_sig() != self.sig {
            self.refresh();
        }
        let loaded: Vec<String> = self.skills.iter().map(|s| s.name.clone()).collect();
        let mut out = String::new();
        let mut spent = 0usize;
        let mut omitted = 0usize;
        for skill in &self.skills {
            let mut marks = Vec::new();
            if !skill.allow_model_invocation {
                marks.push("explicit $name only".to_string());
            }
            if let Some(missing) = missing_dependency(skill, &loaded) {
                marks.push(format!("needs {missing}"));
            }
            let line = if marks.is_empty() {
                format!("- {} — {}", skill.name, skill.description)
            } else {
                format!(
                    "- {} — {} ({})",
                    skill.name,
                    skill.description,
                    marks.join(", ")
                )
            };
            let cost = joyczl_provider::tokens::estimate_text(&line) + 1;
            if spent + cost > max_tokens {
                omitted += 1;
                continue;
            }
            spent += cost;
            out.push_str(&line);
            out.push('\n');
        }
        if omitted > 0 {
            out.push_str(&format!("({omitted} more skills not listed)\n"));
        }
        out
    }

    /// One skill's body, for the `read_skill` tool. `None` = no such skill.
    pub fn body_of(&mut self, name: &str) -> Option<String> {
        if self.scan_sig() != self.sig {
            self.refresh();
        }
        self.skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.body.clone())
    }

    /// The result of building the skill section.
    pub fn hits(&mut self, message: &str) -> SkillHits {
        self.hits_capped(message, usize::MAX)
    }

    /// Same, with a ceiling on one body's text (`JOY_SKILL_BODY_TOKENS`).
    ///
    /// Truncation is by whole lines and always says so: a body cut mid-sentence teaches the
    /// model a broken procedure and it never learns why. Over the cap, the model is told to
    /// call `read_skill` for the rest — which returns the body in full, because a request it
    /// made on purpose is not the same as text forced into every turn.
    pub fn hits_capped(&mut self, message: &str, body_tokens: usize) -> SkillHits {
        if self.scan_sig() != self.sig {
            self.refresh();
        }

        // Explicit references: `$name` force-loads that skill's body without keyword
        // overlap — after `allow-model-invocation` is off, this is the **only**
        // entrance. Collect **names** first: `match_message` takes a mutable borrow.
        let mut chosen: Vec<String> = Vec::new();
        let mut hints: Vec<String> = Vec::new();
        let mut stripped = String::new();
        for word in message.split_whitespace() {
            let Some(raw) = word.strip_prefix('$') else {
                stripped.push_str(word);
                stripped.push(' ');
                continue;
            };
            let name = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
            if self.skills.iter().any(|skill| skill.name == name) {
                chosen.push(name.to_string());
            } else {
                // No error (the user may have just typed a $), but the model must know,
                // so it can reply "no skill named x" instead of ignoring it.
                hints.push(format!("用户引用了 ${name}，但没有叫 '{name}' 的技能。"));
            }
        }
        let stripped = stripped.trim().to_string();

        for skill in self.match_message(&stripped, 2) {
            if !chosen.iter().any(|name| name == &skill.name) {
                chosen.push(skill.name.clone());
            }
        }

        // Order is priority: explicit references come first.
        let section = chosen
            .iter()
            .filter_map(|name| self.skills.iter().find(|skill| &skill.name == name))
            .map(|s| format!("### {}\n{}", s.name, cap_body(&s.body, body_tokens)))
            .collect::<Vec<_>>()
            .join("\n\n");
        // The hit log feeds the memory center's "how often did this skill actually fire" —
        // the only honest way to answer "is this skill worth keeping".
        self.record_hits(&chosen);

        SkillHits {
            section,
            message: if stripped.is_empty() {
                // The whole message was one reference (`$weekly-review`): stripping leaves
                // it empty, so hand the reference back rather than an empty message.
                message.to_string()
            } else {
                stripped
            },
            hints,
        }
    }

    /// Append one line per hit to `<home>/skill-hits.jsonl`.
    ///
    /// A file rather than a table: this is telemetry, it never gates anything, and a bad
    /// line costs one datapoint instead of a failed turn. Each append is a single small
    /// write, so two processes sharing a home interleave lines but never split one.
    fn record_hits(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        // `dirs[0]` is always `<home>/skills` (see `dirs_for`), so its parent is the home.
        let Some(home) = self.dirs.first().and_then(|dir| dir.parent()) else {
            return;
        };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("skill-hits.jsonl"))
        else {
            return;
        };
        use std::io::Write;
        let at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        for name in names {
            let quoted = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
            let _ = writeln!(file, "{{\"name\":{quoted},\"at\":\"{at}\"}}");
        }
    }

    /// A dependency points at a missing (or uninstalled) skill: returns the missing name.
    pub fn dangling_dependencies(&mut self) -> Vec<(String, String)> {
        if self.scan_sig() != self.sig {
            self.refresh();
        }
        let loaded: Vec<String> = self.skills.iter().map(|s| s.name.clone()).collect();
        self.skills
            .iter()
            .filter_map(|skill| {
                missing_dependency(skill, &loaded).map(|missing| (skill.name.clone(), missing))
            })
            .collect()
    }
}

/// The result of building the skill section.
#[derive(Debug, Clone, Default)]
pub struct SkillHits {
    /// The section spliced into the system prompt. Empty = no hits; the caller then skips the header.
    pub section: String,
    /// The message with `$skillname` **stripped** (references are for the loader, not the model).
    pub message: String,
    /// Hints for explicit references to nonexistent skills, for the model to reply to.
    pub hints: Vec<String>,
}

/// Cut one skill body at a whole-line boundary when it exceeds `max_tokens`, and say so.
///
/// Whole lines, not characters: a procedure cut mid-sentence teaches a broken recipe and
/// never says why. The marker names the tool that returns the rest — and `read_skill`
/// deliberately does not apply this cap, because text the model asked for on purpose is not
/// the same as text forced into every turn.
fn cap_body(body: &str, max_tokens: usize) -> String {
    if max_tokens == usize::MAX || joyczl_provider::tokens::estimate_text(body) <= max_tokens {
        return body.to_string();
    }
    let mut out = String::new();
    let mut spent = 0usize;
    for line in body.lines() {
        let cost = joyczl_provider::tokens::estimate_text(line) + 1;
        if spent + cost > max_tokens {
            break;
        }
        spent += cost;
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[body truncated at {max_tokens} tokens — call read_skill for the rest]"
    ));
    out
}

/// Which of this skill's dependencies is absent.
fn missing_dependency(skill: &Skill, loaded: &[String]) -> Option<String> {
    skill
        .dependencies
        .iter()
        .find(|dep| !loaded.iter().any(|name| name == *dep))
        .cloned()
}

/// One row of `joy skill list`.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub folder: PathBuf,
    /// Present means this is a scheduled job (see the cli's schedule.rs).
    pub schedule: Option<String>,
    /// Present lets `joy skill update` recognize versions.
    pub version: Option<String>,
    /// The author says "don't let the model auto-use me" (explicit `$skillname` only).
    pub allow_model_invocation: bool,
    /// Which skills must be present.
    pub dependencies: Vec<String>,
}

/// All loaded skills, deduped by name; a same-name skill in `home/skills` beats other
/// directories — same priority as the loader. SKILL.md files sitting directly in the
/// skills root, or in `_incoming` (install's staging area), don't count as exportable.
pub fn loaded_skills(home: &Path) -> Vec<LoadedSkill> {
    let dirs = SkillLoader::dirs_for(home);
    let mut by_name: std::collections::BTreeMap<String, LoadedSkill> =
        std::collections::BTreeMap::new();
    // home inserted last → same-name overwrite (beats the JOY_SKILL_DIRS entries).
    for dir in dirs.iter().skip(1).chain(dirs.first()) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let root = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        for entry in entries.flatten() {
            let path = entry.path().join("SKILL.md");
            let Some(skill) = parse_file(&path) else {
                continue;
            };
            let folder = skill.path.parent().unwrap_or(&path).to_path_buf();
            let canon = folder.canonicalize().unwrap_or_else(|_| folder.clone());
            if canon == root || canon.file_name().is_some_and(|n| n == "_incoming") {
                continue;
            }
            by_name.insert(
                skill.name.clone(),
                LoadedSkill {
                    name: skill.name,
                    description: skill.description,
                    folder,
                    schedule: skill.schedule,
                    version: skill.version,
                    allow_model_invocation: skill.allow_model_invocation,
                    dependencies: skill.dependencies,
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// Recursively collects all files of a skill folder (relative path → bytes), skipping
/// junk. export uses it both for the "modified on the other side" diff and to copy.
fn collect_files(folder: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    fn walk(folder: &Path, base: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == ".DS_Store" || name == "__pycache__" {
                continue;
            }
            if path.is_dir() {
                walk(&path, base, files);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                files.insert(relative, bytes);
            }
        }
    }
    walk(folder, folder, &mut files);
    files
}

/// The body of `joy skill export`: copies each skill folder to
/// `<base>/<target>/skills/<name>/`. A copy **modified on the other side is kept by
/// default** (only `force` overwrites) — it may be their edits in another agent.
/// Returns line-by-line human-readable output.
pub fn export_skills(home: &Path, targets: &[(String, PathBuf)], force: bool) -> Vec<String> {
    let skills = loaded_skills(home);
    let mut lines = Vec::new();
    for root in targets.iter().map(|(_, root)| root) {
        let root = root.join("skills");
        for skill in &skills {
            let dest = root.join(&skill.name);
            let same = dest.is_dir() && collect_files(&dest) == collect_files(&skill.folder);
            if same {
                lines.push(format!("  unchanged   {}", dest.display()));
                continue;
            }
            if dest.exists() && !force {
                lines.push(format!(
                    "  kept yours  {}（内容有出入；--force 覆盖）",
                    dest.display()
                ));
                continue;
            }
            if dest.exists() {
                let _ = std::fs::remove_dir_all(&dest);
            }
            match copy_dir(&skill.folder, &dest) {
                Ok(()) => lines.push(format!("  copied      {}", dest.display())),
                Err(e) => lines.push(format!("  FAILED      {}：{e}", dest.display())),
            }
        }
    }
    lines
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for (relative, bytes) in collect_files(src) {
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)?;
    }
    Ok(())
}

/// A skill name must be a slug: lowercase letters, digits, hyphens, not starting or
/// ending with a hyphen.
///
/// **One source of truth**: the name becomes a directory (`<home>/skills/<n>/`), and
/// all three write paths — `create_skill`, `joy skill install`, `joy skill update` —
/// must pass this gate. Three copies **diverged once** (one allowed edge hyphens,
/// others didn't) — inconsistency across entrances is harder to debug than any rule.
pub fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// The body of `joy skill install`: receives SKILL.md **text** (fetching is the
/// caller's job), validates frontmatter (same set as create_skill), and lands it at
/// `<home>/skills/<name>/SKILL.md`. Name clashes are always rejected — never overwrite.
pub fn install_from_text(home: &Path, text: &str) -> Result<String, String> {
    let skill = parse_skill_text(text).ok_or_else(|| {
        "不是合法的技能：SKILL.md 需要 YAML frontmatter，含 name 和 description。".to_string()
    })?;
    if !is_slug(&skill.name) {
        return Err(format!(
            "技能名 '{}' 不是小写 slug（小写字母、数字、连字符），拒绝安装 —— 它要变成目录名。",
            skill.name
        ));
    }
    let dest_dir = home.join("skills").join(&skill.name);
    let dest = dest_dir.join("SKILL.md");
    if dest.exists() {
        return Err(format!(
            "已有一个叫 '{}' 的技能 —— 换个名字或先删掉旧的。",
            skill.name
        ));
    }
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    std::fs::write(&dest, text).map_err(|e| e.to_string())?;
    Ok(format!(
        "已安装 '{}' → {}\n  {}",
        skill.name,
        dest.display(),
        skill.description
    ))
}

/// `joy skill install <url>` in one call: fetch → validate → land. Shared by the CLI and
/// the dashboard's capability center — three entrances into "install a skill" would
/// otherwise drift on the fetch rules (which is exactly what the raw-URL rewrite is).
pub async fn install_from_url(home: &Path, url: &str) -> Result<String, String> {
    let text = fetch_text(url).await?;
    install_from_text(home, &text)
}

/// Fetch a URL's body as text. GitHub blob and Gist pages are rewritten to their
/// raw-content URLs first: what people copy is the browser address bar, and fetching
/// that HTML would only ever produce "not a skill".
pub async fn fetch_text(url: &str) -> Result<String, String> {
    let raw = raw_url(url);
    let response = reqwest::Client::new()
        .get(&raw)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("拉不动 {raw}：{e}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| format!("读不动 {raw} 的内容：{e}"))?;
    if status >= 400 {
        return Err(format!("{raw}：HTTP {status}"));
    }
    Ok(text)
}

/// GitHub blob and Gist pages → raw content URLs. Other URLs returned as-is.
pub fn raw_url(url: &str) -> String {
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
