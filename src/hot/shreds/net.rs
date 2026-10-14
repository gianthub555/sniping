//! Shred receive sockets and batched receive. Ported from pumpfeed (`net.rs`); pinning lives in
//! `hot::affinity`.
//!
//! Two kinds of input, both drained with `recvmmsg(2)` into preallocated
//! buffers (one syscall per batch):
//! * a UDP socket that a shred forwarder (ShredStream proxy) sends to, and
//! * a packet-capture socket (AF_PACKET) that sees the shreds arriving at the
//!   local Solana node's TVU ports: the node's own turbine traffic, read at
//!   the same moment the node receives it, without taking it from the node.

use socket2::{Domain, Protocol, Socket, Type};
use std::{io, net::SocketAddr};

/// Larger than any shred packet (1232 bytes) plus IPv4 and UDP headers.
pub const MAX_PACKET: usize = 1320;

pub struct SocketOpts {
    /// Requested SO_RCVBUF in bytes.
    pub rcvbuf: usize,
    /// SO_BUSY_POLL in microseconds. 0 disables it.
    pub busy_poll_us: u32,
}

/// Creates and binds the non-blocking UDP ingest socket with large buffers and busy polling.
pub fn bind_udp(addr: SocketAddr, o: &SocketOpts) -> io::Result<Socket> {
    // No SO_REUSEADDR: a second instance on the same port must fail, not
    // silently take a share of the shreds.
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Capped by net.core.rmem_max. linux::tune retries with SO_RCVBUFFORCE.
    sock.set_recv_buffer_size(o.rcvbuf)?;
    {
        use std::os::fd::AsRawFd;
        linux::tune(sock.as_raw_fd(), o);
    }
    report_rcvbuf(&sock, o.rcvbuf);
    sock.bind(&addr.into())?;
    sock.set_nonblocking(true)?;
    Ok(sock)
}

fn report_rcvbuf(sock: &Socket, wanted: usize) {
    // Linux reports double the requested value (bookkeeping overhead).
    match sock.recv_buffer_size() {
        Ok(effective) if effective < wanted => {
            log::warn!("shreds: SO_RCVBUF capped at {effective} (wanted {wanted}); raise net.core.rmem_max or run with CAP_NET_ADMIN")
        }
        Ok(effective) => log::info!("shreds: receive buffer {effective} bytes"),
        Err(_) => {}
    }
}

/// UDP destination port ranges (inclusive) to capture, e.g. "8001,8002" or "8000-8020".
pub fn parse_port_ranges(s: &str) -> Option<Vec<(u16, u16)>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse().ok()?),
            None => {
                let p = part.parse().ok()?;
                (p, p)
            }
        };
        if lo > hi {
            return None;
        }
        out.push((lo, hi));
    }
    (!out.is_empty() && out.len() <= 32).then_some(out)
}

pub use linux::{capture_socket, wait_readable};

mod linux {
    use super::SocketOpts;
    use socket2::Socket;
    use std::{
        io,
        os::fd::{AsRawFd, FromRawFd, RawFd},
    };

    const PACKET_IGNORE_OUTGOING: libc::c_int = 23;

    fn setsockopt_int(fd: RawFd, level: libc::c_int, name: libc::c_int, val: libc::c_int) -> io::Result<()> {
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &val as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Best effort: failures are logged, not fatal. Both options need
    /// CAP_NET_ADMIN to go beyond the sysctl limits.
    pub(super) fn tune(fd: RawFd, o: &SocketOpts) {
        let rcvbuf = o.rcvbuf.min(libc::c_int::MAX as usize) as libc::c_int;
        // SO_RCVBUFFORCE ignores net.core.rmem_max.
        if let Err(e) = setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_RCVBUFFORCE, rcvbuf) {
            log::warn!("shreds: SO_RCVBUFFORCE failed (needs CAP_NET_ADMIN): {e}");
        }
        // SO_BUSY_POLL: reads spin on the NIC queue for up to N µs instead of
        // sleeping until an interrupt. Values above net.core.busy_read need CAP_NET_ADMIN.
        if o.busy_poll_us > 0 {
            let us = o.busy_poll_us.min(libc::c_int::MAX as u32) as libc::c_int;
            match setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_BUSY_POLL, us) {
                Ok(()) => log::info!("shreds: SO_BUSY_POLL {us} us"),
                Err(e) => log::warn!("shreds: SO_BUSY_POLL failed (needs CAP_NET_ADMIN above net.core.busy_read): {e}"),
            }
        }
    }

    /// Classic BPF for an AF_PACKET/SOCK_DGRAM socket (offset 0 = IPv4
    /// header): accept unfragmented UDP whose destination port is in one of
    /// `ranges`; drop everything else in the kernel.
    pub(super) fn port_filter(ranges: &[(u16, u16)]) -> Vec<libc::sock_filter> {
        let ins = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
        let n = ranges.len();
        let drop_at = 6 + 2 * n; // index of "ret 0"
        let mut p = vec![
            ins(0x30, 0, 0, 9),                        // ldb [9]            protocol
            ins(0x15, 0, (drop_at - 2) as u8, 17),     // jeq #17 (UDP)
            ins(0x28, 0, 0, 6),                        // ldh [6]            flags + fragment offset
            ins(0x45, (drop_at - 4) as u8, 0, 0x1fff), // jset #0x1fff -> drop fragments
            ins(0xb1, 0, 0, 0),                        // ldxb 4*([0]&0xf)   IP header length
            ins(0x48, 0, 0, 2),                        // ldh [x+2]          UDP destination port
        ];
        for (i, &(lo, hi)) in ranges.iter().enumerate() {
            let accept_off = (2 * n - 2 * i - 1) as u8;
            p.push(ins(0x35, 0, 1, lo as u32)); //          jge #lo, else next range
            p.push(ins(0x25, 0, accept_off, hi as u32)); // jgt #hi -> next range, else accept
        }
        p.push(ins(0x06, 0, 0, 0)); //                      ret #0       (drop)
        p.push(ins(0x06, 0, 0, 0x0004_0000)); //            ret #262144  (accept)
        p
    }

    /// Packet-capture socket for UDP shreds arriving at `ports` on `iface`
    /// ("any" = all interfaces). Needs CAP_NET_RAW. Only incoming packets:
    /// the node's own retransmissions to other nodes are ignored.
    pub fn capture_socket(iface: &str, ports: &[(u16, u16)], o: &SocketOpts) -> io::Result<Socket> {
        let proto = (libc::ETH_P_IP as u16).to_be();
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, proto as libc::c_int) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh socket we own.
        let sock = unsafe { Socket::from_raw_fd(fd) };
        // Filter before bind, so no unfiltered packet is ever queued.
        let filter = port_filter(ports);
        let prog = libc::sock_fprog { len: filter.len() as libc::c_ushort, filter: filter.as_ptr() as *mut libc::sock_filter };
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                &prog as *const libc::sock_fprog as *const libc::c_void,
                std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Err(e) = setsockopt_int(fd, libc::SOL_PACKET, PACKET_IGNORE_OUTGOING, 1) {
            log::warn!("shreds: PACKET_IGNORE_OUTGOING unsupported (kernel < 4.20), outgoing shreds are seen too: {e}");
        }
        let _ = sock.set_recv_buffer_size(o.rcvbuf);
        tune(fd, o);
        super::report_rcvbuf(&sock, o.rcvbuf);
        let ifindex = if iface == "any" {
            0
        } else {
            let name = std::ffi::CString::new(iface).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad interface name"))?;
            match unsafe { libc::if_nametoindex(name.as_ptr()) } {
                0 => return Err(io::Error::new(io::ErrorKind::NotFound, format!("no interface {iface}"))),
                i => i as libc::c_int,
            }
        };
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as libc::c_ushort;
        sll.sll_protocol = proto;
        sll.sll_ifindex = ifindex;
        let rc = unsafe {
            libc::bind(fd, &sll as *const libc::sockaddr_ll as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sock)
    }

    /// Sleeps until one of `socks` is readable or `timeout_ms` passes.
    pub fn wait_readable(socks: &[&Socket], timeout_ms: i32) {
        let mut fds: Vec<libc::pollfd> = socks.iter().map(|s| libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    }
}

// ---------------------------------------------------------------------------
// Batched receive
// ---------------------------------------------------------------------------

/// What a received buffer holds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// The UDP payload (a UDP socket).
    Udp,
    /// A whole IPv4 datagram (a capture socket): the shred follows the IP and UDP headers.
    Ip,
}

/// UDP payload of an IPv4 datagram, or empty if it is not a well-formed UDP packet.
#[inline(always)]
pub fn udp_payload(p: &[u8]) -> &[u8] {
    if p.len() < 28 || p[0] >> 4 != 4 || p[9] != 17 {
        return &[];
    }
    let ihl = (p[0] & 0x0f) as usize * 4;
    let Some(udp) = p.get(ihl..ihl + 8) else { return &[] };
    let end = (ihl + u16::from_be_bytes([udp[4], udp[5]]) as usize).min(p.len());
    p.get(ihl + 8..end).unwrap_or(&[])
}

/// Linux: `recvmmsg` into a fixed set of buffers. The `iovec`/`mmsghdr`
/// arrays point into `bufs` and are built once. Boxed slices never move,
/// so the pointers stay valid when the struct moves. Not `Send`, because
/// it holds raw pointers; create it on the thread that uses it.
pub struct PacketReceiver {
    sock: Socket,
    framing: Framing,
    bufs: Box<[[u8; MAX_PACKET]]>,
    _iovs: Box<[libc::iovec]>,
    hdrs: Box<[libc::mmsghdr]>,
}

impl PacketReceiver {
    /// `sock` must be non-blocking.
    pub fn new(sock: Socket, framing: Framing, batch: usize) -> Self {
        let batch = batch.clamp(1, 1024);
        let mut bufs = vec![[0u8; MAX_PACKET]; batch].into_boxed_slice();
        let mut iovs: Box<[libc::iovec]> = bufs.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: MAX_PACKET }).collect();
        let hdrs: Box<[libc::mmsghdr]> = iovs
            .iter_mut()
            .map(|iov| {
                // SAFETY: mmsghdr is a plain C struct; all-zero is valid.
                let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                h.msg_hdr.msg_iov = iov;
                h.msg_hdr.msg_iovlen = 1 as _;
                h
            })
            .collect();
        Self { sock, framing, bufs, _iovs: iovs, hdrs }
    }

    pub fn socket(&self) -> &Socket {
        &self.sock
    }

    /// Takes up to `batch` queued packets without waiting (WouldBlock when
    /// empty). `packet(i)` is valid for `i < n` until the next call.
    #[inline(always)]
    pub fn recv(&mut self) -> io::Result<usize> {
        use std::os::fd::AsRawFd;
        // SAFETY: hdrs/iovs/bufs are live and consistent for the call.
        let n = unsafe {
            libc::recvmmsg(self.sock.as_raw_fd(), self.hdrs.as_mut_ptr(), self.hdrs.len() as libc::c_uint, libc::MSG_DONTWAIT, std::ptr::null_mut())
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// The shred bytes of packet `i`.
    #[inline(always)]
    pub fn packet(&self, i: usize) -> &[u8] {
        let raw = &self.bufs[i][..(self.hdrs[i].msg_len as usize).min(MAX_PACKET)];
        match self.framing {
            Framing::Udp => raw,
            Framing::Ip => udp_payload(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges_parse() {
        assert_eq!(parse_port_ranges("8001, 8002"), Some(vec![(8001, 8001), (8002, 8002)]));
        assert_eq!(parse_port_ranges("8000-8020"), Some(vec![(8000, 8020)]));
        assert_eq!(parse_port_ranges("9-8"), None);
        assert_eq!(parse_port_ranges(""), None);
    }

    #[test]
    fn udp_payload_skips_ip_and_udp_headers() {
        let mut p = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        p.extend_from_slice(&[0x1f, 0x41, 0x1f, 0x41, 0, 11, 0, 0]); // ports, length 8 + 3
        p.extend_from_slice(&[7, 8, 9, 0xee]); // payload + trailing padding
        assert_eq!(udp_payload(&p), &[7, 8, 9]);
        p[9] = 6; // TCP
        assert!(udp_payload(&p).is_empty());
    }

    /// Runs the BPF program in a tiny interpreter over a sample packet.
    #[test]
    fn port_filter_accepts_only_the_ranges() {
        fn run(prog: &[libc::sock_filter], pkt: &[u8]) -> u32 {
            let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
            loop {
                let i = prog[pc];
                pc += 1;
                match i.code {
                    0x30 => a = pkt[i.k as usize] as u32,
                    0x28 => a = u16::from_be_bytes([pkt[i.k as usize], pkt[i.k as usize + 1]]) as u32,
                    0x48 => a = u16::from_be_bytes([pkt[(x + i.k) as usize], pkt[(x + i.k) as usize + 1]]) as u32,
                    0xb1 => x = 4 * (pkt[i.k as usize] & 0xf) as u32,
                    0x15 => pc += if a == i.k { i.jt } else { i.jf } as usize,
                    0x25 => pc += if a > i.k { i.jt } else { i.jf } as usize,
                    0x35 => pc += if a >= i.k { i.jt } else { i.jf } as usize,
                    0x45 => pc += if a & i.k != 0 { i.jt } else { i.jf } as usize,
                    0x06 => return i.k,
                    c => panic!("opcode {c:#x}"),
                }
            }
        }
        let prog = linux::port_filter(&[(8001, 8002), (9000, 9000)]);
        let pkt = |proto: u8, frag: u16, port: u16| {
            let mut p = vec![0x45, 0, 0, 0, 0, 0, (frag >> 8) as u8, frag as u8, 64, proto, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
            p.extend_from_slice(&[0, 1]);
            p.extend_from_slice(&port.to_be_bytes());
            p.extend_from_slice(&[0, 8, 0, 0]);
            p
        };
        for (port, want) in [(8000, false), (8001, true), (8002, true), (8003, false), (9000, true), (9001, false)] {
            assert_eq!(run(&prog, &pkt(17, 0, port)) != 0, want, "port {port}");
        }
        assert_eq!(run(&prog, &pkt(6, 0, 8001)), 0, "tcp");
        assert_eq!(run(&prog, &pkt(17, 0x0010, 8001)), 0, "fragment");
    }
}
