use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::io::IntoRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    pub pending: AtomicUsize,
    fail_streak: AtomicUsize,
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
                    let _ = nix::unistd::close(entry.fd);
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
                    let _ = nix::unistd::close(entry.fd);
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
                    let remote_addr_clone = remote_addr.clone();
                    let cfg2 = Arc::clone(&cfg);
                    thread::spawn(move || {
                        refill_one(&cfg2, &remote_addr_clone, &pool2);
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
                    let timeout = PollTimeout::from((cfg.connect_timeout * 1000) as u16);
                    match poll(&mut pfd, timeout) {
                        Ok(n) if n > 0 => {
                            match getsockopt(&bfd(fd), SocketError) {
                                Ok(0) => {
                                    connect_success(fd, pool, cfg);
                                    return;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
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
