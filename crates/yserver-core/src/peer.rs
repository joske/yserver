//! Peer identity of a client connection, and the server's own addresses.
//!
//! Capture only: no policy lives here. See
//! docs/superpowers/specs/2026-10-30-xhost-access-control-design.md, "Peer
//! identity, captured once".

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(any(
    target_os = "linux",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "freebsd"
))]
use std::os::fd::AsRawFd;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
use std::os::fd::RawFd;

use crate::{
    core_loop::{
        auth::AuthTransport,
        host_access::{FAMILY_INTERNET, FAMILY_INTERNET6, HostEntry},
    },
    transport::Transport,
};

/// Credentials of a Unix-socket peer, as the kernel reports them at
/// `connect()` time: effective uid and primary gid, never supplementary
/// groups (Xorg's `GetLocalClientCreds` on its `SO_PEERCRED` and
/// `getpeereid` paths, `os/access.c:1246-1274`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    /// `None` where the platform cannot report it (FreeBSD).
    pub pid: Option<u32>,
}

/// Who is on the other end of a connection, read once from the socket at
/// setup and never from anything the client sends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    pub transport: AuthTransport,
    /// The TCP peer address; `None` for Unix.
    pub addr: Option<IpAddr>,
    /// Unix only; `None` when the lookup failed, which matches no
    /// credential-based rule.
    pub cred: Option<PeerCred>,
}

impl PeerIdentity {
    /// Read the identity of `stream`'s peer.
    #[must_use]
    pub fn of(stream: &Transport) -> Self {
        match stream {
            Transport::Unix(stream) => Self {
                transport: AuthTransport::Unix,
                addr: None,
                cred: unix_peer_cred(stream),
            },
            Transport::Tcp(stream) => Self {
                transport: AuthTransport::Tcp,
                addr: stream.peer_addr().ok().map(|peer| peer.ip()),
                cred: None,
            },
            #[cfg(test)]
            Transport::Capture(_) => Self::unknown_unix(),
        }
    }

    /// A Unix peer whose credentials are unknown.
    #[must_use]
    pub fn unknown_unix() -> Self {
        Self {
            transport: AuthTransport::Unix,
            addr: None,
            cred: None,
        }
    }
}

/// `SO_PEERCRED`: `ucred` on Linux, `sockpeercred` on OpenBSD, pid included.
#[cfg(any(target_os = "linux", target_os = "openbsd"))]
fn unix_peer_cred(stream: &impl AsRawFd) -> Option<PeerCred> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    #[cfg(target_os = "openbsd")]
    let mut cred = libc::sockpeercred {
        uid: 0,
        gid: 0,
        pid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of_val(&cred)).ok()?;
    // SAFETY: `fd` is a live socket for the call, and `cred`/`len` are the
    // out-param types `SO_PEERCRED` documents on this platform.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then(|| PeerCred {
        uid: cred.uid,
        gid: cred.gid,
        pid: u32::try_from(cred.pid).ok().filter(|&pid| pid > 0),
    })
}

/// `getpeereid`, Xorg's `HAVE_GETPEEREID` path. Not `LOCAL_PEERCRED`: its
/// supplementary groups would make `localgroup` broader than Xorg's.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn unix_peer_cred(stream: &impl AsRawFd) -> Option<PeerCred> {
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a live socket; `uid`/`gid` are valid out-params.
    if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } != 0 {
        return None;
    }
    Some(PeerCred {
        uid,
        gid,
        pid: peer_pid(fd),
    })
}

/// `LOCAL_PEERPID`, as Xorg reads it (`os/access.c:1265-1272`).
#[cfg(target_os = "macos")]
fn peer_pid(fd: RawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>()).ok()?;
    // SAFETY: `fd` is a live socket; `pid`/`len` match `LOCAL_PEERPID`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            std::ptr::from_mut(&mut pid).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    u32::try_from(pid).ok().filter(|&pid| pid > 0)
}

/// FreeBSD's `getpeereid` path has no pid.
#[cfg(target_os = "freebsd")]
fn peer_pid(_fd: RawFd) -> Option<u32> {
    None
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "freebsd"
)))]
fn unix_peer_cred(_stream: &std::os::unix::net::UnixStream) -> Option<PeerCred> {
    None
}

/// The access-list entry for one of our addresses: `{FamilyInternet, 4
/// bytes}` or `{FamilyInternet6, 16 bytes}`, in network order, as Xorg's
/// `DefineSelf` stores interface addresses (`os/access.c:793-818`).
#[must_use]
pub fn host_entry_of(addr: &SocketAddr) -> HostEntry {
    match addr.ip() {
        IpAddr::V4(v4) => HostEntry {
            family: FAMILY_INTERNET,
            addr: v4.octets().to_vec(),
        },
        IpAddr::V6(v6) => HostEntry {
            family: FAMILY_INTERNET6,
            addr: v6.octets().to_vec(),
        },
    }
}

/// Xorg's `selfhosts` interface part: every `getifaddrs` address of both
/// families, loopback included.
#[must_use]
pub fn own_interface_addrs() -> Vec<HostEntry> {
    interface_ips()
        .into_iter()
        .map(|ip| host_entry_of(&SocketAddr::new(ip, 0)))
        .collect()
}

/// IPv4 and IPv6 addresses of every interface. Empty if `getifaddrs` fails.
fn interface_ips() -> Vec<IpAddr> {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `ifap` with an owned list on success; we
    // walk it without retaining anything and free it before returning.
    if unsafe { libc::getifaddrs(&raw mut ifap) } != 0 {
        return Vec::new();
    }
    let mut ips = Vec::new();
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list `getifaddrs` just built, and
        // `ifa_addr` is either null or a valid `sockaddr` for its family.
        let addr = unsafe { (*cur).ifa_addr };
        if !addr.is_null() {
            // SAFETY: as above.
            let family = libc::c_int::from(unsafe { (*addr).sa_family });
            if family == libc::AF_INET {
                // SAFETY: family said AF_INET, so the node is a sockaddr_in.
                let raw = unsafe { (*addr.cast::<libc::sockaddr_in>()).sin_addr.s_addr };
                ips.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(raw))));
            } else if family == libc::AF_INET6 {
                // SAFETY: family said AF_INET6, so the node is a sockaddr_in6.
                let raw = unsafe { (*addr.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr };
                ips.push(IpAddr::V6(Ipv6Addr::from(raw)));
            }
        }
        // SAFETY: as above; `ifa_next` is null at the end of the list.
        cur = unsafe { (*cur).ifa_next };
    }
    // SAFETY: `ifap` is exactly what `getifaddrs` returned and is freed once.
    unsafe { libc::freeifaddrs(ifap) };
    ips
}

/// Is `peer` one of this machine's own addresses?
///
/// Xorg's `xtransLocalClient` (`os/access.c`) treats an AF_UNIX peer as
/// local, and otherwise compares the peer against `selfhosts` — the
/// addresses `DefineSelf` collected from the interfaces. So a TCP
/// connection from the machine's own address is a LOCAL client there, and
/// keeps the locality-gated extensions.
///
/// Queried per accept rather than snapshotted at startup: accepts are
/// rare, `getifaddrs` is cheap, and a cached set goes stale across a
/// hotplug or a DHCP renewal. Xorg snapshots and then patches with
/// `AugmentSelf`; asking each time is simpler and cannot drift.
///
/// Any loopback address counts; otherwise only IPv4 interface addresses
/// are compared, since the TCP listener is IPv4-only.
///
/// Not implemented: Xorg additionally treats a client whose command name
/// is `ssh` as non-local, to catch a forwarded connection. That is a
/// heuristic on `/proc`, and `ssh -X` reaches us over a UNIX socket
/// anyway.
pub(crate) fn address_is_ours(peer: IpAddr) -> bool {
    peer.is_loopback() || (peer.is_ipv4() && interface_ips().contains(&peer))
}

/// A Unix connection made through a bound listener and `accept`, the
/// production path, rather than `socketpair`, whose peer credentials are not
/// portable to every BSD. Returns `(accepted, client)`.
#[cfg(test)]
pub(crate) fn connected_unix_pair() -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        sync::atomic::{AtomicU32, Ordering},
    };
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "yserver-peer-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let client = UnixStream::connect(&path).expect("connect");
    let (accepted, _) = listener.accept().expect("accept");
    let _ = std::fs::remove_dir_all(&dir);
    (accepted, client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddrV4, SocketAddrV6, TcpListener, TcpStream};

    #[test]
    fn host_entry_of_maps_both_families_in_network_order() {
        let v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 6000));
        assert_eq!(
            host_entry_of(&v4),
            HostEntry {
                family: FAMILY_INTERNET,
                addr: vec![192, 0, 2, 7],
            }
        );
        let ip6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let v6 = SocketAddr::V6(SocketAddrV6::new(ip6, 6000, 0, 0));
        assert_eq!(
            host_entry_of(&v6),
            HostEntry {
                family: FAMILY_INTERNET6,
                addr: vec![0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            }
        );
    }

    #[test]
    fn unix_peer_reports_our_euid_egid_and_pid() {
        let (accepted, _client) = connected_unix_pair();
        let peer = PeerIdentity::of(&Transport::Unix(accepted));

        assert_eq!(peer.transport, AuthTransport::Unix);
        assert_eq!(peer.addr, None);
        let cred = peer.cred.expect("peer credentials");
        // SAFETY: plain getters with no preconditions.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!((cred.uid, cred.gid), (euid, egid));
        let expected_pid = if cfg!(target_os = "freebsd") {
            None
        } else {
            Some(std::process::id())
        };
        assert_eq!(cred.pid, expected_pid);
    }

    #[test]
    fn tcp_peer_has_an_address_and_no_credentials() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let _client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (accepted, _) = listener.accept().expect("accept");
        let peer = PeerIdentity::of(&Transport::Tcp(accepted));
        assert_eq!(peer.transport, AuthTransport::Tcp);
        assert_eq!(peer.addr, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(peer.cred, None);
    }
}
