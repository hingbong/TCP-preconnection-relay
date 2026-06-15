use std::os::fd::AsRawFd;

use libc::{c_int, socklen_t};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::config::Config;

/// Apply TCP keepalive, TCP_NODELAY, and optional TCP_USER_TIMEOUT.
/// Mirrors `set_tcp_socket_options` in the C code.
pub fn set_tcp_options(sock: &Socket, cfg: &Config) {
    let fd = sock.as_raw_fd();

    let one: c_int = 1;
    unsafe {
        // TCP_NODELAY
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
        // SO_KEEPALIVE
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
        // TCP_KEEPIDLE
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPIDLE,
            &cfg.tcp_keepidle as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
        // TCP_KEEPINTVL
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPINTVL,
            &cfg.tcp_keepintvl as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
        // TCP_KEEPCNT
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPCNT,
            &cfg.tcp_keepcnt as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
        // TCP_USER_TIMEOUT (optional)
        if cfg.tcp_user_timeout_ms > 0 {
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_USER_TIMEOUT,
                &cfg.tcp_user_timeout_ms as *const _ as *const libc::c_void,
                std::mem::size_of::<c_int>() as socklen_t,
            );
        }
        // TCP_QUICKACK — disable delayed ACK for low-latency forwarding
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as socklen_t,
        );
    }
}

use std::io;

/// Create a non-blocking TCP socket with keepalive options, bound if local bind is desired.
pub fn create_tcp_socket(
    domain: Domain,
    cfg: &Config,
    bind_addr: Option<&SockAddr>,
) -> io::Result<Socket> {
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_nonblocking(true)?;
    set_tcp_options(&sock, cfg);
    if let Some(addr) = bind_addr {
        sock.bind(addr)?;
    }
    Ok(sock)
}

/// Create a non-blocking UDP socket.
pub fn create_udp_socket(domain: Domain, cfg: &Config) -> io::Result<Socket> {
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    // Set buffer size
    let sz = cfg.udp_socket_buffer;
    let _ = sock.set_recv_buffer_size(sz);
    let _ = sock.set_send_buffer_size(sz);
    Ok(sock)
}

/// Resolve `host:port` to a `SockAddr` for the given socket type.
pub fn resolve(host: &str, port: u16, _socktype: Type) -> io::Result<SockAddr> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let addr_str = format!("{host}:{port}");
    // Try numeric first
    if let Ok(addr) = addr_str.parse::<SocketAddr>() {
        return Ok(SockAddr::from(addr));
    }
    // DNS resolve
    let mut addrs = addr_str
        .to_socket_addrs()?;
    let addr = addrs
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no addresses resolved"))?;
    Ok(SockAddr::from(addr))
}

/// Fast dead-socket check using poll(fd, POLLIN, 0).
/// Returns true if the socket has error/hup (i.e., is dead).
pub fn socket_dead_fast(fd: std::os::fd::RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, 0) };
    if r <= 0 {
        return false;
    }
    (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
}

/// Compare two sockaddr values for equality (ip + port), handling v4 and v6.
pub fn sockaddr_eq(a: &SockAddr, b: &SockAddr) -> bool {
    let a_raw = a.as_ptr();
    let b_raw = b.as_ptr();
    let a_len = a.len();
    let b_len = b.len();

    if a_len != b_len {
        return false;
    }
    // memcmp the raw bytes
    unsafe {
        libc::memcmp(
            a_raw as *const libc::c_void,
            b_raw as *const libc::c_void,
            a_len as usize,
        ) == 0
    }
}

/// Shutdown write half of a socket (send EOF).
pub fn shutdown_write(fd: std::os::fd::RawFd) {
    unsafe {
        libc::shutdown(fd, libc::SHUT_WR);
    }
}
