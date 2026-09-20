//! 过程记忆 —— SKILL.md：怎么做事，只在相关时载入。
//!
//! 官方 Anthropic Agent Skills 格式：YAML frontmatter 带 `name` 和
//! `description`（description 兼任触发器 —— 不设自定义 `triggers` 字段）。
//!
//! 渐进披露，真正要紧的部分：
//!   1. 每个技能的 frontmatter 永远被扫到（便宜）；
//!   2. 技能的**正文**只在它跟消息匹配时才进 prompt；
//!   3. 技能引用的文件只在模型开口要时才读。
//!
//! 触发是**透明**的 —— 消息与 name+description 的关键词重合度，没有
//! embedding，没有魔法，得分心算得出来。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 解析出来的一个技能。`body` 只有匹配上才会被读进 prompt。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}

/// 解析 SKILL.md 的文本（loader 与 create_skill 工具共用同一套校验）。
///
/// 形状必须是：
/// ```text
/// ---
/// name: weekly-review
/// description: ...
/// ---
/// 正文……
/// ```
/// frontmatter 里没有 `name` 或 `description` 就不算技能。
pub fn parse_skill_text(text: &str) -> Option<Skill> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']);
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            _ => {}
        }
    }
    Some(Skill {
        name: name?,
        description: description?,
        body: body.trim().to_string(),
        path: PathBuf::new(),
    })
}

fn parse_file(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut skill = parse_skill_text(&text)?;
    skill.path = path.to_path_buf();
    Some(skill)
}

/// 消息与技能描述共用的分词：小写、连续的 ASCII 字母数字、至少 3 个字符。
/// 中文不参与 —— 触发词要写进 description，这也是 Skills 格式的要求。
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

/// 扫描技能目录。目录变更（新增/编辑 SKILL.md）时自动重扫，
/// 所以会话中途 create_skill 写下的技能下一轮就生效。
pub struct SkillLoader {
    dirs: Vec<PathBuf>,
    skills: Vec<Skill>,
    sig: Vec<(PathBuf, SystemTime)>,
}

impl SkillLoader {
    /// 目录列表：`home/skills`（装出来的 + agent 自己写的），加上
    /// `JOY_SKILL_DIRS` 里冒号分隔的额外目录（自带 / 社区技能包）。
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

    /// 目录指纹：每个 SKILL.md 的路径 + 修改时间。
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

    /// 当前已装载的技能（name + description 那一层，不含正文）。
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// 透明触发：消息与 name+description 的关键词重合数 ≥ 2 才算命中，
    /// 取重合最多的前 `max_skills` 个。目录变了就先重扫。
    pub fn match_message(&mut self, message: &str, max_skills: usize) -> Vec<&Skill> {
        if self.scan_sig() != self.sig {
            self.refresh();
        }
        let msg_words: std::collections::HashSet<String> = tokens(message).into_iter().collect();
        let mut scored: Vec<(usize, &Skill)> = self
            .skills
            .iter()
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
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored
            .into_iter()
            .take(max_skills)
            .map(|(_, s)| s)
            .collect()
    }

    /// 拼进 system prompt 的技能段落。空串 = 没匹配上，调用方就不加标题。
    pub fn matching_skills(&mut self, message: &str) -> String {
        let matched = self.match_message(message, 2);
        matched
            .iter()
            .map(|s| format!("### {}\n{}", s.name, s.body))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// `joy skill list` 的一行。
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub folder: PathBuf,
}

/// 全部已装载技能，按名字去重，`home/skills` 的同名技能赢过其它目录
/// —— 跟 loader 的优先级是同一条。SKILL.md 直接躺在技能根目录的、
/// `_incoming`（install 的暂存区）里的，都不算可导出的技能。
pub fn loaded_skills(home: &Path) -> Vec<LoadedSkill> {
    let dirs = SkillLoader::dirs_for(home);
    let mut by_name: std::collections::BTreeMap<String, LoadedSkill> =
        std::collections::BTreeMap::new();
    // home 最后插入 → 同名覆盖（赢过 JOY_SKILL_DIRS 里的）。
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
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// 递归收集一个技能文件夹的全部文件（相对路径 → 字节），跳过垃圾。
/// export 用它做「有没有被对方改过」的对比，也用它复制。
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

/// `joy skill export` 的执行体：把每个技能文件夹复制到
/// `<base>/<target>/skills/<名>/`。对方手里那份**被改过的副本默认保留**
/// （`force` 才覆盖）—— 那可能是人家在另一个 agent 里做的修改。
/// 返回逐行的人读输出。
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

/// `joy skill install` 的执行体：收到 SKILL.md 的**文本**（网络抓取由
/// CLI 做），校验 frontmatter（与 create_skill 同一套），落位
/// `<home>/skills/<名>/SKILL.md`。重名一律拒绝 —— 从不覆盖已有技能。
pub fn install_from_text(home: &Path, text: &str) -> Result<String, String> {
    let skill = parse_skill_text(text).ok_or_else(|| {
        "不是合法的技能：SKILL.md 需要 YAML frontmatter，含 name 和 description。".to_string()
    })?;
    if !skill
        .name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || skill.name.is_empty()
    {
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
