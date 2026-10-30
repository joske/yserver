//! The host access list (`xhost`): entries, server-interpreted (SI) address
//! validation and matching, and the `ListHosts` entry encoding.
//!
//! Pure: no locking and no I/O except name lookups through [`NameResolver`].
//! [`AuthState`](super::auth::AuthState) holds the state and decides
//! admission. Faithful to Xorg's `os/access.c` except where
//! docs/superpowers/plans/2026-10-30-xhost-access-control-plan.md,
//! "Decisions (review 2026-10-30)", says otherwise: a `#<num>` id is kept as
//! the full `long` instead of being truncated to `int`, so an oversized or
//! negative id is stored and listed but never matches anyone.

use std::{
    ffi::{CStr, CString},
    net::Ipv6Addr,
};

use yserver_protocol::x11::ClientByteOrder;

use crate::peer::PeerCred;

pub const FAMILY_INTERNET: u8 = 0;
pub const FAMILY_DECNET: u8 = 1;
pub const FAMILY_CHAOS: u8 = 2;
pub const FAMILY_SERVER_INTERPRETED: u8 = 5;
pub const FAMILY_INTERNET6: u8 = 6;
pub const FAMILY_LOCAL_HOST: u8 = 252;

/// `NI_MAXHOST`, Xorg's `SI_HOSTNAME_MAXLEN` (`os/access.c:1809`).
const SI_HOSTNAME_MAXLEN: usize = 1025;
/// `INET6_ADDRSTRLEN`, Xorg's `SI_IPv6_MAXLEN`.
const SI_IPV6_MAXLEN: usize = 46;
/// `GetHosts` stops counting once the entry bytes reach this
/// (`os/access.c:1460-1467`).
const LIST_HOSTS_BYTE_CAP: usize = 1 << 20;

/// One access-list entry, exactly as the client sent it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostEntry {
    pub family: u8,
    pub addr: Vec<u8>,
}

/// Xorg's `validhosts` plus `AccessEnabled`. Newest entry first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostList {
    pub access_enabled: bool,
    pub hosts: Vec<HostEntry>,
}

impl Default for HostList {
    /// The `ResetHosts` state without `-ac`: enabled, empty
    /// (`os/access.c:956-962`).
    fn default() -> Self {
        Self {
            access_enabled: true,
            hosts: Vec::new(),
        }
    }
}

impl HostList {
    /// `NewHost`: an equal entry already present is a no-op, otherwise the
    /// entry is prepended (`os/access.c:1371-1396`). Equality is family,
    /// length and bytes (`addrEqual`, `:193-196`).
    pub fn insert(&mut self, entry: HostEntry) {
        if !self.hosts.contains(&entry) {
            self.hosts.insert(0, entry);
        }
    }

    /// `RemoveHost`: unlink the first equal entry; an absent one is fine
    /// (`os/access.c:1439-1446`).
    pub fn delete(&mut self, entry: &HostEntry) {
        if let Some(index) = self.hosts.iter().position(|host| host == entry) {
            self.hosts.remove(index);
        }
    }
}

/// Which value a failed `ChangeHosts` reports in its `BadValue`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BadValue {
    /// `CheckAddr` failed for a known family: the value is the length.
    Length,
    /// An unknown family: the value is the family.
    Family,
}

/// `AddHost`/`RemoveHost` validation for every family except
/// `FamilyLocalHost`, which the caller handles (`os/access.c:1313-1349`,
/// `CheckAddr` at `:1495-1524`). DECnet and Chaos always fail.
pub fn check_addr(family: u8, addr: &[u8], resolver: &dyn NameResolver) -> Result<(), BadValue> {
    let valid = match family {
        FAMILY_INTERNET => addr.len() == 4,
        FAMILY_INTERNET6 => addr.len() == 16,
        FAMILY_DECNET | FAMILY_CHAOS => false,
        FAMILY_SERVER_INTERPRETED => {
            SiEntry::parse(addr).is_some_and(|entry| entry.value_is_valid(resolver))
        }
        _ => return Err(BadValue::Family),
    };
    if valid { Ok(()) } else { Err(BadValue::Length) }
}

/// The SI types Xorg registers (`siTypesInitialize`, `os/access.c:2160-2171`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SiType {
    Hostname,
    Ipv6,
    LocalUser,
    LocalGroup,
}

/// Whether a `localuser`/`localgroup` value names a user or a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredKind {
    User,
    Group,
}

/// A `FamilyServerInterpreted` address split into its type and value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SiEntry<'a> {
    pub kind: SiType,
    /// Everything after the first NUL; no trailing NUL on the wire.
    pub value: &'a [u8],
}

impl<'a> SiEntry<'a> {
    /// `type\0value`. `None` for a missing NUL or an unregistered type
    /// (`siCheckAddr`, `os/access.c:1745-1777`).
    #[must_use]
    pub fn parse(addr: &'a [u8]) -> Option<Self> {
        let nul = addr.iter().position(|&b| b == 0)?;
        let kind = match &addr[..nul] {
            b"hostname" => SiType::Hostname,
            b"ipv6" => SiType::Ipv6,
            b"localuser" => SiType::LocalUser,
            b"localgroup" => SiType::LocalGroup,
            _ => return None,
        };
        Some(Self {
            kind,
            value: &addr[nul + 1..],
        })
    }

    fn value_is_valid(&self, resolver: &dyn NameResolver) -> bool {
        match self.kind {
            SiType::Hostname => hostname_is_valid(self.value),
            SiType::Ipv6 => ipv6_is_valid(self.value),
            SiType::LocalUser => parse_cred_id(self.value, CredKind::User, resolver).is_some(),
            SiType::LocalGroup => parse_cred_id(self.value, CredKind::Group, resolver).is_some(),
        }
    }
}

/// `siHostnameCheckAddr` (`os/access.c:1901-1966`): RFC 2396 hostname
/// syntax, ASCII only. Xorg writes the digit range as `0x30..=0x3A`, so `:`
/// passes too.
fn hostname_is_valid(value: &[u8]) -> bool {
    if value.is_empty() || value.len() >= SI_HOSTNAME_MAXLEN {
        return false;
    }
    let mut dot_allowed = false;
    let mut dash_allowed = false;
    for &c in value {
        match c {
            b'.' => {
                if !dot_allowed {
                    return false;
                }
                dot_allowed = false;
                dash_allowed = false;
            }
            b'-' => {
                if !dash_allowed {
                    return false;
                }
                dot_allowed = false;
            }
            0x30..=0x3A | b'a'..=b'z' | b'A'..=b'Z' => {
                dot_allowed = true;
                dash_allowed = true;
            }
            _ => return false,
        }
    }
    true
}

/// `siIPv6CheckAddr` (`os/access.c:2013-2035`): `3 <= len <
/// INET6_ADDRSTRLEN`, then an IPv6 literal parse of the NUL-terminated copy.
fn ipv6_is_valid(value: &[u8]) -> bool {
    if value.len() < 3 || value.len() >= SI_IPV6_MAXLEN {
        return false;
    }
    std::str::from_utf8(before_nul(value)).is_ok_and(|text| text.parse::<Ipv6Addr>().is_ok())
}

fn before_nul(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|&b| b == 0)
        .map_or(bytes, |nul| &bytes[..nul])
}

/// `siLocalCredGetId` (`os/access.c:2051-2093`). Xorg copies the value and
/// NUL-terminates it, so everything below sees the bytes before the first
/// NUL. `#<num>` is `strtol` base 0 into the full `long`, with no
/// truncation to `int` (plan decision 1); anything else is a user or group
/// name lookup.
pub fn parse_cred_id(value: &[u8], kind: CredKind, resolver: &dyn NameResolver) -> Option<i64> {
    let value = before_nul(value);
    if let Some(number) = value.strip_prefix(b"#") {
        return strtol_base0(number);
    }
    let name = CString::new(value).ok()?;
    let id = match kind {
        CredKind::User => resolver.uid(&name),
        CredKind::Group => resolver.gid(&name),
    }?;
    Some(i64::from(id))
}

/// C `strtol(s, &end, 0)` on LP64, accepted when `end` moved and `errno`
/// stayed 0: leading whitespace, an optional sign, a `0x` (hex) or `0`
/// (octal) prefix, trailing junk ignored. `None` for no digits or `ERANGE`.
fn strtol_base0(s: &[u8]) -> Option<i64> {
    let mut i = 0;
    while s
        .get(i)
        .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    // `0x` only counts as a prefix when a hex digit follows; otherwise C
    // parses the `0` and stops at the `x`.
    let radix = if s.get(i) == Some(&b'0') {
        if matches!(s.get(i + 1), Some(b'x' | b'X'))
            && s.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            i += 2;
            16
        } else {
            8
        }
    } else {
        10
    };
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    let mut digits = 0usize;
    for &c in &s[i..] {
        let Some(digit) = char::from(c).to_digit(radix) else {
            break;
        };
        digits += 1;
        match magnitude
            .checked_mul(u64::from(radix))
            .and_then(|m| m.checked_add(u64::from(digit)))
        {
            Some(m) => magnitude = m,
            None => overflow = true,
        }
    }
    if digits == 0 || overflow {
        return None;
    }
    let signed = if negative {
        -i128::from(magnitude)
    } else {
        i128::from(magnitude)
    };
    i64::try_from(signed).ok()
}

/// `siLocalCredAddrMatch` for a local peer (`os/access.c:2096-2140`), plus
/// the `hostname`/`ipv6` matchers, which only accept Internet peers and so
/// never match here (`:1819-1846`, `:1970-1994`). `localuser` compares the
/// peer's effective uid, `localgroup` its primary gid; the full parsed value
/// is compared, so an id outside `0..=u32::MAX` never matches. No
/// credentials never match (`:2104`).
pub fn si_matches_local(
    entry: &HostEntry,
    cred: Option<&PeerCred>,
    resolver: &dyn NameResolver,
) -> bool {
    if entry.family != FAMILY_SERVER_INTERPRETED {
        return false;
    }
    let Some(si) = SiEntry::parse(&entry.addr) else {
        return false;
    };
    let Some(cred) = cred else {
        return false;
    };
    let (kind, id) = match si.kind {
        SiType::LocalUser => (CredKind::User, cred.uid),
        SiType::LocalGroup => (CredKind::Group, cred.gid),
        SiType::Hostname | SiType::Ipv6 => return false,
    };
    parse_cred_id(si.value, kind, resolver) == Some(i64::from(id))
}

/// `GetHosts` + `SLHostsExtend` (`os/access.c:1451-1491`,
/// `dix/swaprep.c:648-661`): the `ListHosts` entries, each `family, 0,
/// length:CARD16, addr`, padded to 4 with zeros. Entries are counted until
/// the running byte total reaches 1 MiB; the entry that reaches it is kept.
/// Returns the entry count and the bytes.
///
/// Deliberately not Xorg: Xorg counts entries in an `int` and stores the
/// count in the reply's CARD16 `nHosts`, so a list of more than 65535 short
/// entries reports a truncated count over the full data. We stop at 65535
/// entries, so the count and the data agree.
#[must_use]
pub fn encode_entries(hosts: &[HostEntry], byte_order: ClientByteOrder) -> (u16, Vec<u8>) {
    let mut total = 0usize;
    let mut count = 0u16;
    for host in hosts {
        if count == u16::MAX {
            break;
        }
        count += 1;
        total += 4 + host.addr.len().next_multiple_of(4);
        if total >= LIST_HOSTS_BYTE_CAP {
            break;
        }
    }
    let mut bytes = Vec::with_capacity(total);
    for host in &hosts[..usize::from(count)] {
        // ChangeHosts carries the length as a CARD16, so a stored address
        // always fits.
        let len = u16::try_from(host.addr.len()).unwrap_or(u16::MAX);
        bytes.push(host.family);
        bytes.push(0);
        bytes.extend_from_slice(&match byte_order {
            ClientByteOrder::LittleEndian => len.to_le_bytes(),
            ClientByteOrder::BigEndian => len.to_be_bytes(),
        });
        bytes.extend_from_slice(&host.addr);
        bytes.resize(
            bytes.len() + host.addr.len().next_multiple_of(4) - host.addr.len(),
            0,
        );
    }
    (count, bytes)
}

/// User and group name lookup for `localuser:<name>`/`localgroup:<name>`.
/// A trait so tests do not depend on the host's NSS.
pub trait NameResolver {
    fn uid(&self, name: &CStr) -> Option<u32>;
    fn gid(&self, name: &CStr) -> Option<u32>;
}

/// NSS through `getpwnam_r`/`getgrnam_r`: setup threads resolve
/// concurrently, so never the non-reentrant calls.
pub struct SystemResolver;

/// Upper bound for the `*_r` scratch buffer; a lookup that still reports
/// `ERANGE` at this size is treated as unresolved.
const NSS_BUF_MAX: usize = 1 << 20;

impl NameResolver for SystemResolver {
    fn uid(&self, name: &CStr) -> Option<u32> {
        let mut buf = vec![0 as libc::c_char; 1024];
        loop {
            // SAFETY: `passwd` is plain old data; `getpwnam_r` overwrites it.
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `name` is NUL-terminated, `buf` is writable for its
            // length, and `pwd`/`result` are valid out-params.
            let rc = unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    &raw mut pwd,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut result,
                )
            };
            if rc == libc::ERANGE && buf.len() < NSS_BUF_MAX {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            return (rc == 0 && !result.is_null()).then_some(pwd.pw_uid);
        }
    }

    fn gid(&self, name: &CStr) -> Option<u32> {
        let mut buf = vec![0 as libc::c_char; 1024];
        loop {
            // SAFETY: `group` is plain old data; `getgrnam_r` overwrites it.
            let mut grp: libc::group = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::group = std::ptr::null_mut();
            // SAFETY: as in `uid`.
            let rc = unsafe {
                libc::getgrnam_r(
                    name.as_ptr(),
                    &raw mut grp,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut result,
                )
            };
            if rc == libc::ERANGE && buf.len() < NSS_BUF_MAX {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            return (rc == 0 && !result.is_null()).then_some(grp.gr_gid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed NSS: users `root` 0 and `bob` 1000, groups `wheel` 10 and
    /// `staff` 50.
    struct FakeResolver;

    impl NameResolver for FakeResolver {
        fn uid(&self, name: &CStr) -> Option<u32> {
            match name.to_bytes() {
                b"root" => Some(0),
                b"bob" => Some(1000),
                _ => None,
            }
        }

        fn gid(&self, name: &CStr) -> Option<u32> {
            match name.to_bytes() {
                b"wheel" => Some(10),
                b"staff" => Some(50),
                _ => None,
            }
        }
    }

    fn entry(family: u8, addr: &[u8]) -> HostEntry {
        HostEntry {
            family,
            addr: addr.to_vec(),
        }
    }

    fn si(kind: &str, value: &[u8]) -> Vec<u8> {
        let mut addr = kind.as_bytes().to_vec();
        addr.push(0);
        addr.extend_from_slice(value);
        addr
    }

    fn cred(uid: u32, gid: u32) -> PeerCred {
        PeerCred {
            uid,
            gid,
            pid: None,
        }
    }

    #[test]
    fn insert_dedups_and_prepends() {
        let a = entry(FAMILY_INTERNET, &[10, 0, 0, 1]);
        let b = entry(FAMILY_INTERNET, &[10, 0, 0, 2]);
        let mut list = HostList::default();
        list.insert(a.clone());
        list.insert(b.clone());
        list.insert(a.clone());
        assert_eq!(list.hosts, vec![b, a]);
    }

    #[test]
    fn delete_of_an_absent_entry_changes_nothing() {
        let a = entry(FAMILY_INTERNET, &[10, 0, 0, 1]);
        let mut list = HostList::default();
        list.insert(a.clone());
        list.delete(&entry(FAMILY_INTERNET, &[10, 0, 0, 9]));
        assert_eq!(list.hosts, vec![a.clone()]);
        list.delete(&a);
        assert!(list.hosts.is_empty());
    }

    #[test]
    fn equality_includes_family_and_length() {
        let mut list = HostList::default();
        list.insert(entry(FAMILY_LOCAL_HOST, &[]));
        list.insert(entry(FAMILY_LOCAL_HOST, &[1]));
        list.insert(entry(FAMILY_INTERNET, &[]));
        assert_eq!(list.hosts.len(), 3);
    }

    #[test]
    fn default_list_is_enabled_and_empty() {
        let list = HostList::default();
        assert!(list.access_enabled);
        assert!(list.hosts.is_empty());
    }

    #[test]
    fn check_addr_lengths_and_families() {
        let r = &FakeResolver;
        assert_eq!(check_addr(FAMILY_INTERNET, &[0; 4], r), Ok(()));
        assert_eq!(
            check_addr(FAMILY_INTERNET, &[0; 3], r),
            Err(BadValue::Length)
        );
        assert_eq!(check_addr(FAMILY_INTERNET6, &[0; 16], r), Ok(()));
        assert_eq!(
            check_addr(FAMILY_INTERNET6, &[0; 15], r),
            Err(BadValue::Length)
        );
        for len in [0, 4, 16] {
            assert_eq!(
                check_addr(FAMILY_DECNET, &vec![0; len], r),
                Err(BadValue::Length)
            );
            assert_eq!(
                check_addr(FAMILY_CHAOS, &vec![0; len], r),
                Err(BadValue::Length)
            );
        }
        assert_eq!(check_addr(7, &[0; 4], r), Err(BadValue::Family));
    }

    #[test]
    fn check_addr_server_interpreted() {
        let r = &FakeResolver;
        assert_eq!(
            check_addr(FAMILY_SERVER_INTERPRETED, &si("localuser", b"bob"), r),
            Ok(())
        );
        assert_eq!(
            check_addr(FAMILY_SERVER_INTERPRETED, &si("foo", b"bar"), r),
            Err(BadValue::Length)
        );
        assert_eq!(
            check_addr(FAMILY_SERVER_INTERPRETED, b"localuserbob", r),
            Err(BadValue::Length)
        );
        assert_eq!(
            check_addr(
                FAMILY_SERVER_INTERPRETED,
                &si("localuser", b"nosuchuser"),
                r
            ),
            Err(BadValue::Length)
        );
        assert_eq!(
            check_addr(FAMILY_SERVER_INTERPRETED, &si("localgroup", b"staff"), r),
            Ok(())
        );
        assert_eq!(
            check_addr(FAMILY_SERVER_INTERPRETED, &si("localgroup", b"bob"), r),
            Err(BadValue::Length)
        );
    }

    #[test]
    fn parse_cred_id_follows_strtol_base_0() {
        let r = &FakeResolver;
        let id = |v: &[u8]| parse_cred_id(v, CredKind::User, r);
        assert_eq!(id(b"#1000"), Some(1000));
        assert_eq!(id(b"#0x3e8"), Some(1000));
        assert_eq!(id(b"#0X3E8"), Some(1000));
        assert_eq!(id(b"#01750"), Some(1000));
        assert_eq!(id(b"#1000x"), Some(1000));
        assert_eq!(id(b"# 1000"), Some(1000));
        assert_eq!(id(b"#+1000"), Some(1000));
        assert_eq!(id(b"#0"), Some(0));
        assert_eq!(id(b"#0x"), Some(0));
        assert_eq!(id(b"#"), None);
        assert_eq!(id(b"#x"), None);
        assert_eq!(id(b"#-"), None);
        assert_eq!(id(b"#99999999999999999999"), None);
        assert_eq!(id(b"#9223372036854775807"), Some(i64::MAX));
        assert_eq!(id(b"#9223372036854775808"), None);
        assert_eq!(id(b"#-9223372036854775808"), Some(i64::MIN));
        assert_eq!(id(b"#4294967296"), Some(4_294_967_296));
        assert_eq!(id(b"#-1"), Some(-1));
        assert_eq!(id(b"#10\x0099"), Some(10));
    }

    #[test]
    fn parse_cred_id_resolves_names_before_the_first_nul() {
        let r = &FakeResolver;
        assert_eq!(parse_cred_id(b"bob", CredKind::User, r), Some(1000));
        assert_eq!(parse_cred_id(b"alice", CredKind::User, r), None);
        assert_eq!(parse_cred_id(b"bob\0junk", CredKind::User, r), Some(1000));
        assert_eq!(parse_cred_id(b"staff", CredKind::Group, r), Some(50));
        assert_eq!(parse_cred_id(b"bob", CredKind::Group, r), None);
        assert_eq!(parse_cred_id(b"", CredKind::User, r), None);
    }

    #[test]
    fn oversized_and_negative_ids_validate_but_never_match() {
        let r = &FakeResolver;
        for kind in ["localuser", "localgroup"] {
            for value in [&b"#4294967296"[..], b"#-1"] {
                let addr = si(kind, value);
                assert_eq!(check_addr(FAMILY_SERVER_INTERPRETED, &addr, r), Ok(()));
                let e = entry(FAMILY_SERVER_INTERPRETED, &addr);
                for id in [0, u32::MAX] {
                    assert!(!si_matches_local(&e, Some(&cred(id, id)), r));
                }
            }
        }
    }

    #[test]
    fn hostname_validator() {
        assert!(hostname_is_valid(b"a.b-c"));
        assert!(hostname_is_valid(b"a:b"));
        assert!(hostname_is_valid(&[b'a'; 1024]));
        assert!(!hostname_is_valid(b".a"));
        assert!(!hostname_is_valid(b"-a"));
        assert!(!hostname_is_valid(b"a..b"));
        assert!(!hostname_is_valid(b"a_b"));
        assert!(!hostname_is_valid(b""));
        assert!(!hostname_is_valid(&[b'a'; 1025]));
    }

    #[test]
    fn ipv6_validator() {
        assert!(ipv6_is_valid(b"::1"));
        assert!(ipv6_is_valid(b"::1\0zz"));
        assert!(!ipv6_is_valid(b"::"));
        assert!(!ipv6_is_valid(b"fe80::1%lo"));
        assert!(!ipv6_is_valid(&[b'1'; 46]));
    }

    #[test]
    fn si_matches_local_compares_euid_and_primary_gid() {
        let r = &FakeResolver;
        let user = entry(FAMILY_SERVER_INTERPRETED, &si("localuser", b"#1000"));
        let named = entry(FAMILY_SERVER_INTERPRETED, &si("localuser", b"bob"));
        let group = entry(FAMILY_SERVER_INTERPRETED, &si("localgroup", b"staff"));
        assert!(si_matches_local(&user, Some(&cred(1000, 1)), r));
        assert!(si_matches_local(&named, Some(&cred(1000, 1)), r));
        assert!(!si_matches_local(&user, Some(&cred(1001, 1)), r));
        assert!(si_matches_local(&group, Some(&cred(1, 50)), r));
        // A supplementary group is never seen: `PeerCred` carries none.
        assert!(!si_matches_local(&group, Some(&cred(1, 10)), r));
        assert!(!si_matches_local(&user, None, r));
        assert!(!si_matches_local(&group, None, r));
        let host = entry(FAMILY_SERVER_INTERPRETED, &si("hostname", b"localhost"));
        let v6 = entry(FAMILY_SERVER_INTERPRETED, &si("ipv6", b"::1"));
        assert!(!si_matches_local(&host, Some(&cred(0, 0)), r));
        assert!(!si_matches_local(&v6, Some(&cred(0, 0)), r));
    }

    #[test]
    fn encode_one_si_entry_matches_measured_xorg() {
        let hosts = [entry(FAMILY_SERVER_INTERPRETED, b"localuser\0jos")];
        let (n, le) = encode_entries(&hosts, ClientByteOrder::LittleEndian);
        assert_eq!(n, 1);
        assert_eq!(
            le,
            [
                0x05, 0x00, 0x0d, 0x00, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x75, 0x73, 0x65, 0x72, 0x00,
                0x6a, 0x6f, 0x73, 0x00, 0x00, 0x00
            ]
        );
        assert_eq!(le.len() / 4, 5);
        let (_, be) = encode_entries(&hosts, ClientByteOrder::BigEndian);
        assert_eq!(&be[..4], [0x05, 0x00, 0x00, 0x0d]);
        assert_eq!(be[4..], le[4..]);
    }

    #[test]
    fn encode_keeps_the_entry_that_reaches_one_mib() {
        let hosts: Vec<HostEntry> = (0..17u8)
            .map(|i| entry(FAMILY_SERVER_INTERPRETED, &vec![i; 65532]))
            .collect();
        let (n, bytes) = encode_entries(&hosts[..16], ClientByteOrder::LittleEndian);
        assert_eq!((n, bytes.len()), (16, 1 << 20));
        let (n, bytes) = encode_entries(&hosts, ClientByteOrder::LittleEndian);
        assert_eq!((n, bytes.len()), (16, 1 << 20));
        // Newest first: the 17th (oldest) entry is the one dropped.
        assert_eq!(bytes[(1 << 20) - 1], 15);
    }

    #[test]
    fn encode_stops_at_65535_entries_so_the_count_fits_card16() {
        // 65536 empty entries are 256 KiB, far below the byte cap.
        let hosts = vec![entry(FAMILY_LOCAL_HOST, &[]); 65536];
        let (n, bytes) = encode_entries(&hosts, ClientByteOrder::LittleEndian);
        assert_eq!((n, bytes.len()), (u16::MAX, 4 * 65535));
        let (n, bytes) = encode_entries(&hosts[..65535], ClientByteOrder::LittleEndian);
        assert_eq!((n, bytes.len()), (u16::MAX, 4 * 65535));
    }
}
