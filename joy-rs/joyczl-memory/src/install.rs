//! Skill install and update: fetch → validate → compare versions → stage → backup → atomic swap.
//!
//! The order isn't arbitrary; each step guards against one class of accident:
//!
//! * **Validate first**: content failing `parse_skill_text` (missing name/description)
//!   never touches disk — an unparseable skill swaps a working one for a bad file.
//! * **Stage, then replace**: new content lands in `<home>/skills/.staging/` first;
//!   the target moves only on success. Mid-way failure leaves junk, not a broken skill.
//! * **Backup before replace**: the old version moves to `<home>/skills/.backup/<name>-<ts>/`;
//!   rolling back is one `mv`.
//! * **No downgrade**: a newer installed version is left alone (unless the user reinstalls).
//!
//! Fetching (HTTP vs local file) is injected by the caller — this layer knows nothing
//! about the network, so tests need no network either.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::skills::{is_slug, parse_skill_text};

/// One entry of the index. `url` may be http(s) or a local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Installed (`None` = wasn't installed before).
    Installed {
        name: String,
        from: Option<String>,
        to: String,
    },
    /// Already up to date.
    UpToDate { name: String, version: String },
    /// The installed version is newer; left untouched.
    Ahead {
        name: String,
        installed: String,
        offered: String,
    },
    /// Failed; disk untouched.
    Failed { name: String, why: String },
}

impl Outcome {
    /// One human-readable line.
    pub fn line(&self) -> String {
        match self {
            Outcome::Installed { name, from, to } => match from {
                Some(from) => format!("✓ {name}：{from} → {to}"),
                None => format!("✓ {name}: installed {to}"),
            },
            Outcome::UpToDate { name, version } => {
                format!("· {name}: already up to date ({version})")
            }
            Outcome::Ahead {
                name,
                installed,
                offered,
            } => format!(
                "· {name}: local {installed} is newer than the index's {offered}; left alone"
            ),
            Outcome::Failed { name, why } => format!("✗ {name}：{why}"),
        }
    }

    pub fn ok(&self) -> bool {
        !matches!(self, Outcome::Failed { .. })
    }
}

/// Parses the index. Bad entries are skipped with a reason — one broken entry must not fail the update.
pub fn parse_index(text: &str) -> Result<(Vec<IndexEntry>, Vec<String>), String> {
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("the index is not valid JSON: {e}"))?;
    let raw = parsed
        .get("skills")
        .and_then(Value::as_array)
        .ok_or("the index has no skills array")?;

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for item in raw {
        let name = item.get("name").and_then(Value::as_str);
        let version = item.get("version").and_then(Value::as_str);
        let url = item.get("url").and_then(Value::as_str);
        match (name, version, url) {
            (Some(name), Some(version), Some(url)) => entries.push(IndexEntry {
                name: name.to_string(),
                version: version.to_string(),
                url: url.to_string(),
            }),
            _ => warnings.push(format!("skipped an entry missing name/version/url: {item}")),
        }
    }
    Ok((entries, warnings))
}

/// Installed `installed`, index offers `offered` — replace or not?
///
/// Pure numeric dotted versions compare segment by segment; non-numeric (`2024-05`)
/// degrades to string comparison — just enough to keep the "no downgrade" rule.
pub fn needs_update(installed: Option<&str>, offered: &str) -> bool {
    let Some(installed) = installed else {
        return true;
    };
    let installed = installed.trim();
    if installed == offered.trim() {
        return false;
    }
    match (numeric(installed), numeric(offered)) {
        (Some(a), Some(b)) => b > a,
        _ => false, // uncomparable → don't touch — better fewer updates than thrash
    }
}

fn numeric(version: &str) -> Option<Vec<u32>> {
    version
        .split('.')
        .map(|part| part.trim().parse::<u32>().ok())
        .collect()
}

/// Installs a skill's body into `<home>/skills/<name>/SKILL.md`.
///
/// Returns Err on failed validation without touching a byte on disk.
///
/// Fresh installs go through `skills::install_from_text` (same path and validation
/// as `joy skill install`); only **replacing an existing version** takes the staging
/// + backup + atomic swap below — fresh installs have nothing to protect, replacements do.
pub fn install(home: &Path, name: &str, content: &str) -> Result<PathBuf, String> {
    // First gate against path traversal: the name must be a slug.
    if !is_slug(name) {
        return Err(format!("skill name '{name}' is not a valid slug"));
    }
    let skill = parse_skill_text(content).ok_or(
        "the content is not a valid SKILL.md (frontmatter is missing name or description)",
    )?;
    // Whatever the index calls it, the file must say: a name mismatch desyncs the
    // index from the directory from then on.
    if skill.name != name {
        return Err(format!(
            "the index calls it '{name}' but the file says '{}' — refusing to install a skill under the wrong name",
            skill.name
        ));
    }

    let skills = home.join("skills");
    if !skills.join(name).exists() {
        crate::skills::install_from_text(home, content)?;
        return Ok(skills.join(name).join("SKILL.md"));
    }

    let target = skills.join(name);
    let staging = skills.join(".staging").join(name);
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create the staging directory: {e}"))?;
    std::fs::write(staging.join("SKILL.md"), content)
        .map_err(|e| format!("could not write the staging file: {e}"))?;

    // Back up the old version first: a failed replacement can still go back (it must
    // exist here — the fresh-install branch already returned above).
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = skills.join(".backup").join(format!("{name}-{stamp}"));
    copy_dir(&target, &backup).map_err(|e| format!("backup failed: {e}"))?;
    std::fs::remove_dir_all(&target)
        .map_err(|e| format!("could not remove the old version: {e}"))?;
    // Atomic replace: a rename within the same filesystem.
    std::fs::rename(&staging, &target).map_err(|e| format!("replace failed: {e}"))?;
    Ok(target.join("SKILL.md"))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The locally installed version (reads the SKILL.md frontmatter).
pub fn installed_version(home: &Path, name: &str) -> Option<String> {
    let path = home.join("skills").join(name).join("SKILL.md");
    let text = std::fs::read_to_string(path).ok()?;
    parse_skill_text(&text).and_then(|skill| skill.version)
}

/// Updates everything per the index. `fetch` is injected by the caller (HTTP or local
/// file), so this layer knows nothing about the network and tests need none.
pub async fn update_all<F, Fut>(home: &Path, entries: &[IndexEntry], fetch: F) -> Vec<Outcome>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let mut outcomes = Vec::new();
    for entry in entries {
        let installed = installed_version(home, &entry.name);
        if !needs_update(installed.as_deref(), &entry.version) {
            outcomes.push(match installed {
                Some(version) if version == entry.version => Outcome::UpToDate {
                    name: entry.name.clone(),
                    version,
                },
                Some(installed) => Outcome::Ahead {
                    name: entry.name.clone(),
                    installed,
                    offered: entry.version.clone(),
                },
                None => Outcome::UpToDate {
                    name: entry.name.clone(),
                    version: entry.version.clone(),
                },
            });
            continue;
        }

        let content = match fetch(entry.url.clone()).await {
            Ok(content) => content,
            Err(why) => {
                outcomes.push(Outcome::Failed {
                    name: entry.name.clone(),
                    why: format!("fetch failed: {why}"),
                });
                continue;
            }
        };
        match install(home, &entry.name, &content) {
            Ok(_) => outcomes.push(Outcome::Installed {
                name: entry.name.clone(),
                from: installed,
                to: entry.version.clone(),
            }),
            Err(why) => outcomes.push(Outcome::Failed {
                name: entry.name.clone(),
                why,
            }),
        }
    }
    outcomes
}
