//! 超长文本的**落盘**与**能放得下的预览**。
//!
//! 两处调用方，同一套规矩：`exec.rs`（命令输出超 8000 字符）与
//! `joyczl-loop` 的轮内工具结果预算（一批结果总量超阈值）。
//!
//! 三条不变式，抄的是 deepseek-harness 的 `spill-policy`：
//!
//! 1. **预览必须真的放得下**：先按最坏情况把省略说明的字数预留进去，再算
//!    head/tail 的预算 —— 替换之后仍然超限，这个桩就没有意义。
//! 2. **只切完整行**：半个行（尤其半句 JSON、半条日志）比少一行更难读，模型
//!    还会以为那就是全部。首行本身就超预算时如实说明，不硬塞。
//! 3. **落盘失败不影响结果**：桩照样成立（只是没有路径可回查），绝不因为写不进
//!    磁盘就把一次成功的调用变成错误。

use std::path::Path;

/// 落盘后的桩：替换文本 + 完整原文的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    /// 替换后的文本，**字符数不超过调用方给的预算**。
    pub text: String,
    /// 完整原文相对 home 的路径（落盘失败或没给目录时为 `None`）。
    pub path: Option<String>,
    /// 省掉了多少字符、多少行（给测试与日志看）。
    pub omitted_chars: usize,
    pub omitted_lines: usize,
}

/// 把完整原文写到 `<dir>/<日期>/<tag>-<时刻>-<pid>.txt`，返回**相对 home 的路径**。
///
/// 相对路径好读，也不把用户的绝对路径泄漏进上下文。任何一步失败都返回 `None`
/// —— 调用方按「没落盘」继续。
pub fn spill_text(dir: &Path, text: &str, tag: &str) -> Option<String> {
    let now = chrono::Local::now();
    let day = now.format("%Y%m%d").to_string();
    let stamp = now.format("%H%M%S%.3f").to_string();
    let folder = dir.join(&day);
    std::fs::create_dir_all(&folder).ok()?;
    let file = folder.join(format!("{tag}-{stamp}-{}.txt", std::process::id()));
    std::fs::write(&file, text).ok()?;
    // 相对 home（spill 的父目录）：读起来就是「去 spill/… 看」。
    //
    // 分隔符统一成 `/`：这个字符串是**写给模型看**的，而 Windows 上 `display()`
    // 给的是反斜杠 —— 同一句话在两种平台长得不一样，测试也就跟着分成两套。
    // 统一的 `/` 在 Windows 上照样能 join 回来（系统 API 两种都认）。
    let relative = dir
        .parent()
        .and_then(|home| file.strip_prefix(home).ok())
        .unwrap_or(&file);
    Some(relative.display().to_string().replace('\\', "/"))
}

/// 给超长文本做一个放得下的桩。文本本来就不超预算时返回 `None`。
///
/// `dir` 为 `None` 时不落盘（桩里也就不会出现路径）。
pub fn stub(dir: Option<&Path>, text: &str, budget_chars: usize, tag: &str) -> Option<Stub> {
    let total = text.chars().count();
    if total <= budget_chars || budget_chars == 0 {
        return None;
    }

    // 先落盘**完整原文**：这一步必须在切之前（顺序反了存下去的会是切过的那份）。
    let path = dir.and_then(|dir| spill_text(dir, text, tag));

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // 省略说明按最坏情况定长预留：先算一个上界（行数、字符数都按全量估计），
    // 再据此算 head/tail 预算。说明短一点没关系，**总长绝不超预算**。
    let notice = notice_for(total_lines, total, path.as_deref());
    let notice_len = notice.chars().count();
    if notice_len + 2 >= budget_chars {
        // 预算小到连说明都放不下：只留说明，并如实说这是全部。
        let mut text = notice;
        text.truncate_chars(budget_chars);
        return Some(Stub {
            text,
            path,
            omitted_chars: total,
            omitted_lines: total_lines,
        });
    }

    let remaining = budget_chars - notice_len - 2; // 两个换行
    let head_budget = remaining.div_ceil(2);
    let tail_budget = remaining / 2;

    let (head, head_lines, head_chars) = take_head_lines(&lines, head_budget);
    let (tail, tail_lines, tail_chars) = take_tail_lines(&lines, tail_budget);

    let omitted_lines = total_lines.saturating_sub(head_lines + tail_lines);
    let omitted_chars = total.saturating_sub(head_chars + tail_chars);
    let notice = notice_for(omitted_lines, total, path.as_deref());

    let mut out = String::new();
    if !head.is_empty() {
        out.push_str(&head);
        out.push('\n');
    }
    out.push_str(&notice);
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail);
    }
    debug_assert!(
        out.chars().count() <= budget_chars.max(notice_len + 2),
        "桩必须放得下：{} > {budget_chars}",
        out.chars().count()
    );

    Some(Stub {
        text: out,
        path,
        omitted_chars,
        omitted_lines,
    })
}

/// 省略说明。带路径与不带路径两种措辞 —— 后者绝不能出现「完整输出在」这种
/// 指向不存在文件的说法。
fn notice_for(omitted_lines: usize, total_chars: usize, path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "…（结果共 {total_chars} 字符，已截断，省略了 {omitted_lines} 行；完整输出在 {path}）"
        ),
        // 没落盘就**不提路径** —— 「完整输出在 …」而文件不存在，比不写更坏。
        None => format!("…（结果共 {total_chars} 字符，已截断，省略了 {omitted_lines} 行）"),
    }
}

/// 从开头取完整行，直到快要超预算。
fn take_head_lines(lines: &[&str], budget: usize) -> (String, usize, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    let mut taken = 0usize;
    for line in lines {
        let cost = line.chars().count() + 1; // 换行也算
        if used + cost > budget {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        used += cost;
        taken += 1;
    }
    // 一行都没放下：如实交白卷，让调用方只留省略说明。
    (out, taken, used)
}

/// 从末尾取完整行，直到快要超预算。
fn take_tail_lines(lines: &[&str], budget: usize) -> (String, usize, usize) {
    let mut picked: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            break;
        }
        picked.push(line);
        used += cost;
    }
    picked.reverse();
    let taken = picked.len();
    (picked.join("\n"), taken, used)
}

/// 按字符数截断（只在预算小到连说明都放不下时用）。
trait TruncateChars {
    fn truncate_chars(&mut self, max: usize);
}

impl TruncateChars for String {
    fn truncate_chars(&mut self, max: usize) {
        if self.chars().count() <= max {
            return;
        }
        *self = self.chars().take(max).collect();
    }
}
