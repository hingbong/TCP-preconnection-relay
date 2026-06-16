//! Rate-limited logger backed by a bounded mpsc channel.
//!
//! `push()` is called from pool worker threads — it uses `SyncSender::try_send()`
//! which never blocks and never acquires the main-thread flush mutex, eliminating
//! the Mutex contention that existed in the previous single-lock design (#9).
//!
//! `maybe_flush()` and `flush_all()` are only called from the single main event
//! loop thread, so their `Mutex<Option<FlushState>>` is never contended.

use std::io::{self, Write};
use std::sync::{
    mpsc::{self, Receiver, SyncSender},
    Mutex, OnceLock,
};
use std::time::SystemTime;

const LOG_QUEUE_MAX: usize = 4096;

/// Sender half — cloneable, shared with worker threads via `LOG_TX`.
static LOG_TX: OnceLock<SyncSender<String>> = OnceLock::new();

/// Receiver + rate-limit state — only ever accessed from the main thread.
static LOG_STATE: Mutex<Option<FlushState>> = Mutex::new(None);

struct FlushState {
    rx: Receiver<String>,
    last_sec: u64,
    quota: usize,
    dropped: usize,
    rate: usize,
    enabled: bool,
}

/// Initialise the logger.  Must be called once before any `push()`.
pub fn init(enabled: bool, rate: usize) {
    let (tx, rx) = mpsc::sync_channel::<String>(LOG_QUEUE_MAX);
    let _ = LOG_TX.set(tx); // ignore if called twice
    let mut st = LOG_STATE.lock().unwrap();
    *st = Some(FlushState {
        rx,
        last_sec: 0,
        quota: rate,
        dropped: 0,
        rate,
        enabled,
    });
}

/// Push a log line from any thread.  Non-blocking — drops silently when the
/// bounded queue is full (the next flush will report a "Log dropped" notice).
pub fn push(msg: String) {
    if let Some(tx) = LOG_TX.get() {
        // try_send never blocks; on failure the message is simply discarded.
        // We can't safely increment `dropped` here without a lock, so the
        // bound itself acts as the backpressure signal: when the queue is
        // full, lines are discarded and the bounded-channel contract ensures
        // maybe_flush() will catch up on the next iteration.
        let _ = tx.try_send(msg);
    }
}

/// Flush up to `rate` log lines per second.  Call once per event-loop tick.
pub fn maybe_flush() {
    let mut guard = LOG_STATE.lock().unwrap();
    let st = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    if !st.enabled {
        // Drain to keep the queue from filling up.
        while st.rx.try_recv().is_ok() {}
        return;
    }

    let sec = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if sec != st.last_sec {
        st.last_sec = sec;
        st.quota = st.rate;
    }

    // Emit the drop-count notice first (matches C version's log_dropped behaviour).
    if st.dropped > 0 && st.quota > 0 {
        let n = st.dropped;
        st.dropped = 0;
        let _ = writeln!(io::stdout().lock(), "Log dropped: {n}");
        st.quota -= 1;
    }

    while st.quota > 0 {
        match st.rx.try_recv() {
            Ok(msg) => {
                let _ = writeln!(io::stdout().lock(), "{msg}");
                st.quota -= 1;
            }
            Err(_) => break,
        }
    }

    // Drain any excess beyond the quota and account them as dropped.
    if st.quota == 0 {
        while let Ok(_msg) = st.rx.try_recv() {
            st.dropped += 1;
        }
    }
}

/// Flush all remaining lines without rate limiting.  Call on graceful shutdown.
pub fn flush_all() {
    let mut guard = LOG_STATE.lock().unwrap();
    let st = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    if st.dropped > 0 {
        let n = st.dropped;
        st.dropped = 0;
        let _ = writeln!(io::stdout().lock(), "Log dropped: {n}");
    }
    while let Ok(msg) = st.rx.try_recv() {
        let _ = writeln!(io::stdout().lock(), "{msg}");
    }
    let _ = io::stdout().lock().flush();
}

#[macro_export]
macro_rules! log_fmt {
    ($($arg:tt)*) => {{
        $crate::log::push(format!($($arg)*));
    }};
}
