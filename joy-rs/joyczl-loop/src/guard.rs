//! 循环护栏：模型卡在重复或交替的工具调用里时，**早点**告诉它。
//!
//! 没有这一层，卡住的代价是整个迭代预算：模型把同一个调用重复到
//! `max_iterations` 用完，每一轮都是真金白银。`fold_tool_activity` 防的是
//! **跨轮**忘记自己做过（历史里的证据），这一层防的是**同一轮内**的死循环。
//!
//! 两条判据（照 hermes 的 `tool_guardrails.py`，那是唯一把这件事做全的实现）：
//!
//! 1. **连续相同**：同一个 `(工具, 参数, 结果)` 连续出现到第 3 次；
//! 2. **交替循环**：`A,B,A,B…` 这种按周期重复的批次。
//!
//! 第 2 条必须单独判，因为每一次交替都会把第 1 条的连续计数**重置** ——
//! 只做连续检测的话，模型每轮重放同一组 2–4 个调用，可以一路跑到预算用完
//! 而完全不触发告警。
//!
//! 命中之后做两件事：给模型一段话（它还来得及改道），以及把**字节级重复的大
//! 结果换成引用桩**（重复的长输出只是白烧上下文）。两条都只作用于喂回模型的
//! 文本 —— `LoopResult.tool_calls` 里留的永远是真结果，trace 与通知看到的该是真的。

use std::collections::VecDeque;

use serde_json::Value;

/// 连续相同多少次算「卡住」。3 而不是 2：一次重复可能是无害的复核。
const IDENTICAL_THRESHOLD: u32 = 3;

/// 交替循环的最大周期（`A,B` 的周期是 2，`A,B,C` 是 3）。
const MAX_CYCLE_PERIOD: usize = 4;

/// 结果短于这个长度就不值得换桩 —— 桩本身也有几十字符，省不下来还看不全。
const STUB_MIN_CHARS: usize = 512;

/// 桩里保留多少字符的参数预览：压缩把原文挤掉之后，至少还知道当时调的是什么。
const STUB_ARGS_PREVIEW_CHARS: usize = 120;

/// 一次调用的指纹。只存哈希不存原文：这条路每轮都在跑，不必留大字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    name: String,
    args: u64,
    result: u64,
}

/// 护栏的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Ok,
    /// 同一调用（同工具、同参数、同结果）连续第 `times` 次。
    Repeated {
        times: u32,
    },
    /// 同一组调用在按 `period` 步的周期重复。
    Cyclic {
        period: usize,
    },
}

impl GuardVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, GuardVerdict::Ok)
    }
}

/// 一轮之内的循环检测器。**每个 turn 新建一个** —— 跨轮的重复由
/// `fold_tool_activity` 那套（把工具活动折进历史）负责，不是这里的事。
#[derive(Default)]
pub struct StallGuard {
    recent: VecDeque<Call>,
    hits: i32,
    last_note: Option<String>,
}

impl StallGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 命中过几次（给 `TurnMeta.guard_hits`）。
    pub fn hits(&self) -> i32 {
        self.hits
    }

    /// 最近一次命中说的话（给 `TurnMeta.guard_note`）。
    pub fn last_note(&self) -> Option<String> {
        self.last_note.clone()
    }

    /// 观察一次调用，返回判定。调用方据此决定喂回模型的文本。
    pub fn observe(&mut self, name: &str, args: &Value, output: &str) -> GuardVerdict {
        let call = Call {
            name: name.to_string(),
            args: hash(&args.to_string()),
            result: hash(output),
        };
        self.recent.push_back(call.clone());
        // 只需要留够「两个周期」，多了没用。
        while self.recent.len() > MAX_CYCLE_PERIOD * 2 {
            self.recent.pop_front();
        }

        let verdict = self.classify(&call);
        if !verdict.is_ok() {
            self.hits += 1;
            self.last_note = reminder(&verdict);
        }
        verdict
    }

    fn classify(&self, call: &Call) -> GuardVerdict {
        let identical = self
            .recent
            .iter()
            .rev()
            .take_while(|recent| *recent == call)
            .count() as u32;
        if identical >= IDENTICAL_THRESHOLD {
            return GuardVerdict::Repeated { times: identical };
        }

        // 交替循环：最近 2p 个是不是两两相同。`p` 从 2 起 —— 周期 1 就是
        // 「一直同一个调用」，那由上面那条判据管。
        for period in 2..=MAX_CYCLE_PERIOD {
            let len = self.recent.len();
            if len < period * 2 {
                continue;
            }
            let window: Vec<&Call> = self.recent.iter().skip(len - period).collect();
            // 周期内全是同一个调用的话，是「重复」不是「交替」—— 跳过，
            // 免得把连续重复误报成周期循环。
            if window.iter().all(|item| *item == window[0]) {
                continue;
            }
            let repeats = (0..period)
                .all(|i| self.recent[len - 2 * period + i] == self.recent[len - period + i]);
            if repeats {
                return GuardVerdict::Cyclic { period };
            }
        }
        GuardVerdict::Ok
    }
}

/// 命中时追加给模型的一段话。没有命中返回 `None`。
pub fn reminder(verdict: &GuardVerdict) -> Option<String> {
    match verdict {
        GuardVerdict::Ok => None,
        GuardVerdict::Repeated { times } => Some(format!(
            "\n[guard] 同一个调用（同样的参数、同样的结果）已经出现 {times} 次。\
             别再重复它了 —— 换一条路，或者把卡住的地方直接告诉用户。"
        )),
        GuardVerdict::Cyclic { period } => Some(format!(
            "\n[guard] 你正在按 {period} 步的周期重复同一组调用。\
             别再重复了 —— 换一条路，或者把卡住的地方直接告诉用户。"
        )),
    }
}

/// 结果够长且是第 2 次以上重复 → 值得换桩。
fn should_stub(output: &str, times: u32) -> bool {
    times >= 2 && output.chars().count() >= STUB_MIN_CHARS
}

/// 引用桩：说清「这是第几次相同调用」，并留下参数预览。
fn stub(name: &str, args: &Value, times: u32) -> String {
    let preview: String = args
        .to_string()
        .chars()
        .take(STUB_ARGS_PREVIEW_CHARS)
        .collect();
    format!("[{name} 第 {times} 次相同调用：结果与上一次完全相同，此处省略（参数：{preview}）]")
}

/// 决定「喂回模型的文本」：命中时该换桩就换桩，并追加提醒；
/// 没命中就原样返回。
pub fn for_model(output: &str, name: &str, args: &Value, verdict: &GuardVerdict) -> String {
    let Some(note) = reminder(verdict) else {
        return output.to_string();
    };
    let body = match verdict {
        GuardVerdict::Repeated { times } if should_stub(output, *times) => stub(name, args, *times),
        _ => output.to_string(),
    };
    format!("{body}{note}")
}

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// 一轮里护栏的账：命中几次 + 最后一次说的话。
/// 进 `LoopResult`，最终进 `TurnMeta`（于是 trace 与界面都能看到「这一轮卡过」）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardReport {
    pub hits: i32,
    pub note: Option<String>,
}

impl GuardReport {
    /// 从检测器结账。
    pub fn close(guard: &StallGuard) -> Self {
        Self {
            hits: guard.hits(),
            note: guard.last_note(),
        }
    }
}
