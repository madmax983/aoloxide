//! Name resolution (`nsd/dns.c`): `Ns_GetHostByAddr`, `Ns_GetAddrByHost`.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// Reverse-resolve an address to a host name (`Ns_GetHostByAddr`).
/// Returns `None` if the address is invalid or has no PTR record.
pub fn host_by_addr(addr: &str) -> Option<String> {
    let ip: IpAddr = addr.parse().ok()?;
    let sa = SocketAddr::new(ip, 0);
    // SAFETY: an all-zero sockaddr_storage is a valid initial value.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match sa {
        SocketAddr::V4(v4) => {
            // SAFETY: sockaddr_in is POD; zeroed is valid.
            let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            s.sin_family = libc::AF_INET as libc::sa_family_t;
            s.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            // SAFETY: sockaddr_in fits inside sockaddr_storage.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const s).cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_in>(),
                );
            }
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            // SAFETY: sockaddr_in6 is POD; zeroed is valid.
            let mut s: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            s.sin6_addr.s6_addr = v6.ip().octets();
            // SAFETY: sockaddr_in6 fits inside sockaddr_storage.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const s).cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_in6>(),
                );
            }
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    let mut host = [0 as libc::c_char; 1025];
    // SAFETY: storage/len describe a valid sockaddr; host is writable for
    // its length.
    let rc = unsafe {
        libc::getnameinfo(
            (&raw const storage).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: getnameinfo NUL-terminates `host` on success.
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// Resolve a host name to its IPv4 addresses (`Ns_GetAllAddrByHost`).
pub fn addrs_by_host(host: &str) -> Vec<String> {
    (host, 0)
        .to_socket_addrs()
        .map(|it| {
            it.filter(SocketAddr::is_ipv4)
                .map(|a| a.ip().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert!(addrs_by_host("localhost").iter().any(|a| a == "127.0.0.1"));
        assert_eq!(
            addrs_by_host("no-such-host.invalid"),
            [] as [std::string::String; 0]
        );
        assert!(host_by_addr("not an address").is_none());
        // 127.0.0.1 normally reverse-resolves via /etc/hosts; don't require it.
        let _ = host_by_addr("127.0.0.1");
        let _ = host_by_addr("::1");
    }
}
