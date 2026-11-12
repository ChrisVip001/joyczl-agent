//! Bounded live output and persisted process logs.
use std::collections::VecDeque;

/// Byte cap of the output log file. Wider than the memory ring (16x) — post-hoc review needs more
/// context than live watching; but a screen-flooding command still must not write through the disk; when exceeded, say so.
const LOG_CAP: u64 = 1024 * 1024;

/// The spilled output log: written as output arrives; once capped, further bytes are dropped with a marker note.
pub(super) struct Log {
    file: Option<std::fs::File>,
    written: u64,
    truncated: bool,
}

impl Log {
    pub(super) fn open(path: &std::path::Path) -> Self {
        let file = std::fs::File::create(path)
            .map_err(|e| {
                eprintln!(
                    "(joy) the job log could not be created ({}): {e}",
                    path.display()
                )
            })
            .ok();
        Self {
            file,
            written: 0,
            truncated: false,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.truncated {
            return;
        }
        let room = LOG_CAP.saturating_sub(self.written) as usize;
        let take = bytes.len().min(room);
        if take > 0 && file.write_all(&bytes[..take]).is_ok() {
            self.written += take as u64;
        }
        if take < bytes.len() {
            self.truncated = true;
            let note = format!(
                "\n[output truncated at the {LOG_CAP}-byte log cap; the command kept running]\n"
            );
            let _ = file.write_all(note.as_bytes());
        }
    }
}

pub(crate) struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// Bytes already dropped (on read, the model must be told "you can no longer read these early ones").
    pub(super) dropped: u64,
}

impl Ring {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            // Exceeds the whole ring in one shot: keep only the last cap bytes; everything earlier counts as dropped.
            let keep = bytes.len() - self.cap;
            self.dropped += self.buf.len() as u64 + keep as u64;
            self.buf.clear();
            self.buf.extend(bytes[keep..].iter().copied());
            return;
        }
        self.buf.extend(bytes.iter().copied());
        if self.buf.len() > self.cap {
            let overflow = self.buf.len() - self.cap;
            self.buf.drain(..overflow);
            self.dropped += overflow as u64;
        }
    }

    /// Read from absolute position `cursor` to now. Returns (text, new cursor, whether anything was dropped).
    pub(crate) fn read_from(&self, cursor: u64) -> (String, u64, bool) {
        let start = cursor.max(self.dropped);
        let lossy = cursor < self.dropped;
        let offset = (start - self.dropped) as usize;
        let bytes: Vec<u8> = self.buf.iter().skip(offset).copied().collect();
        (
            String::from_utf8_lossy(&bytes).to_string(),
            self.dropped + self.buf.len() as u64,
            lossy,
        )
    }

    pub(super) fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub(crate) fn total(&self) -> u64 {
        self.dropped + self.buf.len() as u64
    }
}
