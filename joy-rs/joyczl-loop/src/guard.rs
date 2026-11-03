//! Loop guardrails: when the model is stuck in repeated or alternating tool calls, tell it **early**.
//!
//! Without this layer, getting stuck costs the whole iteration budget: the model repeats the
//! same call until `max_iterations` runs out, and every round is real money. `fold_tool_activity`
//! guards **cross-turn** forgetting (history); this layer guards **within-turn** dead loops.
//!
//! Two criteria:
//!
//! 1. **Consecutive identical**: the same `(tool, args, result)` appearing 3 times in a row;
//! 2. **Alternating cycles**: batches like `A,B,A,B…` repeating on a period.
//!
//! Criterion 2 must be judged separately, because every alternation **resets** criterion 1's
//! consecutive count — with consecutive detection alone, a model replaying the same 2–4 calls
//! each round could run the budget down without ever tripping the alarm.
//!
//! On a hit, two things happen: the model gets a message (still in time to change course), and
//! **byte-identical large results are replaced by a reference stub** (repeated long output only
//! burns context). Both act on the fed-back text only — `LoopResult.tool_calls` keeps the truth.

use std::collections::VecDeque;

use serde_json::Value;

/// Consecutive identical calls that count as "stuck". 3, not 2: one repeat may be a recheck.
const IDENTICAL_THRESHOLD: u32 = 3;

/// Max period of an alternating cycle (`A,B` has period 2, `A,B,C` has 3).
const MAX_CYCLE_PERIOD: usize = 4;

/// Results shorter than this are not worth stubbing — the stub itself costs tens of chars.
const STUB_MIN_CHARS: usize = 512;

/// Args preview kept in the stub: with the original compacted away, the call stays identifiable.
const STUB_ARGS_PREVIEW_CHARS: usize = 120;

/// Fingerprint of one call. Store hashes, not text: this path runs every turn, keep it light.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    name: String,
    args: u64,
    result: u64,
}

/// The guard's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Ok,
    /// The same call (same tool, args, result) for the `times`-th consecutive time.
    Repeated {
        times: u32,
    },
    /// The same group of calls repeating on a `period`-step cycle.
    Cyclic {
        period: usize,
    },
}

impl GuardVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, GuardVerdict::Ok)
    }
}

/// A within-turn loop detector. **Create a fresh one per turn** — cross-turn repetition is
/// handled by the `fold_tool_activity` machinery (folding tool activity into history), not here.
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

    /// How many times it has tripped (for `TurnMeta.guard_hits`).
    pub fn hits(&self) -> i32 {
        self.hits
    }

    /// What the last hit said (for `TurnMeta.guard_note`).
    pub fn last_note(&self) -> Option<String> {
        self.last_note.clone()
    }

    /// Observe one call, return the verdict. The caller decides what text to feed back.
    pub fn observe(&mut self, name: &str, args: &Value, output: &str) -> GuardVerdict {
        let call = Call {
            name: name.to_string(),
            args: hash(&args.to_string()),
            result: hash(output),
        };
        self.recent.push_back(call.clone());
        // Only keep enough for "two periods"; more is useless.
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

        // Alternating cycle: are the last 2p entries pairwise identical. `p` starts at 2 —
        // period 1 is "always the same call", already covered by the criterion above.
        for period in 2..=MAX_CYCLE_PERIOD {
            let len = self.recent.len();
            if len < period * 2 {
                continue;
            }
            let window: Vec<&Call> = self.recent.iter().skip(len - period).collect();
            // If the whole window is the same call, it is "repeated", not "alternating" — skip,
            // so consecutive repeats are not misreported as a cycle.
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

/// A message appended for the model on a hit. Returns `None` when there is no hit.
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

/// Long enough result repeated for the 2nd time or more → worth stubbing.
fn should_stub(output: &str, times: u32) -> bool {
    times >= 2 && output.chars().count() >= STUB_MIN_CHARS
}

/// Reference stub: states which identical call this is, and keeps an args preview.
fn stub(name: &str, args: &Value, times: u32) -> String {
    let preview: String = args
        .to_string()
        .chars()
        .take(STUB_ARGS_PREVIEW_CHARS)
        .collect();
    format!("[{name} 第 {times} 次相同调用：结果与上一次完全相同，此处省略（参数：{preview}）]")
}

/// Decides the "text fed back to the model": stub on a hit when warranted, and append the
/// reminder; return as-is when there is no hit.
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

/// The within-turn guard tally: hit count + the last note.
/// Goes into `LoopResult`, then `TurnMeta` (so trace and UI can see "this turn stalled").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardReport {
    pub hits: i32,
    pub note: Option<String>,
}

impl GuardReport {
    /// Settle up with the detector.
    pub fn close(guard: &StallGuard) -> Self {
        Self {
            hits: guard.hits(),
            note: guard.last_note(),
        }
    }
}
