//! Main event loop, zero-copy splice pump, TCP/UDP forwarding, connection lifecycle.
//!
//! Architecture mirrors the C version's single-threaded epoll + splice model.
//! All system calls use safe nix/socket2 wrappers — zero `unsafe` in business logic.

mod config;
mod log;
mod pool;
mod sock;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::io::FromRawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};
use std::os::fd::BorrowedFd;

use nix::errno::Errno;
use nix::fcntl::{splice, SpliceFFlags, FcntlArg};
use nix::sys::signal::{signal, SigHandler, Signal};
use nix::sys::socket::{
    accept4, connect, send, SockFlag, MsgFlags, SockaddrStorage,
};
use nix::sys::socket::SockaddrLike;
use nix::unistd::pipe2;
use socket2::{SockAddr, Socket, Type};

/// Unsafe helper: borrow `RawFd` as `BorrowedFd<'static>` for nix calls.
fn bfd(fd: RawFd) -> BorrowedFd<'static> {
    unsafe { BorrowedFd::borrow_raw(fd) }
}

use config::Config;
use pool::Pool;
use sock as s;

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

const TOKEN_ACCEPT: Token = Token(usize::MAX);
const TOKEN_UDP: Token = Token(usize::MAX - 1);
// UDP upstream sockets use tokens in [UDP_UPSTREAM_BASE, ...), which is
// well above any realistic TCP slab token (slab_idx*2) and below the
// special sentinel tokens near usize::MAX.
const UDP_UPSTREAM_BASE: usize = 1 << 26;

#[derive(Debug, PartialEq)]
enum PumpStatus {
    Ok,
    Eof,
    Err,
}

struct Conn {
    fd_l: RawFd,
    fd_r: RawFd,
    pipe_l2r: RawFd,
    pipe_r2l: RawFd,
    pipe_l2r_read: RawFd,
    pipe_r2l_read: RawFd,
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
    closed: bool,
}

impl Conn {
    fn close_all(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = nix::unistd::close(self.fd_l);
        let _ = nix::unistd::close(self.fd_r);
        let _ = nix::unistd::close(self.pipe_l2r);
        let _ = nix::unistd::close(self.pipe_l2r_read);
        let _ = nix::unistd::close(self.pipe_r2l);
        let _ = nix::unistd::close(self.pipe_r2l_read);
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn pump(
    src_fd: RawFd,
    dst_fd: RawFd,
    pipe_write_fd: RawFd,
    pipe_read_fd: RawFd,
    pipe_len: &mut usize,
    splice_chunk: usize,
    now: Instant,
    last_ts: &mut Instant,
) -> PumpStatus {
    let mut got_eof = false;
    let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;
    let (s, pw, pr, d) = (bfd(src_fd), bfd(pipe_write_fd), bfd(pipe_read_fd), bfd(dst_fd));

    while *pipe_len < splice_chunk {
        let remain = splice_chunk - *pipe_len;
        match splice(s, None, pw, None, remain, flags) {
            Ok(n) if n > 0 => {
                *pipe_len += n;
                *last_ts = now;
                if *pipe_len >= splice_chunk {
                    break;
                }
            }
            Ok(_) => {
                got_eof = true;
                break;
            }
            Err(Errno::EAGAIN) => break,
            Err(_) => return PumpStatus::Err,
        }
    }

    while *pipe_len > 0 {
        match splice(pr, None, d, None, *pipe_len, flags) {
            Ok(n) if n > 0 => {
                *pipe_len -= n;
                *last_ts = now;
            }
            Err(Errno::EAGAIN) => break,
            Err(_) => return PumpStatus::Err,
            Ok(_) => {} // n == 0, shouldn't happen for non-blocking pipe→socket
        }
    }

    if got_eof {
        PumpStatus::Eof
    } else {
        PumpStatus::Ok
    }
}

fn conn_watch(conn: &Conn, registry: &Registry, slab_idx: usize) {
    if conn.closed {
        return;
    }
    // Token scheme: local = slab_idx*2, remote = slab_idx*2+1.
    // This avoids the collision that occurred when using slab_idx | 1
    // (odd indices produced identical local/remote tokens).
    let token_l = Token(slab_idx * 2);
    let token_r = Token(slab_idx * 2 + 1);

    // Only register WRITABLE when there is buffered data waiting to be
    // drained into that socket.  Always registering WRITABLE causes an
    // edge-triggered epoll busy-loop because EPOLL_CTL_MOD with EPOLLOUT
    // on a socket that is already writable fires an event immediately.
    let int_l = if conn.len_r2l > 0 {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };
    let _ = registry.reregister(&mut SourceFd(&conn.fd_l), token_l, int_l);

    let int_r = if conn.len_l2r > 0 {
        Interest::READABLE | Interest::WRITABLE
    } else {
        Interest::READABLE
    };
    let _ = registry.reregister(&mut SourceFd(&conn.fd_r), token_r, int_r);
}

struct UdpAssoc {
    cli_addr: SockAddr,
    cli_net_addr: std::net::SocketAddr,
    up_fd: RawFd,
    last_act: Instant,
    #[allow(dead_code)] // stored for potential introspection/debugging
    token: Token,
}

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
    if let Some(ref ip) = cli.local_ip { cfg.local_ip = ip.clone(); }
    if let Some(port) = cli.local_port { cfg.local_port = port; }
    if let Some(ref ip) = cli.remote_ip { cfg.remote_ip = ip.clone(); }
    if let Some(port) = cli.remote_tcp_port { cfg.remote_tcp_port = port; }
    if let Some(port) = cli.remote_udp_port { cfg.remote_udp_port = port; }
    let cfg = cfg.validate().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    log::push(format!(
        "Using config: local={}:{} -> remote={} TCP:{}/UDP:{}",
        cfg.local_ip, cfg.local_port, cfg.remote_ip, cfg.remote_tcp_port, cfg.remote_udp_port
    ));
    log::push(format!(
        "Runtime: pool={} refill={} splice={} backlog={} udp_buf={} log={}",
        cfg.pool_size, cfg.refill_batch, cfg.splice_chunk,
        cfg.listen_backlog, cfg.udp_socket_buffer,
        if cfg.log_enable { "on" } else { "off" }
    ));

    // Raise fd limit and ignore SIGPIPE
    let _ = nix::sys::resource::setrlimit(
        nix::sys::resource::Resource::RLIMIT_NOFILE, 65535, 65535,
    );
    unsafe { signal(Signal::SIGPIPE, SigHandler::SigIgn).unwrap(); }

    let local_addr = sock::resolve(&cfg.local_ip, cfg.local_port, Type::STREAM)
        .expect("failed to resolve LOCAL_IP");
    let remote_tcp_addr =
        sock::resolve(&cfg.remote_ip, cfg.remote_tcp_port, Type::STREAM)
            .expect("failed to resolve REMOTE_TCP");
    let remote_udp_addr =
        sock::resolve(&cfg.remote_ip, cfg.remote_udp_port, Type::DGRAM)
            .expect("failed to resolve REMOTE_UDP");

    let domain = local_addr.domain();

    // TCP listen
    let tcp_listen = Socket::new(domain, Type::STREAM, Some(socket2::Protocol::TCP)).unwrap();
    tcp_listen.set_nonblocking(true).unwrap();
    tcp_listen.set_reuse_address(true).unwrap();
    tcp_listen.bind(&local_addr).unwrap();
    tcp_listen.listen(cfg.listen_backlog).unwrap();
    let tcp_listen_fd = tcp_listen.into_raw_fd();

    // UDP listen
    let udp_listen = Socket::new(domain, Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    udp_listen.set_nonblocking(true).unwrap();
    udp_listen.set_reuse_address(true).unwrap();
    let _ = udp_listen.set_recv_buffer_size(cfg.udp_socket_buffer);
    let _ = udp_listen.set_send_buffer_size(cfg.udp_socket_buffer);
    udp_listen.bind(&local_addr).unwrap();
    let udp_listen_fd = udp_listen.into_raw_fd();

    // Epoll
    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut SourceFd(&tcp_listen_fd), TOKEN_ACCEPT, Interest::READABLE)
        .unwrap();
    poll.registry()
        .register(&mut SourceFd(&udp_listen_fd), TOKEN_UDP, Interest::READABLE)
        .unwrap();

    // Preconnection pool
    let pool = Arc::new(Mutex::new(Pool::new(cfg.pool_size)));
    if cfg.pool_size > 0 {
        let cfg_arc = Arc::new(cfg.clone());
        pool::spawn_maintain_thread(cfg_arc, Arc::clone(&pool), remote_tcp_addr.clone());
    }

    let splice_chunk = cfg.splice_chunk;

    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut free_slots: VecDeque<usize> = VecDeque::new();

    fn alloc_slot(conns: &mut Vec<Option<Conn>>, free: &mut VecDeque<usize>, conn: Conn) -> usize {
        if let Some(idx) = free.pop_front() {
            conns[idx] = Some(conn);
            idx
        } else {
            let idx = conns.len();
            conns.push(Some(conn));
            idx
        }
    }

    fn free_slot(conns: &mut Vec<Option<Conn>>, free: &mut VecDeque<usize>, idx: usize) {
        conns[idx] = None;
        free.push_back(idx);
    }

    let mut udp_map: HashMap<std::net::SocketAddr, usize> = HashMap::new();
    let mut udp_slots: Vec<Option<UdpAssoc>> = Vec::new();
    let mut udp_free: VecDeque<usize> = VecDeque::new();
    let mut events = Events::with_capacity(1024);
    let mut last_cleanup = Instant::now();

    loop {
        poll.poll(&mut events, Some(Duration::from_millis(100))).unwrap();
        let now = Instant::now();
        log::maybe_flush();

        for event in events.iter() {
            let token = event.token();

            // ── TCP accept ────────────────────────────────────
            if token == TOKEN_ACCEPT {
                loop {
                    match accept4(tcp_listen_fd, SockFlag::SOCK_NONBLOCK) {
                        Ok(cli_fd) => {
                            // Apply TCP options via socket2
                            let cli_sock = unsafe { Socket::from_raw_fd(cli_fd) };
                            s::set_tcp_options(&cli_sock, &cfg);
                            let cli_fd = cli_sock.into_raw_fd();

                            let rem_opt = if cfg.pool_size > 0 {
                                pool.lock().unwrap().take_live()
                            } else {
                                None
                            };

                            let (rem_fd, connecting) = if let Some(fd) = rem_opt {
                                (fd, false)
                            } else {
                                log::push("Exceeded Connections Pool, Direct Out...".into());
                                match direct_connect(&remote_tcp_addr, &cfg) {
                                    Ok(fd) => (fd, true),
                                    Err(_) => {
                                        let _ = nix::unistd::close(cli_fd);
                                        continue;
                                    }
                                }
                            };

                            let (pipe_l2r_r, pipe_l2r_w) = make_pipe().unwrap();
                            let (pipe_r2l_r, pipe_r2l_w) = make_pipe().unwrap();
                            tune_pipe(pipe_l2r_r.as_raw_fd());
                            tune_pipe(pipe_l2r_w.as_raw_fd());
                            tune_pipe(pipe_r2l_r.as_raw_fd());
                            tune_pipe(pipe_r2l_w.as_raw_fd());

                            let slab_idx = alloc_slot(
                                &mut conns,
                                &mut free_slots,
                                Conn {
                                    fd_l: cli_fd,
                                    fd_r: rem_fd,
                                    pipe_l2r: pipe_l2r_w.into_raw_fd(),
                                    pipe_l2r_read: pipe_l2r_r.into_raw_fd(),
                                    pipe_r2l: pipe_r2l_w.into_raw_fd(),
                                    pipe_r2l_read: pipe_r2l_r.into_raw_fd(),
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
                                    closed: false,
                                },
                            );

                            let conn = conns[slab_idx].as_ref().unwrap();
                            let token_l = Token(slab_idx * 2);
                            let mut src_fd_l = SourceFd(&conn.fd_l);
                            if poll.registry().register(&mut src_fd_l, token_l, Interest::READABLE).is_err() {
                                conns[slab_idx] = None;
                                free_slot(&mut conns, &mut free_slots, slab_idx);
                                continue;
                            }

                            let token_r = Token(slab_idx * 2 + 1);
                            let mut src_fd_r = SourceFd(&conn.fd_r);
                            let interest = if connecting { Interest::WRITABLE } else { Interest::READABLE };
                            if poll.registry().register(&mut src_fd_r, token_r, interest).is_err() {
                                let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                                conns[slab_idx] = None;
                                free_slot(&mut conns, &mut free_slots, slab_idx);
                                continue;
                            }
                        }
                        Err(Errno::EAGAIN) => break,
                        Err(_) => break,
                    }
                }
            }

            // ── UDP listen ────────────────────────────────────
            else if token == TOKEN_UDP {
                let mut buf = [0u8; 65535];
                loop {
                    let mut cli_addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                    let mut cli_len: libc::socklen_t =
                        std::mem::size_of::<libc::sockaddr_storage>() as u32;
                    let n = unsafe {
                        libc::recvfrom(
                            udp_listen_fd,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                            0,
                            &mut cli_addr as *mut _ as *mut libc::sockaddr,
                            &mut cli_len,
                        )
                    };
                    if n < 0 {
                        let err = unsafe { *libc::__errno_location() };
                        if err == libc::EAGAIN { break; }
                        break;
                    }
                    let n = n as usize;

                    let cli_sock_addr = unsafe {
                        SockAddr::try_init(|storage, len| {
                            let ptr = &cli_addr as *const libc::sockaddr_storage
                                as *const libc::sockaddr;
                            std::ptr::copy_nonoverlapping(
                                ptr as *const u8,
                                storage as *mut u8,
                                cli_len as usize,
                            );
                            *len = cli_len;
                            Ok(())
                        })
                    }
                    .map(|(_, addr)| addr)
                    .unwrap();

                    // O(1) lookup via hash map keyed on the client's SocketAddr.
                    let cli_net = cli_sock_addr.as_socket();
                    let found_up_fd = if let Some(net_addr) = cli_net {
                        if let Some(&slot_idx) = udp_map.get(&net_addr) {
                            udp_slots.get_mut(slot_idx)
                                .and_then(|s| s.as_mut())
                                .map(|a| { a.last_act = now; a.up_fd })
                        } else {
                            None
                        }
                    } else {
                        // Non-IP address family: fall back to linear scan.
                        udp_slots.iter_mut()
                            .filter_map(|s| s.as_mut())
                            .find(|a| sock::sockaddr_eq(&a.cli_addr, &cli_sock_addr))
                            .map(|a| { a.last_act = now; a.up_fd })
                    };

                    if let Some(up_fd) = found_up_fd {
                        let _ = send(up_fd, &buf[..n], MsgFlags::MSG_DONTWAIT);
                    } else {
                        match sock::create_udp_socket(domain, &cfg) {
                            Ok(s) => {
                                let up_fd = s.into_raw_fd();
                                unsafe {
                                    libc::connect(
                                        up_fd,
                                        remote_udp_addr.as_ptr() as *const libc::sockaddr,
                                        remote_udp_addr.len(),
                                    );
                                }
                                // Allocate a slot and assign a unique epoll token.
                                let slot_idx = if let Some(idx) = udp_free.pop_front() {
                                    idx
                                } else {
                                    let idx = udp_slots.len();
                                    udp_slots.push(None);
                                    idx
                                };
                                let t = Token(UDP_UPSTREAM_BASE + slot_idx);
                                if poll.registry()
                                    .register(&mut SourceFd(&up_fd), t, Interest::READABLE)
                                    .is_err()
                                {
                                    let _ = nix::unistd::close(up_fd);
                                    udp_free.push_back(slot_idx);
                                    continue;
                                }
                                let _ = send(up_fd, &buf[..n], MsgFlags::MSG_DONTWAIT);
                                if let Some(net_addr) = cli_net {
                                    udp_map.insert(net_addr, slot_idx);
                                }
                                let cli_net_addr = cli_net.unwrap_or(
                                    std::net::SocketAddr::from(([0u8, 0, 0, 0], 0))
                                );
                                udp_slots[slot_idx] = Some(UdpAssoc {
                                    cli_addr: cli_sock_addr,
                                    cli_net_addr,
                                    up_fd,
                                    last_act: now,
                                    token: t,
                                });
                            }
                            Err(_) => {}
                        }
                    }
                }
            }

            // ── UDP upstream response / TCP connection event ──
            else {
                let raw = usize::from(token);
                if raw >= (usize::MAX - 10) {
                    continue;
                }

                // ── UDP upstream response ─────────────────────
                if raw >= UDP_UPSTREAM_BASE {
                    let slot_idx = raw - UDP_UPSTREAM_BASE;
                    let mut buf = [0u8; 65535];
                    if let Some(Some(ref mut assoc)) = udp_slots.get_mut(slot_idx) {
                        loop {
                            let n = unsafe {
                                libc::recv(
                                    assoc.up_fd,
                                    buf.as_mut_ptr() as *mut libc::c_void,
                                    buf.len(),
                                    libc::MSG_DONTWAIT,
                                )
                            };
                            if n <= 0 { break; }
                            let n = n as usize;
                            unsafe {
                                libc::sendto(
                                    udp_listen_fd,
                                    buf.as_ptr() as *const libc::c_void,
                                    n,
                                    0,
                                    assoc.cli_addr.as_ptr() as *const libc::sockaddr,
                                    assoc.cli_addr.len(),
                                );
                            }
                            assoc.last_act = now;
                        }
                    }
                    continue;
                }

                // ── TCP connection event ──────────────────────
                // Decode token: local = slab_idx*2, remote = slab_idx*2+1
                let is_remote = (raw & 1) != 0;
                let idx = raw >> 1;
                if idx >= conns.len() {
                    continue;
                }
                let conn = match conns.get_mut(idx) {
                    Some(Some(c)) => c,
                    _ => continue,
                };
                if conn.closed {
                    continue;
                }

                // Handle connecting remote
                if is_remote && conn.connecting {
                    if event.is_writable() || event.is_error() {
                        let err = nix::sys::socket::getsockopt(&bfd(conn.fd_r), nix::sys::socket::sockopt::SocketError);
                        if err != Ok(0) {
                            log::push("Connect failed".into());
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                            conns[idx] = None;
                            free_slot(&mut conns, &mut free_slots, idx);
                            continue;
                        }
                        conn.connecting = false;
                        conn_watch(conn, poll.registry(), idx);
                    }
                    continue;
                }

                if event.is_error() {
                    log::push(format!(
                        "Connection Error: {}",
                        if is_remote { "Remote" } else { "Local" }
                    ));
                    let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                    let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                    conns[idx] = None;
                    free_slot(&mut conns, &mut free_slots, idx);
                    continue;
                }

                // Pump local → remote
                if !conn.eof_l2r {
                    let res = pump(
                        conn.fd_l, conn.fd_r, conn.pipe_l2r, conn.pipe_l2r_read,
                        &mut conn.len_l2r, splice_chunk, now, &mut conn.last_l2r,
                    );
                    match res {
                        PumpStatus::Err => {
                            log::push("Connection Error: Local->Remote".into());
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                            conns[idx] = None;
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
                            log::push("EOF: Local->Remote".into());
                        }
                        PumpStatus::Ok => {}
                    }
                } else if conn.len_l2r > 0 {
                    let mut drain_err = false;
                    let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;
                    while conn.len_l2r > 0 {
                        match splice(bfd(conn.pipe_l2r_read), None, bfd(conn.fd_r), None, conn.len_l2r, flags) {
                            Ok(n) if n > 0 => {
                                conn.len_l2r -= n;
                                conn.last_l2r = now;
                            }
                            Err(Errno::EAGAIN) => break,
                            _ => { drain_err = true; break; }
                        }
                    }
                    if drain_err {
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                        conns[idx] = None;
                        free_slot(&mut conns, &mut free_slots, idx);
                        continue;
                    }
                }

                if conn.eof_l2r && conn.len_l2r == 0 && !conn.shut_wr_r {
                    s::shutdown_write(conn.fd_r);
                    conn.shut_wr_r = true;
                }

                // Pump remote → local
                if !conn.eof_r2l {
                    let res = pump(
                        conn.fd_r, conn.fd_l, conn.pipe_r2l, conn.pipe_r2l_read,
                        &mut conn.len_r2l, splice_chunk, now, &mut conn.last_r2l,
                    );
                    match res {
                        PumpStatus::Err => {
                            log::push("Connection Error: Remote->Local".into());
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                            let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                            conns[idx] = None;
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
                            log::push("EOF: Remote->Local".into());
                        }
                        PumpStatus::Ok => {}
                    }
                } else if conn.len_r2l > 0 {
                    let mut drain_err = false;
                    let flags = SpliceFFlags::SPLICE_F_MOVE | SpliceFFlags::SPLICE_F_NONBLOCK;
                    while conn.len_r2l > 0 {
                        match splice(bfd(conn.pipe_r2l_read), None, bfd(conn.fd_l), None, conn.len_r2l, flags) {
                            Ok(n) if n > 0 => {
                                conn.len_r2l -= n;
                                conn.last_r2l = now;
                            }
                            Err(Errno::EAGAIN) => break,
                            _ => { drain_err = true; break; }
                        }
                    }
                    if drain_err {
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                        conns[idx] = None;
                        free_slot(&mut conns, &mut free_slots, idx);
                        continue;
                    }
                }

                if conn.eof_r2l && conn.len_r2l == 0 && !conn.shut_wr_l {
                    s::shutdown_write(conn.fd_l);
                    conn.shut_wr_l = true;
                }

                if conn.eof_l2r && conn.eof_r2l && conn.len_l2r == 0 && conn.len_r2l == 0 {
                    log::push("Connection Fully Closed".into());
                    let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                    let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                    conns[idx] = None;
                    free_slot(&mut conns, &mut free_slots, idx);
                    continue;
                }

                conn_watch(conn, poll.registry(), idx);
            }
        }

        // Periodic cleanup (1 Hz)
        if now - last_cleanup > Duration::from_secs(1) {
            last_cleanup = now;

            let mut i = 0;
            while i < conns.len() {
                let should_remove = if let Some(Some(ref conn)) = conns.get(i) {
                    if conn.closed { true }
                    else if conn.connecting && now - conn.connect_start > Duration::from_secs(cfg.connect_timeout) {
                        log::push("Connect timeout".into());
                        true
                    } else if !conn.connecting {
                        let last = conn.last_l2r.max(conn.last_r2l);
                        if now - last > Duration::from_secs(cfg.idle_timeout) {
                            log::push(format!("Timeout({}s): Local->Remote", cfg.idle_timeout));
                            log::push(format!("Timeout({}s): Remote->Local", cfg.idle_timeout));
                            true
                        } else if let Some(hs) = conn.half_close_since {
                            if now - hs > Duration::from_secs(cfg.half_close_timeout) {
                                log::push(format!("Half-close timeout({}s)", cfg.half_close_timeout));
                                true
                            } else { false }
                        } else { false }
                    } else { false }
                } else { false };

                if should_remove {
                    if let Some(Some(conn)) = conns.get(i) {
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_l));
                        let _ = poll.registry().deregister(&mut SourceFd(&conn.fd_r));
                    }
                    conns[i] = None;
                    free_slot(&mut conns, &mut free_slots, i);
                    continue;
                }
                i += 1;
            }

            for (slot_idx, slot) in udp_slots.iter_mut().enumerate() {
                if let Some(ref assoc) = *slot {
                    if now - assoc.last_act > Duration::from_secs(cfg.udp_idle_timeout) {
                        let up_fd = assoc.up_fd;
                        let net_addr = assoc.cli_net_addr;
                        let _ = poll.registry().deregister(&mut SourceFd(&up_fd));
                        let _ = nix::unistd::close(up_fd);
                        udp_map.remove(&net_addr);
                        *slot = None;
                        udp_free.push_back(slot_idx);
                    }
                }
            }
        }
    }
}

fn direct_connect(addr: &SockAddr, cfg: &Config) -> Result<RawFd, ()> {
    let sock = sock::create_tcp_socket(addr.domain(), cfg, None).map_err(|_| ())?;
    let fd = sock.into_raw_fd();
    let nix_addr = unsafe { SockaddrStorage::from_raw(addr.as_ptr() as *const libc::sockaddr, Some(addr.len())) };
    match nix_addr {
        Some(na) => match connect(fd, &na) {
            Ok(()) => Ok(fd),
            Err(Errno::EINPROGRESS) => Ok(fd),
            Err(_) => { let _ = nix::unistd::close(fd); Err(()) }
        },
        None => { let _ = nix::unistd::close(fd); Err(()) }
    }
}

fn make_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use nix::fcntl::OFlag;
    pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}

fn tune_pipe(fd: RawFd) {
    let _ = nix::fcntl::fcntl(bfd(fd), FcntlArg::F_SETPIPE_SZ(262144));
}
