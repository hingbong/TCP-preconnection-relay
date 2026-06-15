use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::io::IntoRawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::socket::{
    connect, getsockopt, sockopt::SocketError,
    SockaddrLike, SockaddrStorage,
};

use crate::config::Config;
use crate::log;
use crate::sock;

fn bfd(fd: RawFd) -> BorrowedFd<'static> {
    unsafe { BorrowedFd::borrow_raw(fd) }
}

struct PoolEntry {
    fd: RawFd,
    birth: Instant,
}

pub struct Pool {
    entries: Vec<PoolEntry>,
    pub max_size: usize,
    // These fields are always accessed under the outer Mutex<Pool> lock;
    // plain integers are simpler and faster than nested atomics/mutexes.
    pub pending: usize,
    fail_streak: usize,
    pub pause_until: Option<Instant>,
}

impl Pool {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_size),
            max_size,
            pending: 0,
            fail_streak: 0,
            pause_until: None,
        }
    }

    pub fn put(&mut self, fd: RawFd, now: Instant) {
        if self.entries.len() < self.max_size {
            self.entries.push(PoolEntry { fd, birth: now });
        } else {
            let _ = nix::unistd::close(fd);
        }
    }

    pub fn take_live(&mut self) -> Option<RawFd> {
        loop {
            let entry = self.entries.pop()?;
            if sock::socket_dead_fast(entry.fd) {
                let _ = nix::unistd::close(entry.fd);
                continue;
            }
            return Some(entry.fd);
        }
    }
}

/// Spawn a fixed pool of worker threads plus a maintenance thread.
/// Workers are long-lived and receive tasks via a shared channel, avoiding
/// the per-connection thread-spawn overhead of the original design.
pub fn spawn_maintain_thread(
    cfg: Arc<Config>,
    pool: Arc<Mutex<Pool>>,
    remote_addr: socket2::SockAddr,
) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = Arc::new(Mutex::new(rx));

    // Spawn `refill_batch` persistent workers that compete for tasks.
    // The Mutex<Receiver> pattern ensures each task is delivered to exactly
    // one worker while allowing all workers to run in parallel.
    let num_workers = cfg.refill_batch;
    for _ in 0..num_workers {
        let rx = Arc::clone(&rx);
        let pool2 = Arc::clone(&pool);
        let cfg2 = Arc::clone(&cfg);
        let addr2 = remote_addr.clone();
        thread::spawn(move || loop {
            // Only hold the mutex for the duration of recv() so that
            // workers are never blocked by each other while doing work.
            let msg = rx.lock().unwrap().recv();
            match msg {
                Ok(()) => refill_one(&cfg2, &addr2, &pool2),
                Err(_) => break, // sender dropped — shut down
            }
        });
    }

    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(50));
        let now = Instant::now();
        let mut p = pool.lock().unwrap();

        // Sweep dead sockets
        p.entries.retain(|entry| {
            if sock::socket_dead_fast(entry.fd) {
                let _ = nix::unistd::close(entry.fd);
                false
            } else {
                true
            }
        });

        // Rotate expired preconnects
        let ttl = Duration::from_millis(cfg.preconnect_ttl_ms);
        p.entries.retain(|entry| {
            if now - entry.birth > ttl {
                let _ = nix::unistd::close(entry.fd);
                false
            } else {
                true
            }
        });

        // Refill up to deficit, bounded by refill_batch
        let deficit = cfg.pool_size.saturating_sub(p.entries.len() + p.pending);
        let paused = p.pause_until.map_or(false, |pu| now < pu);
        if deficit > 0 && !paused {
            let want = deficit.min(cfg.refill_batch);
            for _ in 0..want {
                p.pending += 1;
                if tx.send(()).is_err() {
                    // All workers have exited — stop trying to refill
                    p.pending -= 1;
                    break;
                }
            }
        }
        drop(p);
    });
}

fn refill_one(cfg: &Config, remote_addr: &socket2::SockAddr, pool: &Mutex<Pool>) {
    match sock::create_tcp_socket(remote_addr.domain(), cfg, None) {
        Ok(sock) => {
            let fd = sock.into_raw_fd();

            // Resolve socket2::SockAddr to nix SockaddrStorage for connect()
            let remote_ptr = remote_addr.as_ptr() as *const libc::sockaddr;
            let remote_len = remote_addr.len();
            let nix_addr = unsafe {
                SockaddrStorage::from_raw(remote_ptr, Some(remote_len))
            };
            let nix_addr = match nix_addr {
                Some(a) => a,
                None => {
                    let _ = nix::unistd::close(fd);
                    connect_fail(pool, cfg);
                    return;
                }
            };

            match connect(fd, &nix_addr) {
                Ok(()) => {
                    connect_success(fd, pool, cfg);
                    return;
                }
                Err(nix::Error::EINPROGRESS) => {
                    // Wait for completion
                    let b = bfd(fd);
                    let mut pfd = [PollFd::new(b, PollFlags::POLLOUT)];
                    // Clamp to u16::MAX (65535 ms ≈ 65 s) — nix only supports
                    // From<u16> for PollTimeout on this platform.
                    let timeout_ms = cfg.connect_timeout
                        .saturating_mul(1000)
                        .min(u16::MAX as u64) as u16;
                    let timeout = PollTimeout::from(timeout_ms);
                    if let Ok(n) = poll(&mut pfd, timeout) {
                        if n > 0 {
                            if let Ok(0) = getsockopt(&bfd(fd), SocketError) {
                                connect_success(fd, pool, cfg);
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }

            let _ = nix::unistd::close(fd);
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
    p.pending = p.pending.saturating_sub(1);
    p.fail_streak = 0;
    p.pause_until = None;
    p.put(fd, now);
    let pool_count = p.entries.len();
    let pending = p.pending;
    log::push(format!(
        "Preconnect +1, Current: {pool_count}/{} (Pending: {pending})",
        cfg.pool_size,
    ));
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
