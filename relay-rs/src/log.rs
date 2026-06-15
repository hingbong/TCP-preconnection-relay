use std::io::{self, Write};
use std::sync::Mutex;
use std::time::SystemTime;

/// Rate-limited logger. Pushes lines onto a queue; `maybe_flush` drains
/// them at up to `rate` lines per second. Same model as the C version.
pub struct Logger {
    buf: String,
    last_sec: u64,
    quota: usize,
    pub enabled: bool,
    pub rate: usize,
    dropped: usize,
}

impl Logger {
    pub const fn new() -> Self {
        Self {
            buf: String::new(),
            last_sec: 0,
            quota: 0,
            enabled: true,
            rate: 24,
            dropped: 0,
        }
    }

    pub fn enqueue(&mut self, msg: String) {
        if !self.enabled {
            return;
        }
        self.buf.push_str(&msg);
        if !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
    }

    /// Call this from the event loop. Flushes up to `self.rate` lines per second.
    /// When lines are discarded due to rate-limiting, a "Log dropped: N" notice is
    /// emitted on the next flush — matching the C version's log_dropped behaviour.
    pub fn maybe_flush(&mut self) {
        if !self.enabled || self.rate == 0 {
            if !self.buf.is_empty() {
                self.dropped += self.buf.lines().count();
                self.buf.clear();
            }
            return;
        }

        let sec = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if sec != self.last_sec {
            self.last_sec = sec;
            self.quota = self.rate;
        }

        // Emit the dropped-count notice first (one quota slot), as the C version does.
        if self.dropped > 0 && self.quota > 0 {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "Log dropped: {}", self.dropped);
            let _ = out.flush();
            self.dropped = 0;
            self.quota -= 1;
        }

        if self.quota == 0 {
            self.dropped += self.buf.lines().count();
            self.buf.clear();
            return;
        }

        let mut to_flush = String::new();
        std::mem::swap(&mut to_flush, &mut self.buf);
        if to_flush.is_empty() {
            return;
        }

        let mut out = io::stdout().lock();
        let mut lines_iter = to_flush.lines().peekable();
        while self.quota > 0 {
            match lines_iter.next() {
                Some(line) => {
                    let _ = writeln!(out, "{line}");
                    self.quota -= 1;
                }
                None => break,
            }
        }
        // Any lines that didn't fit within the quota are counted as dropped.
        self.dropped += lines_iter.count();
        let _ = out.flush();
    }

    pub fn flush_all(&mut self) {
        let mut out = io::stdout().lock();
        let mut to_flush = String::new();
        std::mem::swap(&mut to_flush, &mut self.buf);
        for line in to_flush.lines() {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }
}

/// Thread-safe wrapper — both the main event loop and pool thread can log.
pub static LOG: Mutex<Logger> = Mutex::new(Logger::new());

/// Push a log line (pre-formatted string).
pub fn push(msg: String) {
    if let Ok(mut logger) = LOG.lock() {
        logger.enqueue(msg);
    }
}

/// Flush rate-limited log lines. Call from event loop each iteration.
pub fn maybe_flush() {
    if let Ok(mut logger) = LOG.lock() {
        logger.maybe_flush();
    }
}

#[macro_export]
macro_rules! log_fmt {
    ($($arg:tt)*) => {{
        $crate::log::push(format!($($arg)*));
    }};
}
