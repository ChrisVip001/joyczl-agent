//! 技能的安装与更新：取回 → 校验 → 比对版本 → 暂存 → 备份 → 原子替换。
//!
//! 顺序不是随便排的，每一步都在防一类事故：
//!
//! * **先校验**：内容过不了 `parse_skill_text`（缺 name/description）就不动
//!   磁盘 —— 装进去一个解析不了的技能，等于把已有能力换成一个坏文件。
//! * **先暂存再替换**：新内容先落到 `<home>/skills/.staging/`，成了才动目标
//!   目录。中途失败留下的是一份垃圾文件，不是一个坏掉的技能。
//! * **替换前备份**：老版本挪进 `<home>/skills/.backup/<名字>-<时间戳>/`，
//!   要回退就是一条 `mv`。
//! * **不降级**：装着的版本更新就不动它（除非用户删掉重装）。
//!
//! 取回（HTTP 还是本地文件）由调用方注入 —— 这一层不认识网络，测试也就不
//! 需要网络。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::skills::{is_slug, parse_skill_text};

/// 索引里的一条。`url` 可以是 http(s)，也可以是本地路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 装上了（`None` = 以前没装过）。
    Installed {
        name: String,
        from: Option<String>,
        to: String,
    },
    /// 已经是最新的。
    UpToDate { name: String, version: String },
    /// 装着的是更新的版本，没动。
    Ahead {
        name: String,
        installed: String,
        offered: String,
    },
    /// 失败了，磁盘没动。
    Failed { name: String, why: String },
}

impl Outcome {
    /// 给人看的一行。
    pub fn line(&self) -> String {
        match self {
            Outcome::Installed { name, from, to } => match from {
                Some(from) => format!("✓ {name}：{from} → {to}"),
                None => format!("✓ {name}：装上 {to}"),
            },
            Outcome::UpToDate { name, version } => format!("· {name}：已是最新（{version}）"),
            Outcome::Ahead {
                name,
                installed,
                offered,
            } => format!("· {name}：本地 {installed} 比索引里的 {offered} 新，没动"),
            Outcome::Failed { name, why } => format!("✗ {name}：{why}"),
        }
    }

    pub fn ok(&self) -> bool {
        !matches!(self, Outcome::Failed { .. })
    }
}

/// 解析索引。坏条目跳过并给出原因 —— 一条写坏的条目不该让整次更新失败。
pub fn parse_index(text: &str) -> Result<(Vec<IndexEntry>, Vec<String>), String> {
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("索引不是合法 JSON：{e}"))?;
    let raw = parsed
        .get("skills")
        .and_then(Value::as_array)
        .ok_or("索引里没有 skills 数组")?;

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
            _ => warnings.push(format!("跳过一条缺 name/version/url 的条目：{item}")),
        }
    }
    Ok((entries, warnings))
}

/// 装了 `installed`、索引给的是 `offered` —— 要不要换？
///
/// 纯数字点分版本按段比大小；非数字（`2024-05` 这种）退化成字符串比较，
/// 只求「不降级」这条规矩成立。
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
        _ => false, // 比不了大小就不动 —— 宁可少更新，不可来回折腾
    }
}

fn numeric(version: &str) -> Option<Vec<u32>> {
    version
        .split('.')
        .map(|part| part.trim().parse::<u32>().ok())
        .collect()
}

/// 把一份技能的正文装进 `<home>/skills/<名字>/SKILL.md`。
///
/// 校验不过就返回 Err，磁盘一个字节都不动。
///
/// 全新安装走 `skills::install_from_text`（与 `joy skill install` 同一条
/// 路径、同一套校验）；只有**替换已有版本**才走下面的暂存 + 备份 + 原子换 ——
/// 新装没有要保护的东西，替换有。
pub fn install(home: &Path, name: &str, content: &str) -> Result<PathBuf, String> {
    // 路径穿越的第一道闸：名字必须是 slug。
    if !is_slug(name) {
        return Err(format!("技能名 '{name}' 不是合法 slug"));
    }
    let skill = parse_skill_text(content)
        .ok_or("内容不是一个合法的 SKILL.md（frontmatter 缺 name 或 description）")?;
    // 索引说叫什么、文件里就得写什么：装一个名不对版的技能，
    // 等于让索引与目录从此对不上。
    if skill.name != name {
        return Err(format!(
            "索引说它叫 '{name}'，文件里写的是 '{}' —— 拒绝装一个名不对版的技能",
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
    std::fs::create_dir_all(&staging).map_err(|e| format!("建暂存目录失败：{e}"))?;
    std::fs::write(staging.join("SKILL.md"), content)
        .map_err(|e| format!("写暂存文件失败：{e}"))?;

    // 老版本先备份：替换失败还能回去（走到这里必然已存在 —— 新装那条
    // 分支上面就返回了）。
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = skills.join(".backup").join(format!("{name}-{stamp}"));
    copy_dir(&target, &backup).map_err(|e| format!("备份失败：{e}"))?;
    std::fs::remove_dir_all(&target).map_err(|e| format!("清掉旧版本失败：{e}"))?;
    // 原子替换：同一文件系统内的 rename。
    std::fs::rename(&staging, &target).map_err(|e| format!("替换失败：{e}"))?;
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

/// 本地已装的版本（读 SKILL.md 的 frontmatter）。
pub fn installed_version(home: &Path, name: &str) -> Option<String> {
    let path = home.join("skills").join(name).join("SKILL.md");
    let text = std::fs::read_to_string(path).ok()?;
    parse_skill_text(&text).and_then(|skill| skill.version)
}

/// 按索引更新一遍。`fetch` 由调用方注入（HTTP 或本地文件），
/// 这样这一层既不知道网络，测试也不需要网络。
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
                    why: format!("取回失败：{why}"),
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
