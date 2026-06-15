use std::os::fd::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::log;
use crate::sock;

/// A preconnected socket in the pool, with its birth time.
struct PoolEntry {
    fd: RawFd,
    birth: Instant,
}

/// The shared connection pool, protected by a Mutex (same as C's pool_mtx).
pub struct Pool {
    entries: Vec<PoolEntry>,
    pub max_size: usize,
    /// Number of in-flight connect attempts.
    pub pending: AtomicUsize,
    /// Consecutive failed connect attempts; triggers backoff.
    fail_streak: AtomicUsize,
    /// When to resume refills (monotonic Instant).
    pause_until: Mutex<Option<Instant>>,
}

impl Pool {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_size),
            max_size,
            pending: AtomicUsize::new(0),
            fail_streak: AtomicUsize::new(0),
            pause_until: Mutex::new(None),
        }
    }

    /// Put a successfully connected fd into the pool.
    pub fn put(&mut self, fd: RawFd, now: Instant) {
        if self.entries.len() < self.max_size {
            self.entries.push(PoolEntry { fd, birth: now });
        } else {
            unsafe { libc::close(fd) };
        }
    }

    /// Take a preconnected fd from the pool.
    /// **FIX for the C bug:** loops until it finds a live fd or the pool is empty.
    pub fn take_live(&mut self) -> Option<RawFd> {
        loop {
            let entry = self.entries.pop()?;
            if sock::socket_dead_fast(entry.fd) {
                unsafe { libc::close(entry.fd) };
                continue;
            }
            return Some(entry.fd);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Spawn the maintenance thread: sweeps zombies, rotates expired preconnects, refills.
pub fn spawn_maintain_thread(
    cfg: Arc<Config>,
    pool: Arc<Mutex<Pool>>,
    remote_addr: socket2::SockAddr,
) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(50));
            let now = Instant::now();
            let mut pool_guard = pool.lock().unwrap();

            // Sweep zombies
            pool_guard.entries.retain(|entry| {
                if sock::socket_dead_fast(entry.fd) {
                    unsafe { libc::close(entry.fd) };
                    log::push("Checking: Clear Zombies".into());
                    false
                } else {
                    true
                }
            });

            // Rotate expired preconnects
            let ttl = Duration::from_millis(cfg.preconnect_ttl_ms);
            pool_guard.entries.retain(|entry| {
                if now - entry.birth > ttl {
                    unsafe { libc::close(entry.fd) };
                    log::push("Checking: preconnect rotating".into());
                    false
                } else {
                    true
                }
            });

            // Refill
            let deficit = cfg.pool_size.saturating_sub(
                pool_guard.entries.len() + pool_guard.pending.load(Ordering::Acquire),
            );
            let paused = pool_guard.pause_until.lock().unwrap().map_or(false, |pu| now < pu);
            if deficit > 0 && !paused {
                let want = deficit.min(cfg.refill_batch);
                for _ in 0..want {
                    pool_guard.pending.fetch_add(1, Ordering::Release);
                    let pool2 = Arc::clone(&pool);
                    let remote_addr2 = remote_addr.clone();
                    let cfg2 = Arc::clone(&cfg);
                    thread::spawn(move || {
                        refill_one(&cfg2, &remote_addr2, &pool2);
                    });
                }
            }
            drop(pool_guard);
        }
    });
}

fn refill_one(cfg: &Config, remote_addr: &socket2::SockAddr, pool: &Mutex<Pool>) {
    match sock::create_tcp_socket(remote_addr.domain(), cfg, None) {
        Ok(sock) => {
            let fd = sock.into_raw_fd();
            let c_ret = unsafe { libc::connect(fd, remote_addr.as_ptr(), remote_addr.len()) };
            if c_ret == 0 {
                connect_success(fd, pool, cfg);
                return;
            }
            let err = unsafe { *libc::__errno_location() };
            if err != libc::EINPROGRESS {
                unsafe { libc::close(fd) };
                connect_fail(pool, cfg);
                return;
            }

            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let timeout_ms = (cfg.connect_timeout * 1000) as i32;
            let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if r > 0 {
                let mut err_val: i32 = 0;
                let mut len: u32 = std::mem::size_of::<i32>() as u32;
                let gs_ret = unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        &mut err_val as *mut _ as *mut libc::c_void,
                        &mut len,
                    )
                };
                if gs_ret == 0 && err_val == 0 {
                    connect_success(fd, pool, cfg);
                    return;
                }
            }
            unsafe { libc::close(fd) };
            connect_fail(pool, cfg);
        }
        Err(_) => {
            connect_fail(pool, cfg);
        }
    }
}

fn connect_success(fd: RawFd, pool: &Mutex<Pool>, cfg: &Config) {
    let now = Instant::now();
    let mut p = pool.lock().unwrap();
    p.pending.fetch_sub(1, Ordering::Release);
    p.fail_streak.store(0, Ordering::Release);
    *p.pause_until.lock().unwrap() = None;
    p.put(fd, now);
    let pool_count = p.entries.len();
    let pending = p.pending.load(Ordering::Acquire);
    log::push(format!(
        "Preconnect +1, Current: {pool_count}/{} (Pending: {pending})",
        cfg.pool_size,
    ));
}

fn connect_fail(pool: &Mutex<Pool>, cfg: &Config) {
    let p = pool.lock().unwrap();
    p.pending.fetch_sub(1, Ordering::Release);
    let streak = p.fail_streak.fetch_add(1, Ordering::Release) + 1;
    if streak >= cfg.refill_batch as usize {
        let backoff = Duration::from_millis((200 * streak as u64).min(5000));
        *p.pause_until.lock().unwrap() = Some(Instant::now() + backoff);
    }
}
