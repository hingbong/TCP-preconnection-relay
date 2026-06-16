use socket2::{Domain, Protocol, SockAddr, Socket, TcpKeepalive, Type};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use crate::config::Config;

/// Apply TCP keepalive and TCP_NODELAY options via socket2 safe API.
pub fn set_tcp_options(sock: &Socket, cfg: &Config) {
    let _ = sock.set_tcp_nodelay(true);

    let mut ka = TcpKeepalive::new();
    ka = ka.with_time(Duration::from_secs(cfg.tcp_keepidle as u64));
    ka = ka.with_interval(Duration::from_secs(cfg.tcp_keepintvl as u64));
    if cfg.tcp_keepcnt > 0 {
        ka = ka.with_retries(cfg.tcp_keepcnt as u32);
    }
    let _ = sock.set_tcp_keepalive(&ka);

    // TCP_USER_TIMEOUT — socket2 safe API
    if cfg.tcp_user_timeout_ms > 0 {
        let _ =
            sock.set_tcp_user_timeout(Some(Duration::from_millis(cfg.tcp_user_timeout_ms as u64)));
    }

    // TCP_QUICKACK — only available via raw setsockopt
    let fd = sock.as_raw_fd();
    let one: i32 = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

/// Create a non-blocking TCP socket with keepalive options.
pub fn create_tcp_socket(
    domain: Domain,
    cfg: &Config,
    bind_addr: Option<&SockAddr>,
) -> std::io::Result<Socket> {
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_nonblocking(true)?;
    set_tcp_options(&sock, cfg);
    if let Some(addr) = bind_addr {
        sock.bind(addr)?;
    }
    Ok(sock)
}

/// Create a non-blocking UDP socket.
pub fn create_udp_socket(domain: Domain, cfg: &Config) -> std::io::Result<Socket> {
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    let sz = cfg.udp_socket_buffer;
    let _ = sock.set_recv_buffer_size(sz);
    let _ = sock.set_send_buffer_size(sz);
    Ok(sock)
}

use std::net::ToSocketAddrs;

/// Resolve `host:port` to a `SockAddr`.
pub fn resolve(host: &str, port: u16, _socktype: Type) -> std::io::Result<SockAddr> {
    use std::net::SocketAddr;
    let addr_str = format!("{host}:{port}");
    if let Ok(addr) = addr_str.parse::<SocketAddr>() {
        return Ok(SockAddr::from(addr));
    }
    let mut addrs = addr_str.to_socket_addrs()?;
    let addr = addrs.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses resolved")
    })?;
    Ok(SockAddr::from(addr))
}

/// Fast dead-socket check using poll(2) with zero timeout.
/// Also detects FIN-only half-close by peeking when POLLIN is set without
/// error flags (kernel sets POLLIN but not POLLHUP when only a FIN arrived).
pub fn socket_dead_fast(fd: RawFd) -> bool {
    use nix::poll::{poll, PollFd, PollFlags};
    use std::os::fd::BorrowedFd;
    let b = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(b, PollFlags::POLLIN)];
    match poll(&mut fds, 0u8) {
        Ok(1) => {
            let revents = fds[0].revents().unwrap_or(PollFlags::empty());
            if revents.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
                return true;
            }
            // POLLIN without an error flag can mean FIN was received.
            // A zero-byte MSG_PEEK recv confirms EOF; negative with a real
            // error (not EAGAIN/EWOULDBLOCK) also means the socket is dead.
            if revents.contains(PollFlags::POLLIN) {
                let mut byte = [0u8; 1];
                let n = unsafe {
                    libc::recv(
                        fd,
                        byte.as_mut_ptr() as *mut libc::c_void,
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if n == 0 {
                    return true; // EOF (FIN received)
                }
                if n < 0 {
                    let e = unsafe { *libc::__errno_location() };
                    if e != libc::EAGAIN && e != libc::EWOULDBLOCK {
                        return true; // real socket error
                    }
                }
            }
            // Final SO_ERROR sweep — catches sockets that were reset between
            // the poll() call and now (matches the C getsockopt check).
            let mut err: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if rc == 0 && err != 0 {
                return true;
            }
            false
        }
        _ => false,
    }
}

/// Compare two sockaddr values for equality (ip + port).
pub fn sockaddr_eq(a: &SockAddr, b: &SockAddr) -> bool {
    let a_len = a.len();
    let b_len = b.len();
    if a_len != b_len {
        return false;
    }
    unsafe {
        libc::memcmp(
            a.as_ptr() as *const libc::c_void,
            b.as_ptr() as *const libc::c_void,
            a_len as usize,
        ) == 0
    }
}

/// Shutdown write half of a socket (send EOF).
pub fn shutdown_write(fd: RawFd) {
    let _ = nix::sys::socket::shutdown(fd, nix::sys::socket::Shutdown::Write);
}
