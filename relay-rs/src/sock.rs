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
#[allow(dead_code)]
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

/// Convert a nix `SockaddrStorage` to a `std::net::SocketAddr` (IPv4 or IPv6).
/// Returns `None` for unsupported address families.
pub fn storage_to_net(addr: &nix::sys::socket::SockaddrStorage) -> Option<std::net::SocketAddr> {
    if let Some(v4) = addr.as_sockaddr_in() {
        let raw = v4.as_ref();
        // sin_addr.s_addr is stored in network byte order (big-endian).
        // to_ne_bytes() on a little-endian machine converts 0x0101A8C0 →
        // [0xC0, 0xA8, 0x01, 0x01] = 192.168.1.1 octets.  On a big-endian
        // machine it works equally since native == network byte order.
        let ip = std::net::Ipv4Addr::from(raw.sin_addr.s_addr.to_ne_bytes());
        let port = u16::from_be(raw.sin_port);
        Some(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), port))
    } else if let Some(v6) = addr.as_sockaddr_in6() {
        let raw = v6.as_ref();
        // s6_addr is a [u8; 16] byte array — already in the right byte order.
        let ip = std::net::Ipv6Addr::from(raw.sin6_addr.s6_addr);
        let port = u16::from_be(raw.sin6_port);
        Some(std::net::SocketAddr::new(std::net::IpAddr::V6(ip), port))
    } else {
        None
    }
}

/// Byte-level equality for two `SockaddrStorage` values.
/// Used as a fallback for address families that don't map to `SocketAddr`.
pub fn nix_storage_eq(
    a: &nix::sys::socket::SockaddrStorage,
    b: &nix::sys::socket::SockaddrStorage,
) -> bool {
    use nix::sys::socket::SockaddrLike;
    let la = a.len() as usize;
    let lb = b.len() as usize;
    if la != lb {
        return false;
    }
    unsafe {
        libc::memcmp(
            a.as_ptr() as *const libc::c_void,
            b.as_ptr() as *const libc::c_void,
            la,
        ) == 0
    }
}

// ── Batched UDP (sendmmsg / recvmmsg) ────────────────────────────────────────

pub const UDP_BATCH_MAX: usize = 128;
pub const UDP_PKT_SIZE: usize = 1500;

/// Receive up to `n` UDP packets from `fd` via recvmmsg(2).
/// Returns count of received packets (0 on EAGAIN or error).
/// Each received packet's source address is written into `addrs[i]` and
/// data size into `lens[i]`.
pub fn udp_recvmmsg(
    fd: RawFd,
    bufs: &mut [[u8; UDP_PKT_SIZE]],
    lens: &mut [u32],
    addrs: &mut [libc::sockaddr_storage],
    n: usize,
) -> usize {
    let n = n.min(UDP_BATCH_MAX).min(bufs.len());
    if n == 0 {
        return 0;
    }

    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(n);
    let mut iovs: Vec<libc::iovec> = Vec::with_capacity(n);

    for i in 0..n {
        iovs.push(libc::iovec {
            iov_base: bufs[i].as_mut_ptr() as *mut libc::c_void,
            iov_len: UDP_PKT_SIZE,
        });
        msgs.push(libc::mmsghdr {
            msg_hdr: libc::msghdr {
                msg_name: &mut addrs[i] as *mut _ as *mut libc::c_void,
                msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as u32,
                msg_iov: &raw mut iovs[i],
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: 0,
        });
    }

    let rc = unsafe {
        libc::recvmmsg(
            fd,
            msgs.as_mut_ptr(),
            n as u32,
            libc::MSG_DONTWAIT,
            std::ptr::null_mut(),
        )
    };
    if rc < 0 {
        return 0;
    }
    let count = rc as usize;
    for i in 0..count {
        lens[i] = msgs[i].msg_len;
    }
    count
}

/// Send up to `n` packets to the same destination via sendmmsg(2).
/// `data[i]` is the payload for the i-th packet.
/// Returns count of packets actually sent.
pub fn udp_sendmmsg_to(
    fd: RawFd,
    data: &[&[u8]],
    addr: &libc::sockaddr_storage,
    addr_len: libc::socklen_t,
    n: usize,
) -> usize {
    let n = n.min(UDP_BATCH_MAX).min(data.len());
    if n == 0 {
        return 0;
    }

    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(n);
    let mut iovs: Vec<libc::iovec> = Vec::with_capacity(n);
    // Need a mutable copy of addr for the msghdr (safe cast: only read by kernel).
    let addr_ptr = addr as *const libc::sockaddr_storage as *mut libc::c_void;

    for i in 0..n {
        iovs.push(libc::iovec {
            iov_base: data[i].as_ptr() as *mut libc::c_void,
            iov_len: data[i].len(),
        });
        msgs.push(libc::mmsghdr {
            msg_hdr: libc::msghdr {
                msg_name: addr_ptr,
                msg_namelen: addr_len,
                msg_iov: &raw mut iovs[i],
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: 0,
        });
    }

    let rc = unsafe {
        libc::sendmmsg(
            fd,
            msgs.as_mut_ptr(),
            n as u32,
            libc::MSG_DONTWAIT,
        )
    };
    if rc < 0 {
        0
    } else {
        rc as usize
    }
}
