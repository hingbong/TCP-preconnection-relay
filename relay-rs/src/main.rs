//! Main event loop, zero-copy splice pump, TCP/UDP forwarding, connection lifecycle.
//!
//! Key improvements over the original Rust rewrite:
//! * #2+#3: Raw nix epoll with EPOLLET + EPOLLRDHUP instead of mio LT mode.
//! * #4:    Safe nix wrappers for the entire UDP path (no more libc unsafe blocks).
//! * #5:    OwnedFd in Conn — lifetimes are correct and fds close automatically.
//! * #7:    SIGTERM/SIGINT handler with AtomicBool for graceful shutdown.

mod config;
mod log;
mod pool;
mod sock;

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use nix::errno::Errno;
use nix::fcntl::{splice, FcntlArg, SpliceFFlags};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::socket::{
    accept4, connect, MsgFlags, SockFlag, SockaddrLike, SockaddrStorage,
};
use nix::unistd::pipe2;
use socket2::{SockAddr, Socket, Type};

use config::Config;
use pool::Pool;
use sock as s;

// ── Signal handling (#7) ─────────────────────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

// ── Connection statistics ────────────────────────────────────────────────────

static STAT_TOTAL_ACCEPTS: AtomicU64 = AtomicU64::new(0);
static STAT_POOL_HITS: AtomicU64 = AtomicU64::new(0);

extern "C" fn handle_shutdown(_sig: libc::c_int) {
    // AtomicBool store is async-signal-safe.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "relay", about = "TCP/UDP preconnection relay (Rust rewrite)")]
struct Cli {
    #[arg(short = 'c', long, env = "RELAY_CONFIG")]
    config: Option<String>,
    #[arg(short, long, env = "LOCAL_IP")]
    local_ip: Option<String>,
    #[arg(short = 'P', long, env = "LOCAL_PORT")]
    local_port: Option<u16>,
    #[arg(short = 'r', long, env = "REMOTE_IP")]
    remote_ip: Option<String>,
    #[arg(short = 't', long, env = "REMOTE_TCP_PORT")]
    remote_tcp_port: Option<u16>,
    #[arg(short = 'u', long, env = "REMOTE_UDP_PORT")]
    remote_udp_port: Option<u16>,
}

// ── Epoll token constants ────────────────────────────────────────────────────

const TOKEN_ACCEPT: u64 = u64::MAX;
const TOKEN_UDP: u64 = u64::MAX - 1;
// UDP upstream sockets: tokens [UDP_BASE, UDP_BASE + slot_idx)
const UDP_BASE: u64 = 1 << 26;

// ── Epoll token encoding (ABA-safe) ────────────────────────────────────
// Layout: [gen:16][slab_idx:47][side:1]
// gen wraps at 65536. Stale events from a freed+reused slot carry
// the old gen → mismatch → silently skipped.
fn encode_token(gen: u16, idx: usize, side: u8) -> u64 {
    ((gen as u64) << 48) | ((idx as u64) << 1) | (side as u64)
}
fn decode_token(token: u64) -> (u16, usize, bool) {
    let gen = (token >> 48) as u16;
    let idx = ((token & 0x0000_FFFF_FFFF_FFFE) >> 1) as usize;
    let is_remote = (token & 1) != 0;
    (gen, idx, is_remote)
}

// ── Zero-copy pump ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum PumpStatus {
    Ok,
    Eof,
    Err,
}

/// Splice data from `src` → pipe → `dst`.  Drains until EAGAIN on both ends.
/// Borrows `BorrowedFd<'_>` derived from `OwnedFd` fields — no lifetime erasure.
fn pump(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    pipe_w: BorrowedFd<'_>,
    pipe_r: BorrowedFd<'_>,
    pipe_len: &mut usize,
    splice_chunk: usize,
    now: Instant,
    last_ts: &mut Instant,
) -> PumpStatus {
    let mut got_eof = false;
    let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;

    // ET mode: loop until src hits EAGAIN so no buffered data is left unread.
    // A single splice_chunk may be smaller than what is already in the socket
    // buffer; without this loop the connection would stall until new data
    // arrives (which would never trigger another edge in ET).
    loop {
        let mut src_eagain = false;

        // Fill pipe from src until full or EAGAIN/EOF.
        while *pipe_len < splice_chunk {
            let remain = splice_chunk - *pipe_len;
            match splice(src, None, pipe_w, None, remain, flags) {
                Ok(n) if n > 0 => {
                    *pipe_len += n;
                    *last_ts = now;
                }
                Ok(_) => {
                    got_eof = true;
                    break;
                }
                Err(Errno::EAGAIN) => {
                    src_eagain = true;
                    break;
                }
                Err(Errno::EINTR) => continue,
                Err(Errno::ENOMEM) => break,
                Err(_) => return PumpStatus::Err,
            }
        }

        // Drain pipe to dst.
        while *pipe_len > 0 {
            match splice(pipe_r, None, dst, None, *pipe_len, flags) {
                Ok(n) if n > 0 => {
                    *pipe_len -= n;
                    *last_ts = now;
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(_) => return PumpStatus::Err,
                Ok(_) => break, // splice returned 0 yet pipe still has data — shouldn't happen, but break to avoid infinite loop
            }
        }

        // Stop when: EOF received, dst is congested (pipe still has data),
        // or src returned EAGAIN (socket buffer fully drained).
        if got_eof || *pipe_len > 0 || src_eagain {
            break;
        }
        // pipe is empty and src did not hit EAGAIN: a full splice_chunk was
        // consumed and dst accepted it all — src likely has more data, loop.
    }

    if got_eof {
        PumpStatus::Eof
    } else {
        PumpStatus::Ok
    }
}

// ── TCP connection state ─────────────────────────────────────────────────────

/// All file descriptors are OwnedFd so:
///  * Correct lifetimes — no 'static lifetime erasure (#5).
///  * Automatic close via Drop when the Conn is freed from the slab.
struct Conn {
    gen: u16, // generation counter for ABA-safe tokens
    fd_l: OwnedFd,
    fd_r: OwnedFd,
    pipe_l2r_w: OwnedFd, // write end of local→remote splice pipe
    pipe_l2r_r: OwnedFd, // read  end
    pipe_r2l_w: OwnedFd, // write end of remote→local splice pipe
    pipe_r2l_r: OwnedFd, // read  end
    len_l2r: usize,
    len_r2l: usize,
    last_l2r: Instant,
    last_r2l: Instant,
    eof_l2r: bool,
    eof_r2l: bool,
    shut_wr_r: bool,
    shut_wr_l: bool,
    half_close_since: Option<Instant>,
    connecting: bool,
    connect_start: Instant,
}

/// Re-arm both fds with the correct EPOLLET + EPOLLRDHUP flags (#2 + #3).
///
/// ET mode requires unconditional epoll_ctl(MOD) on every event: the kernel
/// re-checks the fd state and will fire immediately if data is already
/// buffered. Skipping MOD when flags are unchanged is safe in LT but fatal
/// in ET — any data left in the socket buffer after a partial read would
/// never produce another edge, stalling the connection.
///
/// Flow control: EPOLLIN is suppressed when the pipe for that direction is
/// already full (len >= splice_chunk).  Without this guard, a congested dst
/// causes pump() to return immediately (pipe full, dst EAGAIN), conn_watch
/// unconditionally re-arms EPOLLIN, the kernel sees data in the src buffer
/// and fires instantly, and the cycle repeats — a busy-loop that burns 100 %
/// of one CPU core until dst becomes writable again.  Suppressing EPOLLIN
/// when we cannot consume more data breaks the cycle; EPOLLOUT on the dst fd
/// will re-arm EPOLLIN once the pipe has been drained.
fn conn_watch(conn: &mut Conn, epoll: &Epoll, slab_idx: usize, splice_chunk: usize) {
    let token_l = encode_token(conn.gen, slab_idx, 0);
    let token_r = encode_token(conn.gen, slab_idx, 1);

    // fd_l: read client data (l2r), write to client (r2l drain).
    let mut want_l = EpollFlags::EPOLLET;
    if !conn.eof_l2r {
        want_l |= EpollFlags::EPOLLRDHUP;
        // Suppress EPOLLIN when the l2r pipe is full — we can't splice more
        // until the remote (fd_r) side drains it via EPOLLOUT.
        if conn.len_l2r < splice_chunk {
            want_l |= EpollFlags::EPOLLIN;
        }
    }
    if conn.len_r2l > 0 {
        want_l |= EpollFlags::EPOLLOUT;
    }

    // fd_r: read server data (r2l), write to server (l2r drain).
    let mut want_r = EpollFlags::EPOLLET;
    if !conn.eof_r2l {
        want_r |= EpollFlags::EPOLLRDHUP;
        // Suppress EPOLLIN when the r2l pipe is full.
        if conn.len_r2l < splice_chunk {
            want_r |= EpollFlags::EPOLLIN;
        }
    }
    if conn.len_l2r > 0 {
        want_r |= EpollFlags::EPOLLOUT;
    }

    // Always MOD — this is the ET re-arm. Cost: ~2 syscalls (~400 ns) per
    // connection event, completely negligible compared to data throughput.
    let mut ev_l = EpollEvent::new(want_l, token_l);
    if let Err(e) = epoll.modify(&conn.fd_l, &mut ev_l) {
        log_fmt!("epoll.modify(fd_l) error: {e}");
    }
    let mut ev_r = EpollEvent::new(want_r, token_r);
    if let Err(e) = epoll.modify(&conn.fd_r, &mut ev_r) {
        log_fmt!("epoll.modify(fd_r) error: {e}");
    }
}

// ── UDP association ──────────────────────────────────────────────────────────

struct UdpAssoc {
    cli_addr: SockaddrStorage,                  // address to echo replies to
    cli_net_addr: Option<std::net::SocketAddr>, // hashmap key (None for exotic AF)
    up_fd: OwnedFd,                             // connected upstream UDP socket
    last_act: Instant,
}

// ── Slab helpers ─────────────────────────────────────────────────────────────

fn alloc_slot<T>(slab: &mut Vec<Option<T>>, free: &mut VecDeque<usize>, val: T) -> usize {
    if let Some(idx) = free.pop_front() {
        slab[idx] = Some(val);
        idx
    } else {
        let idx = slab.len();
        slab.push(Some(val));
        idx
    }
}

fn free_slot<T>(slab: &mut Vec<Option<T>>, free: &mut VecDeque<usize>, idx: usize) {
    slab[idx] = None;
    free.push_back(idx);
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let mut cfg = if let Some(ref path) = cli.config {
        Config::from_file(path).unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        })
    } else {
        Config::default()
    };
    cfg.apply_env_overrides();
    if let Some(ref ip) = cli.local_ip {
        cfg.local_ip = ip.clone();
    }
    if let Some(port) = cli.local_port {
        cfg.local_port = port;
    }
    if let Some(ref ip) = cli.remote_ip {
        cfg.remote_ip = ip.clone();
    }
    if let Some(port) = cli.remote_tcp_port {
        cfg.remote_tcp_port = port;
    }
    if let Some(port) = cli.remote_udp_port {
        cfg.remote_udp_port = port;
    }
    let cfg = cfg.validate().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    // Initialise the new mpsc-backed logger.
    log::init(cfg.log_enable, cfg.log_rate_per_sec);

    log::push(format!(
        "Using config: local={}:{} -> remote={} TCP:{}/UDP:{}",
        cfg.local_ip, cfg.local_port, cfg.remote_ip, cfg.remote_tcp_port, cfg.remote_udp_port
    ));
    log::push(format!(
        "Runtime: pool={} refill={} splice={} backlog={} udp_buf={} log={}",
        cfg.pool_size,
        cfg.refill_batch,
        cfg.splice_chunk,
        cfg.listen_backlog,
        cfg.udp_socket_buffer,
        if cfg.log_enable { "on" } else { "off" }
    ));

    // Raise fd limit and set up signal handlers (#7).
    let _ =
        nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE, 65535, 65535);
    let sa_ign = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    let sa_shut = SigAction::new(
        SigHandler::Handler(handle_shutdown),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGPIPE, &sa_ign).unwrap();
        sigaction(Signal::SIGTERM, &sa_shut).unwrap();
        sigaction(Signal::SIGINT, &sa_shut).unwrap();
    }

    let local_addr = s::resolve(&cfg.local_ip, cfg.local_port, Type::STREAM)
        .expect("failed to resolve LOCAL_IP");
    let remote_tcp_addr = s::resolve(&cfg.remote_ip, cfg.remote_tcp_port, Type::STREAM)
        .expect("failed to resolve REMOTE_TCP");
    let remote_udp_addr = s::resolve(&cfg.remote_ip, cfg.remote_udp_port, Type::DGRAM)
        .expect("failed to resolve REMOTE_UDP");

    let domain = local_addr.domain();

    // TCP listen socket (OwnedFd owns the fd for the program lifetime).
    let tcp_listen_sock = Socket::new(domain, Type::STREAM, Some(socket2::Protocol::TCP)).unwrap();
    tcp_listen_sock.set_nonblocking(true).unwrap();
    tcp_listen_sock.set_reuse_address(true).unwrap();
    tcp_listen_sock.bind(&local_addr).unwrap();
    tcp_listen_sock.listen(cfg.listen_backlog).unwrap();
    let tcp_listen: OwnedFd = unsafe { OwnedFd::from_raw_fd(tcp_listen_sock.into_raw_fd()) };

    // UDP listen socket.
    let udp_listen_sock = Socket::new(domain, Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    udp_listen_sock.set_nonblocking(true).unwrap();
    udp_listen_sock.set_reuse_address(true).unwrap();
    let _ = udp_listen_sock.set_recv_buffer_size(cfg.udp_socket_buffer);
    let _ = udp_listen_sock.set_send_buffer_size(cfg.udp_socket_buffer);
    udp_listen_sock.bind(&local_addr).unwrap();
    let udp_listen: OwnedFd = unsafe { OwnedFd::from_raw_fd(udp_listen_sock.into_raw_fd()) };

    // Epoll instance (#2: raw epoll with ET).
    let epoll = Epoll::new(EpollCreateFlags::empty()).expect("epoll_create1 failed");
    epoll
        .add(
            tcp_listen.as_fd(),
            EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLET, TOKEN_ACCEPT),
        )
        .unwrap();
    epoll
        .add(
            udp_listen.as_fd(),
            EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLET, TOKEN_UDP),
        )
        .unwrap();

    // Preconnection pool.
    let pool = Arc::new(Mutex::new(Pool::new(cfg.pool_size, cfg.ttl_jitter_pct)));
    if cfg.pool_size > 0 {
        let cfg_arc = Arc::new(cfg.clone());
        pool::spawn_maintain_thread(
            cfg_arc,
            Arc::clone(&pool),
            remote_tcp_addr.clone(),
            &SHUTDOWN,
        );
    }

    let splice_chunk = cfg.splice_chunk;

    // TCP connection slab: token = slab_idx*2 (local fd) or slab_idx*2+1 (remote fd).
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut free_slots: VecDeque<usize> = VecDeque::new();
    let mut slab_gen: u16 = 0;

    // UDP association slab: token = UDP_BASE + slot_idx.
    let mut udp_map: HashMap<std::net::SocketAddr, usize> = HashMap::new();
    let mut udp_slots: Vec<Option<UdpAssoc>> = Vec::new();
    let mut udp_free: VecDeque<usize> = VecDeque::new();

    let mut events = vec![EpollEvent::new(EpollFlags::empty(), 0); 1024];
    let mut last_cleanup = Instant::now();

    // ── Event loop ───────────────────────────────────────────────────────────
    loop {
        let n = match epoll.wait(&mut events, EpollTimeout::from(100u16)) {
            Ok(n) => n,
            Err(Errno::EINTR) => 0, // interrupted by signal — check SHUTDOWN below
            Err(e) => {
                log::push(format!("epoll_wait error: {e}"));
                break;
            }
        };

        if SHUTDOWN.load(Ordering::SeqCst) {
            log::push("Shutdown signal received, exiting...");
            break;
        }

        let now = Instant::now();
        log::maybe_flush();

        for i in 0..n {
            let ev = events[i];
            let token = ev.data();
            let ev_flags = ev.events();

            // ── TCP accept ───────────────────────────────────────────────────
            if token == TOKEN_ACCEPT {
                loop {
                    match accept4(tcp_listen.as_raw_fd(), SockFlag::SOCK_NONBLOCK) {
                        Ok(cli_fd) => {
                            STAT_TOTAL_ACCEPTS.fetch_add(1, Ordering::Relaxed);
                            let cli_sock = unsafe { Socket::from_raw_fd(cli_fd) };
                            s::set_tcp_options(&cli_sock, &cfg);
                            let cli_owned: OwnedFd =
                                unsafe { OwnedFd::from_raw_fd(cli_sock.into_raw_fd()) };

                            // Try to get a pre-connected fd from the pool (#1: mutex-free liveness check).
                            let pool_fd = if cfg.pool_size > 0 {
                                pool::take_live_unlocked(&pool)
                            } else {
                                None
                            };

                            let (rem_owned, connecting) = if let Some(owned) = pool_fd {
                                STAT_POOL_HITS.fetch_add(1, Ordering::Relaxed);
                                (owned, false)
                            } else {
                                log::push("Exceeded Connections Pool, Direct Out...");
                                match direct_connect(&remote_tcp_addr, &cfg) {
                                    Ok(ConnectState::Connected(owned)) => (owned, false),
                                    Ok(ConnectState::Connecting(owned)) => (owned, true),
                                    Err(_) => continue,
                                }
                            };

                            let (pipe_l2r_r, pipe_l2r_w) = match make_pipe() {
                                Ok(p) => p,
                                Err(_) => continue,
                            };
                            let (pipe_r2l_r, pipe_r2l_w) = match make_pipe() {
                                Ok(p) => p,
                                Err(_) => continue,
                            };
                            tune_pipe(pipe_l2r_r.as_fd(), splice_chunk);
                            tune_pipe(pipe_l2r_w.as_fd(), splice_chunk);
                            tune_pipe(pipe_r2l_r.as_fd(), splice_chunk);
                            tune_pipe(pipe_r2l_w.as_fd(), splice_chunk);

                            // Initial epoll registration flags (#2+#3: EPOLLET + EPOLLRDHUP).
                            // During connect: fd_l gets no EPOLLIN (wait for remote to connect).
                            let init_flags_l = if connecting {
                                EpollFlags::EPOLLET | EpollFlags::EPOLLRDHUP
                            } else {
                                EpollFlags::EPOLLET | EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP
                            };
                            let init_flags_r = if connecting {
                                // LT mode for initial connect: EPOLLET is deliberately
                                // omitted.  The TCP handshake can complete between the
                                // non-blocking connect() and epoll_ctl(ADD); with ET the
                                // edge is missed and EPOLLOUT never fires.  LT delivers
                                // EPOLLOUT continuously until the handler sets
                                // connecting=false and conn_watch() switches back to ET.
                                EpollFlags::EPOLLOUT
                                    | EpollFlags::EPOLLRDHUP
                                    | EpollFlags::EPOLLERR
                                    | EpollFlags::EPOLLHUP
                            } else {
                                EpollFlags::EPOLLET | EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP
                            };

                            let gen = slab_gen;
                            slab_gen = slab_gen.wrapping_add(1);

                            let conn = Conn {
                                gen,
                                fd_l: cli_owned,
                                fd_r: rem_owned,
                                pipe_l2r_w,
                                pipe_l2r_r,
                                pipe_r2l_w,
                                pipe_r2l_r,
                                len_l2r: 0,
                                len_r2l: 0,
                                last_l2r: now,
                                last_r2l: now,
                                eof_l2r: false,
                                eof_r2l: false,
                                shut_wr_r: false,
                                shut_wr_l: false,
                                half_close_since: None,
                                connecting,
                                connect_start: now,
                            };
                            let slab_idx = alloc_slot(&mut conns, &mut free_slots, conn);
                            let c = conns[slab_idx].as_ref().unwrap();

                            let token_l = encode_token(c.gen, slab_idx, 0);
                            let token_r = encode_token(c.gen, slab_idx, 1);
                            // Short-circuit: if the first add fails there is no
                            // point attempting the second — the Conn will be
                            // freed below and close() auto-deregisters the fd
                            // from epoll on Linux (no dup anywhere).
                            if epoll
                                .add(c.fd_l.as_fd(), EpollEvent::new(init_flags_l, token_l))
                                .is_err()
                                || epoll
                                    .add(c.fd_r.as_fd(), EpollEvent::new(init_flags_r, token_r))
                                    .is_err()
                            {
                                free_slot(&mut conns, &mut free_slots, slab_idx);
                            } else if connecting {
                                // Catch fast connect completion that happens between
                                // connect() returning EINPROGRESS and epoll_ctl(ADD).
                                // On local connections the TCP handshake can complete
                                // before epoll registers the fd; EPOLLET only fires on
                                // state transitions, so the edge is missed and EPOLLOUT
                                // never fires — the connection would stall until the
                                // connect_timeout cleanup fires 5 seconds later.
                                use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
                                let conn = conns[slab_idx].as_mut().unwrap();
                                let mut pfd = [PollFd::new(
                                    conn.fd_r.as_fd(),
                                    PollFlags::POLLOUT | PollFlags::POLLERR | PollFlags::POLLHUP,
                                )];
                                if let Ok(1) = poll(&mut pfd, PollTimeout::ZERO) {
                                    let err = nix::sys::socket::getsockopt(
                                        &conn.fd_r,
                                        nix::sys::socket::sockopt::SocketError,
                                    );
                                    if err == Ok(0) {
                                        conn.connecting = false;
                                        conn_watch(conn, &epoll, slab_idx, splice_chunk);
                                    } else {
                                        log::push("Connect failed (post-ADD)");
                                        free_slot(&mut conns, &mut free_slots, slab_idx);
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                continue;
            }

            // ── UDP inbound (client → us → upstream) ──
            // Batched recv via recvmmsg(2); per-packet dispatch to upstream.
            if token == TOKEN_UDP {
                const BUF: usize = s::UDP_PKT_SIZE;
                let batch = cfg.udp_batch_size.min(s::UDP_BATCH_MAX);
                let mut bufs: [[u8; BUF]; s::UDP_BATCH_MAX] = [[0u8; BUF]; s::UDP_BATCH_MAX];
                let mut lens: [u32; s::UDP_BATCH_MAX] = [0; s::UDP_BATCH_MAX];
                let mut addrs: [libc::sockaddr_storage; s::UDP_BATCH_MAX] =
                    unsafe { std::mem::zeroed() };
                loop {
                    let n = s::udp_recvmmsg(
                        udp_listen.as_raw_fd(),
                        &mut bufs,
                        &mut lens,
                        &mut addrs,
                        batch,
                    );
                    if n == 0 {
                        break;
                    }
                    for i in 0..n {
                        let data = &bufs[i][..lens[i] as usize];
                        // Translate sockaddr_storage to nix SockaddrStorage for lookup.
                        let cli_addr: SockaddrStorage = unsafe {
                            SockaddrStorage::from_raw(
                                &addrs[i] as *const _ as *const libc::sockaddr,
                                Some(std::mem::size_of::<libc::sockaddr_storage>() as _),
                            )
                            .unwrap()
                        };
                        let cli_net = s::storage_to_net(&cli_addr);

                        let up_fd_raw = if let Some(net) = cli_net {
                            if let Some(&slot) = udp_map.get(&net) {
                                udp_slots.get_mut(slot).and_then(|s| s.as_mut()).map(|a| {
                                    a.last_act = now;
                                    a.up_fd.as_raw_fd()
                                })
                            } else {
                                None
                            }
                        } else {
                            udp_slots
                                .iter_mut()
                                .filter_map(|s| s.as_mut())
                                .find(|a| s::nix_storage_eq(&a.cli_addr, &cli_addr))
                                .map(|a| {
                                    a.last_act = now;
                                    a.up_fd.as_raw_fd()
                                })
                        };

                        if let Some(fd) = up_fd_raw {
                            let _ = nix::sys::socket::send(fd, data, MsgFlags::MSG_DONTWAIT);
                        } else {
                            match s::create_udp_socket(domain, &cfg) {
                                Ok(up_sock) => {
                                    let _ = up_sock.connect(&remote_udp_addr);
                                    let up_owned = unsafe {
                                        OwnedFd::from_raw_fd(up_sock.into_raw_fd())
                                    };
                                    let slot_idx = if let Some(idx) = udp_free.pop_front() {
                                        idx
                                    } else {
                                        let idx = udp_slots.len();
                                        udp_slots.push(None);
                                        idx
                                    };
                                    let t = UDP_BASE + slot_idx as u64;
                                    if epoll
                                        .add(
                                            up_owned.as_fd(),
                                            EpollEvent::new(
                                                EpollFlags::EPOLLIN | EpollFlags::EPOLLET,
                                                t,
                                            ),
                                        )
                                        .is_ok()
                                    {
                                        let _ = nix::sys::socket::send(
                                            up_owned.as_raw_fd(),
                                            data,
                                            MsgFlags::MSG_DONTWAIT,
                                        );
                                        if let Some(net) = cli_net {
                                            udp_map.insert(net, slot_idx);
                                        }
                                        udp_slots[slot_idx] = Some(UdpAssoc {
                                            cli_addr,
                                            cli_net_addr: cli_net,
                                            up_fd: up_owned,
                                            last_act: now,
                                        });
                                    } else {
                                        udp_free.push_back(slot_idx);
                                    }
                                }
                                Err(_) => {}
                            }
                        }
                    }
                }
                continue;
            }

            // ── UDP upstream response (upstream → us → client) ──
            // Batched recv + sendmmsg(2) back to same client.
            if token >= UDP_BASE {
                let slot_idx = (token - UDP_BASE) as usize;
                if slot_idx < udp_slots.len() {
                    if let Some(Some(ref mut assoc)) = udp_slots.get_mut(slot_idx) {
                        let up_raw = assoc.up_fd.as_raw_fd();
                        let cli_addr = &assoc.cli_addr;
                        let cli_addr_len = cli_addr.len();
                        let cli_ptr =
                            cli_addr.as_ptr() as *const libc::sockaddr_storage as *const _;

                        const BUF: usize = s::UDP_PKT_SIZE;
                        let batch = cfg.udp_batch_size.min(s::UDP_BATCH_MAX);
                        let mut bufs: [[u8; BUF]; s::UDP_BATCH_MAX] =
                            [[0u8; BUF]; s::UDP_BATCH_MAX];
                        let mut lens: [u32; s::UDP_BATCH_MAX] = [0; s::UDP_BATCH_MAX];
                        let mut addrs: [libc::sockaddr_storage; s::UDP_BATCH_MAX] =
                            unsafe { std::mem::zeroed() };
                        loop {
                            let n = s::udp_recvmmsg(
                                up_raw,
                                &mut bufs,
                                &mut lens,
                                &mut addrs,
                                batch,
                            );
                            if n == 0 {
                                break;
                            }
                            let slices: Vec<&[u8]> = (0..n)
                                .map(|i| &bufs[i][..lens[i] as usize])
                                .collect();
                            let sent = s::udp_sendmmsg_to(
                                udp_listen.as_raw_fd(),
                                &slices,
                                unsafe { &*(cli_ptr as *const libc::sockaddr_storage) },
                                cli_addr_len as u32,
                                n,
                            );
                            if sent > 0 {
                                assoc.last_act = now;
                            }
                        }
                    }
                    continue;
                }
            }

            // ── TCP connection event ──────────────────────────────────────────
            let (ev_gen, idx, is_remote) = decode_token(token);
            let conn = match conns.get_mut(idx) {
                Some(Some(c)) if c.gen == ev_gen => c,
                _ => continue, // stale event (ABA) or empty slot
            };

            // Handle in-progress connect completing on fd_r.
            if is_remote && conn.connecting {
                if ev_flags.intersects(
                    EpollFlags::EPOLLOUT
                        | EpollFlags::EPOLLERR
                        | EpollFlags::EPOLLHUP
                        | EpollFlags::EPOLLRDHUP,
                ) {
                    let err = nix::sys::socket::getsockopt(
                        &conn.fd_r,
                        nix::sys::socket::sockopt::SocketError,
                    );
                    if err != Ok(0) {
                        log::push("Connect failed");
                        free_slot(&mut conns, &mut free_slots, idx);
                        continue;
                    }
                    conn.connecting = false;
                    conn_watch(conn, &epoll, idx, splice_chunk);
                }
                continue;
            }

            // Immediate close on EPOLLERR (matches C version behaviour).
            if ev_flags.contains(EpollFlags::EPOLLERR) {
                log::push(format!(
                    "Connection Error: {}",
                    if is_remote { "Remote" } else { "Local" }
                ));
                free_slot(&mut conns, &mut free_slots, idx);
                continue;
            }

            // EPOLLRDHUP/EPOLLIN/EPOLLOUT: pump() detects EOF from splice()==0.
            // EPOLLRDHUP gives us instant half-close detection (#3).

            // ── Pump local → remote ──
            if !conn.eof_l2r {
                let res = pump(
                    conn.fd_l.as_fd(),
                    conn.fd_r.as_fd(),
                    conn.pipe_l2r_w.as_fd(),
                    conn.pipe_l2r_r.as_fd(),
                    &mut conn.len_l2r,
                    splice_chunk,
                    now,
                    &mut conn.last_l2r,
                );
                match res {
                    PumpStatus::Err => {
                        log::push("Connection Error: Local->Remote");
                        free_slot(&mut conns, &mut free_slots, idx);
                        continue;
                    }
                    PumpStatus::Eof => {
                        conn.eof_l2r = true;
                        if !conn.eof_r2l && conn.half_close_since.is_none() {
                            conn.half_close_since = Some(now);
                        } else if conn.eof_r2l {
                            conn.half_close_since = None;
                        }
                        log::push("EOF: Local->Remote");
                    }
                    PumpStatus::Ok => {}
                }
            } else if conn.len_l2r > 0 {
                // Drain residual pipe data after EOF.
                let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;
                let mut drain_err = false;
                while conn.len_l2r > 0 {
                    match splice(
                        conn.pipe_l2r_r.as_fd(),
                        None,
                        conn.fd_r.as_fd(),
                        None,
                        conn.len_l2r,
                        flags,
                    ) {
                        Ok(n) if n > 0 => {
                            conn.len_l2r -= n;
                            conn.last_l2r = now;
                        }
                        Err(Errno::EAGAIN) => break,
                        Err(Errno::EINTR) => continue,
                        _ => {
                            drain_err = true;
                            break;
                        }
                    }
                }
                if drain_err {
                    free_slot(&mut conns, &mut free_slots, idx);
                    continue;
                }
            }

            if conn.eof_l2r && conn.len_l2r == 0 && !conn.shut_wr_r {
                s::shutdown_write(conn.fd_r.as_raw_fd());
                conn.shut_wr_r = true;
            }

            // ── Pump remote → local ──
            if !conn.eof_r2l {
                let res = pump(
                    conn.fd_r.as_fd(),
                    conn.fd_l.as_fd(),
                    conn.pipe_r2l_w.as_fd(),
                    conn.pipe_r2l_r.as_fd(),
                    &mut conn.len_r2l,
                    splice_chunk,
                    now,
                    &mut conn.last_r2l,
                );
                match res {
                    PumpStatus::Err => {
                        log::push("Connection Error: Remote->Local");
                        free_slot(&mut conns, &mut free_slots, idx);
                        continue;
                    }
                    PumpStatus::Eof => {
                        conn.eof_r2l = true;
                        if !conn.eof_l2r && conn.half_close_since.is_none() {
                            conn.half_close_since = Some(now);
                        } else if conn.eof_l2r {
                            conn.half_close_since = None;
                        }
                        log::push("EOF: Remote->Local");
                    }
                    PumpStatus::Ok => {}
                }
            } else if conn.len_r2l > 0 {
                let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;
                let mut drain_err = false;
                while conn.len_r2l > 0 {
                    match splice(
                        conn.pipe_r2l_r.as_fd(),
                        None,
                        conn.fd_l.as_fd(),
                        None,
                        conn.len_r2l,
                        flags,
                    ) {
                        Ok(n) if n > 0 => {
                            conn.len_r2l -= n;
                            conn.last_r2l = now;
                        }
                        Err(Errno::EAGAIN) => break,
                        Err(Errno::EINTR) => continue,
                        _ => {
                            drain_err = true;
                            break;
                        }
                    }
                }
                if drain_err {
                    free_slot(&mut conns, &mut free_slots, idx);
                    continue;
                }
            }

            if conn.eof_r2l && conn.len_r2l == 0 && !conn.shut_wr_l {
                s::shutdown_write(conn.fd_l.as_raw_fd());
                conn.shut_wr_l = true;
                // conn_watch below will NOT re-arm EPOLLOUT on fd_l because
                // shut_wr_l is only set when eof_r2l && len_r2l == 0, so the
                // guard `conn.len_r2l > 0` in conn_watch is also false.  If
                // another code path ever sets shut_wr_l without draining len_r2l
                // first, EPOLLOUT would re-arm on a shut-down fd and busy-loop.
            }

            if conn.eof_l2r && conn.eof_r2l && conn.len_l2r == 0 && conn.len_r2l == 0 {
                log::push("Connection Fully Closed");
                free_slot(&mut conns, &mut free_slots, idx);
                continue;
            }

            conn_watch(conn, &epoll, idx, splice_chunk);
        }

        // ── Periodic cleanup (1 Hz) ──────────────────────────────────────────
        if now.duration_since(last_cleanup) > Duration::from_secs(1) {
            last_cleanup = now;

            let mut i = 0;
            while i < conns.len() {
                let should_remove = if let Some(Some(ref conn)) = conns.get(i) {
                    if conn.connecting
                        && now.duration_since(conn.connect_start)
                            > Duration::from_secs(cfg.connect_timeout)
                    {
                        log::push("Connect timeout");
                        true
                    } else if !conn.connecting {
                        let last = conn.last_l2r.max(conn.last_r2l);
                        if now.duration_since(last) > Duration::from_secs(cfg.idle_timeout) {
                            log::push(format!("Timeout({}s): Local->Remote", cfg.idle_timeout));
                            log::push(format!("Timeout({}s): Remote->Local", cfg.idle_timeout));
                            true
                        } else if let Some(hs) = conn.half_close_since {
                            if now.duration_since(hs) > Duration::from_secs(cfg.half_close_timeout)
                            {
                                log::push(format!(
                                    "Half-close timeout({}s)",
                                    cfg.half_close_timeout
                                ));
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if should_remove {
                    // OwnedFd Drop closes the fds; epoll auto-removes closed fds on Linux.
                    free_slot(&mut conns, &mut free_slots, i);
                    continue;
                }
                i += 1;
            }

            for slot_idx in 0..udp_slots.len() {
                let expired = if let Some(ref a) = udp_slots[slot_idx] {
                    now.duration_since(a.last_act) > Duration::from_secs(cfg.udp_idle_timeout)
                } else {
                    false
                };
                if expired {
                    if let Some(ref a) = udp_slots[slot_idx] {
                        let _ = epoll.delete(a.up_fd.as_fd());
                        if let Some(net) = a.cli_net_addr {
                            udp_map.remove(&net);
                        }
                    }
                    udp_slots[slot_idx] = None; // OwnedFd drops → fd closed
                    udp_free.push_back(slot_idx);
                }
            }

            // ── Stats log (1 Hz) ──
            let total = STAT_TOTAL_ACCEPTS.load(Ordering::Relaxed);
            let hits = STAT_POOL_HITS.load(Ordering::Relaxed);
            let active = conns.iter().filter(|c| c.is_some()).count();
            let pool_sz = if cfg.pool_size > 0 {
                pool.lock().unwrap().len()
            } else {
                0
            };
            let rate = if total > 0 {
                (hits as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            log::push(format!(
                "Stats: pool={pool_sz}/{} hit={:.0}% ({hits}/{total}) active_tcp={active}",
                cfg.pool_size, rate,
            ));
        }
    }

    // ── Graceful shutdown (#7): flush log and let local variables drop ────────
    log::flush_all();
    // All OwnedFds (conns, udp_slots, tcp_listen, udp_listen, epoll) close on drop.
}

// ── Direct (non-pool) connect ────────────────────────────────────────────────

enum ConnectState {
    Connected(OwnedFd),
    Connecting(OwnedFd),
}

fn direct_connect(addr: &SockAddr, cfg: &Config) -> Result<ConnectState, ()> {
    let sock = s::create_tcp_socket(addr.domain(), cfg, None).map_err(|_| ())?;
    let owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(sock.into_raw_fd()) };

    let nix_addr = unsafe {
        nix::sys::socket::SockaddrStorage::from_raw(
            addr.as_ptr() as *const libc::sockaddr,
            Some(addr.len()),
        )
    };
    match nix_addr {
        Some(na) => match connect(owned.as_raw_fd(), &na) {
            Ok(()) => Ok(ConnectState::Connected(owned)),
            Err(Errno::EINPROGRESS) => Ok(ConnectState::Connecting(owned)),
            Err(_) => Err(()), // owned drops → fd closed
        },
        None => Err(()),
    }
}

// ── Pipe helpers ─────────────────────────────────────────────────────────────

fn make_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use nix::fcntl::OFlag;
    pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}

fn tune_pipe(fd: BorrowedFd<'_>, splice_chunk: usize) {
    // Clamp to the same range validated by Config::validate() so that the pipe
    // capacity always matches splice_chunk exactly.
    let size = splice_chunk.clamp(16 * 1024, 1024 * 1024) as i32;
    let _ = nix::fcntl::fcntl(fd, FcntlArg::F_SETPIPE_SZ(size));
}
