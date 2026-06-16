use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::IntoRawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::socket::{connect, getsockopt, sockopt::SocketError, SockaddrLike, SockaddrStorage};

use crate::config::Config;
use crate::log;
use crate::sock;

struct PoolEntry {
    fd: RawFd,
    birth: Instant,
    ttl_ms: u64,
}

pub struct Pool {
    entries: Vec<PoolEntry>,
    pub max_size: usize,
    pub pending: usize,
    fail_streak: usize,
    pub pause_until: Option<Instant>,
    /// Xorshift64 state for cheap ±25 % TTL jitter.
    rng: u64,
}

impl Pool {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_size),
            max_size,
            pending: 0,
            fail_streak: 0,
            pause_until: None,
            rng: 0x517cc1b727220a95,
        }
    }

    fn next_rng(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn jittered_ttl(&mut self, base_ms: u64) -> u64 {
        let quarter = base_ms / 4;
        let span = quarter * 2 + 1;
        base_ms - quarter + (self.next_rng() % span)
    }

    pub fn put(&mut self, fd: RawFd, now: Instant, base_ttl_ms: u64) -> bool {
        if self.entries.len() < self.max_size {
            let ttl_ms = self.jittered_ttl(base_ttl_ms);
            self.entries.push(PoolEntry { fd, birth: now, ttl_ms });
            true
        } else {
            let _ = nix::unistd::close(fd);
            false
        }
    }

    /// Pop up to `n` raw entries (no liveness checks) without any I/O.
    /// O(n), sub-microsecond lock hold time.
    fn pop_batch(&mut self, n: usize) -> Vec<PoolEntry> {
        let len = self.entries.len();
        let take = len.min(n);
        if take == 0 {
            return Vec::new();
        }
        self.entries.drain(len - take..).collect()
    }
}

/// Acquire one live pre-connected fd from the pool.
///
/// Addresses issue #1: the previous `take_live()` held the `Pool` mutex
/// during `socket_dead_fast()` — three syscalls per dead entry.  This
/// function pops a small batch *under the lock* (no I/O), checks liveness
/// *outside the lock*, then returns unused live entries *under the lock*.
/// The mutex is held for two O(1) array operations, never for syscalls.
pub fn take_live_unlocked(pool_mutex: &Mutex<Pool>) -> Option<RawFd> {
    const BATCH: usize = 4;

    // Keep popping batches until we find a live fd or the pool is empty.
    // If the first batch's entries are all dead, there may still be live
    // entries deeper in the pool.  Each iteration does lock-pop (O(1)),
    // lock-free liveness check, then lock-reinsert of surplus live entries.
    loop {
        // Step 1: pop candidates under lock — no I/O.
        let candidates = pool_mutex.lock().unwrap().pop_batch(BATCH);
        if candidates.is_empty() {
            return None;
        }

        // Step 2: check liveness WITHOUT holding the lock.
        let mut live_unused: Vec<PoolEntry> = Vec::new();
        let mut result: Option<RawFd> = None;

        for entry in candidates {
            if result.is_some() {
                live_unused.push(entry);
            } else if !sock::socket_dead_fast(entry.fd) {
                result = Some(entry.fd);
            } else {
                let _ = nix::unistd::close(entry.fd);
            }
        }

        // Step 3: return unused live entries under lock.
        if !live_unused.is_empty() {
            pool_mutex.lock().unwrap().entries.extend(live_unused);
        }

        if result.is_some() {
            return result;
        }
        // All candidates were dead → loop and try the next batch.
    }
}

pub fn spawn_maintain_thread(
    cfg: Arc<Config>,
    pool: Arc<Mutex<Pool>>,
    remote_addr: socket2::SockAddr,
) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = Arc::new(Mutex::new(rx));

    let num_workers = cfg.refill_batch;
    for _ in 0..num_workers {
        let rx = Arc::clone(&rx);
        let pool2 = Arc::clone(&pool);
        let cfg2 = Arc::clone(&cfg);
        let addr2 = remote_addr.clone();
        thread::spawn(move || loop {
            let msg = rx.lock().unwrap().recv();
            match msg {
                Ok(()) => refill_one(&cfg2, &addr2, &pool2),
                Err(_) => break,
            }
        });
    }

    thread::spawn(move || {
        let mut last_sweep = Instant::now();
        let mut last_rotate = Instant::now();

        loop {
            thread::sleep(Duration::from_millis(100));
            let now = Instant::now();

            // ── Sweep dead connections (batch-pop + lock-free) ─────────────────
            let need_sweep = now.duration_since(last_sweep) >= Duration::from_secs(1);
            let mut alive: Vec<PoolEntry> = Vec::new();
            if need_sweep {
                last_sweep = now;
                // Pop everything, check liveness outside the lock to avoid
                // holding the mutex during syscalls in socket_dead_fast().
                let batch = pool.lock().unwrap().pop_batch(usize::MAX);
                for entry in batch {
                    if sock::socket_dead_fast(entry.fd) {
                        let _ = nix::unistd::close(entry.fd);
                        log::push("Checking: Clear Zombies".into());
                    } else {
                        alive.push(entry);
                    }
                }
            }

            // ── Rotate + refill (under one lock) ──────────────────────────────
            let mut p = pool.lock().unwrap();

            // Re-insert survivors from the lock-free sweep.
            if need_sweep {
                p.entries.append(&mut alive);
            }

            let need_rotate = now.duration_since(last_rotate) >= Duration::from_secs(1);
            if need_rotate {
                last_rotate = now;
                p.entries.retain(|entry| {
                    if now.duration_since(entry.birth).as_millis() as u64 > entry.ttl_ms {
                        let _ = nix::unistd::close(entry.fd);
                        log::push("Checking: preconnect rotating".into());
                        false
                    } else {
                        true
                    }
                });
            }

            let deficit = cfg.pool_size.saturating_sub(p.entries.len() + p.pending);
            let paused = p.pause_until.map_or(false, |pu| now < pu);
            if deficit > 0 && !paused {
                // Adaptive cap (#6): recover at full refill_batch speed when the
                // pool is more than half empty; throttle to 2 for steady-state
                // top-ups to avoid connect bursts.
                let cap = if deficit > cfg.pool_size / 2 { cfg.refill_batch } else { 2 };
                let want = deficit.min(cfg.refill_batch).min(cap);
                for _ in 0..want {
                    p.pending += 1;
                    if tx.send(()).is_err() {
                        p.pending -= 1;
                        break;
                    }
                }
            }
        }
    });
}

fn refill_one(cfg: &Config, remote_addr: &socket2::SockAddr, pool: &Mutex<Pool>) {
    // Wrap in OwnedFd immediately so the fd is closed automatically on any
    // early-return error path — no explicit nix::unistd::close() needed (#5).
    let owned = match sock::create_tcp_socket(remote_addr.domain(), cfg, None) {
        Ok(s) => unsafe { OwnedFd::from_raw_fd(s.into_raw_fd()) },
        Err(_) => {
            connect_fail(pool, cfg);
            return;
        }
    };

    let remote_ptr = remote_addr.as_ptr() as *const libc::sockaddr;
    let remote_len = remote_addr.len();
    let nix_addr = match unsafe { SockaddrStorage::from_raw(remote_ptr, Some(remote_len)) } {
        Some(a) => a,
        None => {
            connect_fail(pool, cfg);
            return; // owned drops → fd closed
        }
    };

    match connect(owned.as_raw_fd(), &nix_addr) {
        Ok(()) => {
            connect_success(owned.into_raw_fd(), pool, cfg);
            return;
        }
        Err(nix::Error::EINPROGRESS) => {
            let timeout_ms = cfg
                .connect_timeout
                .saturating_mul(1000)
                .min(u16::MAX as u64) as u16;
            let mut pfd = [PollFd::new(owned.as_fd(), PollFlags::POLLOUT)];
            if let Ok(n) = poll(&mut pfd, PollTimeout::from(timeout_ms)) {
                if n > 0 {
                    if let Ok(0) = getsockopt(&owned, SocketError) {
                        connect_success(owned.into_raw_fd(), pool, cfg);
                        return;
                    }
                }
            }
        }
        _ => {}
    }

    // owned drops here → fd closed automatically.
    connect_fail(pool, cfg);
}

fn connect_success(fd: RawFd, pool: &Mutex<Pool>, cfg: &Config) {
    let now = Instant::now();
    let mut p = pool.lock().unwrap();
    p.pending = p.pending.saturating_sub(1);
    p.fail_streak = 0;
    p.pause_until = None;
    let ok = p.put(fd, now, cfg.preconnect_ttl_ms);
    if ok {
        let pool_count = p.entries.len();
        let pending = p.pending;
        log::push(format!(
            "Preconnect +1, Current: {pool_count}/{} (Pending: {pending})",
            cfg.pool_size,
        ));
    } else {
        log::push("Preconnected Too Much, Clearing ...".into());
    }
}

fn connect_fail(pool: &Mutex<Pool>, cfg: &Config) {
    let mut p = pool.lock().unwrap();
    p.pending = p.pending.saturating_sub(1);
    p.fail_streak += 1;
    let streak = p.fail_streak;
    if streak >= cfg.refill_batch {
        let backoff = Duration::from_millis((200 * streak as u64).min(5000));
        p.pause_until = Some(Instant::now() + backoff);
    }
}

