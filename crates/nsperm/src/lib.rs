//! # nsperm
//!
//! The AOLserver permissions module (`nsperm/nsperm.c`): users with
//! `crypt(3)` passwords and optional host/network filters, groups, and
//! per-URL allow/deny rules enforced through the server's authorization
//! hook.  The bundled Tcl library (`init.tcl`) loads `passwd`, `group`,
//! `hosts.allow`, `hosts.deny` and `perms` from
//! `servers/S/modules/nsperm/` at startup via the `ns_perm` command.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, PoisonError, RwLock};

use ns_core::{UrlFlags, UrlSpace};
use ns_script::{command, Exception};
use nsd::op::{set_authorize_proc, AuthRequest};
use nsd::{ModuleContext, NsError, Status};

/// The module's Tcl library: `(path relative to home, contents)`.
pub const TCL_LIBRARY: &[(&str, &str)] = &[
    (
        "modules/tcl/nsperm/init.tcl",
        include_str!("../tcl/init.tcl"),
    ),
    (
        "modules/tcl/nsperm/compat.tcl",
        include_str!("../tcl/compat.tcl"),
    ),
];

/// Default data files installed into `servers/S/modules/nsperm/`.
pub const DATA_FILES: &[(&str, &str)] = &[
    ("passwd", include_str!("../tcl/passwd")),
    ("group", include_str!("../tcl/group")),
    ("perms", include_str!("../tcl/perms")),
    ("hosts.allow", include_str!("../tcl/hosts.allow")),
    ("hosts.deny", include_str!("../tcl/hosts.deny")),
];

/// A user record.
#[derive(Debug, Clone, Default)]
pub struct User {
    /// Encrypted password (empty = none required).
    pub pass: String,
    /// `true`: only listed hosts/nets allowed; `false`: listed ones denied.
    pub filter_allow: bool,
    /// Network filters `(net, mask)`.
    pub nets: Vec<(u32, u32)>,
    /// Host-name filters (domain suffixes).
    pub hosts: HashSet<String>,
    /// Groups the user belongs to.
    pub groups: HashSet<String>,
}

/// A URL permission record.
#[derive(Debug, Clone, Default)]
pub struct Perm {
    /// Normalized base URL.
    pub base_url: String,
    /// Allow access unless explicitly denied (set by deny rules).
    pub implicit_allow: bool,
    /// Users explicitly allowed.
    pub allow_user: HashSet<String>,
    /// Users explicitly denied.
    pub deny_user: HashSet<String>,
    /// Groups explicitly allowed.
    pub allow_group: HashSet<String>,
    /// Groups explicitly denied.
    pub deny_group: HashSet<String>,
}

/// Per-server permission state.
#[derive(Debug, Default)]
pub struct Perms {
    /// Users by name.
    pub users: HashMap<String, User>,
    /// Groups → member users.
    pub groups: HashMap<String, HashSet<String>>,
    /// URL permission records.
    pub perms: UrlSpace<Perm>,
}

fn parse_v4(s: &str) -> Option<u32> {
    s.parse::<Ipv4Addr>().ok().map(u32::from)
}

impl Perms {
    /// `ValidateUserAddr`.
    fn validate_addr(user: &User, peer: &str) -> bool {
        if peer.is_empty() {
            return true;
        }
        let Some(ip) = parse_v4(peer) else {
            return false;
        };
        for (net, mask) in &user.nets {
            if ip & mask == *net {
                return user.filter_allow;
            }
        }
        let mut ok = !user.filter_allow;
        if !user.hosts.is_empty() {
            if let Some(name) = reverse_lookup(peer) {
                let mut s = name.as_str();
                loop {
                    if user.hosts.contains(s) {
                        ok = user.filter_allow;
                        break;
                    }
                    match s[1..].find('.') {
                        Some(i) => s = &s[i + 1..],
                        None => break,
                    }
                }
            }
        }
        ok
    }

    /// Decide access for a request (`AuthProc`).
    pub fn authorize(
        &self,
        method: &str,
        url: &str,
        user: Option<&str>,
        pass: Option<&str>,
        peer: &str,
    ) -> Status {
        let user = user.unwrap_or("");
        let pass = pass.unwrap_or("");
        let Some(perm) = self.perms.get(method, url) else {
            return Status::Ok;
        };
        let Some(u) = self.users.get(user) else {
            return Status::Unauthorized;
        };
        if !u.pass.is_empty() && (pass.is_empty() || ns_core::crypt::crypt(pass, &u.pass) != u.pass)
        {
            return Status::Unauthorized;
        }
        let deny = if user.is_empty() {
            Status::Unauthorized
        } else {
            Status::Forbidden
        };
        if !Self::validate_addr(u, peer) || perm.deny_user.contains(user) {
            return deny;
        }
        if perm.deny_group.iter().any(|g| u.groups.contains(g)) {
            return deny;
        }
        if perm.allow_user.contains(user) || perm.allow_group.iter().any(|g| u.groups.contains(g)) {
            return Status::Ok;
        }
        if perm.implicit_allow {
            Status::Ok
        } else {
            Status::Unauthorized
        }
    }

    /// `ns_perm adduser name encpass userfield ?-allow|-deny host ...?`.
    pub fn add_user(
        &mut self,
        name: &str,
        encpass: &str,
        filters: Option<(bool, &[String])>,
    ) -> Result<(), String> {
        if self.users.contains_key(name) {
            return Err(format!("duplicate user: {name}"));
        }
        let mut u = User {
            pass: encpass.to_owned(),
            ..User::default()
        };
        if let Some((allow, list)) = filters {
            u.filter_allow = allow;
            for f in list {
                if let Some((net, mask)) = f.split_once('/') {
                    let (Some(n), Some(m)) = (parse_v4(net), parse_v4(mask)) else {
                        return Err(format!("invalid address or hostname \"{f}\". should be ipaddr/netmask or hostname"));
                    };
                    u.nets.push((n & m, m));
                } else {
                    u.hosts.insert(f.clone());
                }
            }
        }
        self.users.insert(name.to_owned(), u);
        Ok(())
    }

    /// `ns_perm addgroup name user ?user ...?`.
    pub fn add_group(&mut self, name: &str, users: &[String]) -> Result<(), String> {
        if self.groups.contains_key(name) {
            return Err(format!("duplicate group: {name}"));
        }
        let mut members = HashSet::new();
        for u in users {
            if !self.users.contains_key(u) {
                return Err(format!("no such user: {u}"));
            }
            if !members.insert(u.clone()) {
                return Err(format!("user \"{u}\" already in group \"{name}\""));
            }
        }
        for u in &members {
            if let Some(rec) = self.users.get_mut(u) {
                rec.groups.insert(name.to_owned());
            }
        }
        self.groups.insert(name.to_owned(), members);
        Ok(())
    }

    /// `ns_perm allowuser|denyuser|allowgroup|denygroup ?-noinherit? method url key`.
    pub fn allow_deny(
        &mut self,
        method: &str,
        url: &str,
        key: &str,
        allow: bool,
        user: bool,
        noinherit: bool,
    ) {
        let base = ns_core::pathname::normalize_path(url);
        let flags = UrlFlags {
            noinherit,
            recurse: false,
        };
        // Reuse the record registered for exactly this URL, if any.
        let mut rec = self
            .perms
            .get(method, url)
            .filter(|p| p.base_url == base)
            .cloned()
            .unwrap_or_else(|| Perm {
                base_url: base,
                ..Perm::default()
            });
        rec.implicit_allow = !allow;
        let set = match (allow, user) {
            (true, true) => &mut rec.allow_user,
            (false, true) => &mut rec.deny_user,
            (true, false) => &mut rec.allow_group,
            (false, false) => &mut rec.deny_group,
        };
        set.insert(key.to_owned());
        self.perms.set(method, url, rec, flags);
    }

    /// Check a user's password (extension used by `compat.tcl`).
    pub fn check_pass(&self, user: &str, pass: &str) -> bool {
        self.users
            .get(user)
            .is_some_and(|u| u.pass.is_empty() || ns_core::crypt::crypt(pass, &u.pass) == u.pass)
    }
}

fn reverse_lookup(peer: &str) -> Option<String> {
    ns_core::dns::host_by_addr(peer)
}

/// Module init: install the authorization proc and `ns_perm`.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nsperm must be loaded as a per-server module".into()))?;
    let perms = Arc::new(RwLock::new(Perms::default()));
    let p = Arc::clone(&perms);
    set_authorize_proc(
        server,
        Some(Arc::new(move |r: &AuthRequest| {
            p.read().unwrap_or_else(PoisonError::into_inner).authorize(
                &r.method,
                &r.url,
                r.user.as_deref(),
                r.passwd.as_deref(),
                &r.peer,
            )
        })),
    );
    let p = Arc::clone(&perms);
    server.add_command(
        "ns_perm",
        command(move |_, argv| {
            let sub = argv.get(1).map(String::as_str).ok_or_else(|| {
                Exception::error(format!("wrong # args: should be \"{} command ?args ...?\"", argv[0]))
            })?;
            let mut st = p.write().unwrap_or_else(PoisonError::into_inner);
            match sub {
                "adduser" => {
                    if argv.len() < 5 || argv.len() == 6 {
                        return Err(Exception::error(format!(
                            "wrong # args: should be \"{} {sub} name encpass userfield ?-allow|-deny host ...?\"",
                            argv[0]
                        )));
                    }
                    let filters = if argv.len() > 6 {
                        let allow = match argv[5].as_str() {
                            "-allow" => true,
                            "-deny" => false,
                            o => return Err(Exception::error(format!("invalid switch \"{o}\". Should be -allow or -deny"))),
                        };
                        Some((allow, &argv[6..]))
                    } else {
                        None
                    };
                    st.add_user(&argv[2], &argv[3], filters).map_err(Exception::error)?;
                    Ok(String::new())
                }
                "addgroup" => {
                    if argv.len() < 4 {
                        return Err(Exception::error(format!(
                            "wrong # args: should be \"{} {sub} name user ?user ...?\"",
                            argv[0]
                        )));
                    }
                    st.add_group(&argv[2], &argv[3..]).map_err(Exception::error)?;
                    Ok(String::new())
                }
                "allowuser" | "denyuser" | "allowgroup" | "denygroup" => {
                    let (noinherit, a) = match argv.get(2).map(String::as_str) {
                        Some("-noinherit") => (true, &argv[3..]),
                        _ => (false, &argv[2..]),
                    };
                    if a.len() != 3 {
                        return Err(Exception::error(format!(
                            "wrong # args: should be \"{} {sub} ?-noinherit? method url key\"",
                            argv[0]
                        )));
                    }
                    st.allow_deny(&a[0], &a[1], &a[2], sub.starts_with("allow"), sub.ends_with("user"), noinherit);
                    Ok(String::new())
                }
                "checkpass" => {
                    if argv.len() != 4 {
                        return Err(Exception::error(format!("wrong # args: should be \"{} checkpass user pass\"", argv[0])));
                    }
                    if st.check_pass(&argv[2], &argv[3]) {
                        Ok(String::new())
                    } else {
                        Err(Exception::error("incorrect password"))
                    }
                }
                "setpass" => {
                    if argv.len() != 4 {
                        return Err(Exception::error(format!("wrong # args: should be \"{} setpass user encpass\"", argv[0])));
                    }
                    let u = st
                        .users
                        .get_mut(&argv[2])
                        .ok_or_else(|| Exception::error(format!("no such user: {}", argv[2])))?;
                    u.pass.clone_from(&argv[3]);
                    Ok(String::new())
                }
                "listusers" => {
                    let mut v: Vec<String> = st.users.keys().cloned().collect();
                    v.sort();
                    Ok(v.join(" "))
                }
                o => Err(Exception::error(format!(
                    "unknown command \"{o}\": should be adduser, addgroup, allowuser, denyuser, allowgroup, or denygroup"
                ))),
            }
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn setup() -> Perms {
        let mut p = Perms::default();
        p.add_user("alice", &ns_core::crypt::crypt("secret", "ab"), None)
            .expect("user");
        p.add_user("bob", "", None).expect("user");
        p.add_user("lan", "", Some((true, &sv(&["10.0.0.0/255.0.0.0"]))))
            .expect("user");
        p.add_user("", "", None).expect("anon");
        p.add_group("admins", &sv(&["alice"])).expect("group");
        p
    }

    #[test]
    fn unprotected_urls_are_open() {
        let p = setup();
        assert_eq!(p.authorize("GET", "/", None, None, "1.2.3.4"), Status::Ok);
    }

    #[test]
    fn allow_rules() {
        let mut p = setup();
        p.allow_deny("GET", "/admin", "admins", true, false, false);
        assert_eq!(
            p.authorize("GET", "/admin/x", Some("alice"), Some("secret"), "1.2.3.4"),
            Status::Ok
        );
        assert_eq!(
            p.authorize("GET", "/admin/x", Some("alice"), Some("wrong"), "1.2.3.4"),
            Status::Unauthorized
        );
        assert_eq!(
            p.authorize("GET", "/admin/x", Some("alice"), None, "1.2.3.4"),
            Status::Unauthorized
        );
        assert_eq!(
            p.authorize("GET", "/admin", Some("bob"), None, "1.2.3.4"),
            Status::Unauthorized
        );
        assert_eq!(
            p.authorize("GET", "/admin", Some("nobody"), None, "1.2.3.4"),
            Status::Unauthorized
        );
        p.allow_deny("GET", "/admin", "bob", true, true, false);
        assert_eq!(
            p.authorize("GET", "/admin", Some("bob"), None, "1.2.3.4"),
            Status::Ok
        );
    }

    #[test]
    fn deny_rules_and_networks() {
        let mut p = setup();
        p.allow_deny("GET", "/pub", "bob", false, true, false);
        assert_eq!(
            p.authorize("GET", "/pub", Some("bob"), None, "1.2.3.4"),
            Status::Forbidden
        );
        assert_eq!(
            p.authorize("GET", "/pub", Some("alice"), Some("secret"), "1.2.3.4"),
            Status::Ok,
            "implicit allow"
        );
        assert_eq!(
            p.authorize("GET", "/pub", Some(""), None, "1.2.3.4"),
            Status::Ok
        );
        p.allow_deny("GET", "/lan", "lan", true, true, false);
        assert_eq!(
            p.authorize("GET", "/lan", Some("lan"), None, "10.1.2.3"),
            Status::Ok
        );
        assert_eq!(
            p.authorize("GET", "/lan", Some("lan"), None, "192.168.0.1"),
            Status::Forbidden
        );
        assert_eq!(
            p.authorize("GET", "/lan", Some("lan"), None, "bogus"),
            Status::Forbidden
        );
        p.allow_deny("GET", "/g", "admins", false, false, true);
        assert_eq!(
            p.authorize("GET", "/g", Some("alice"), Some("secret"), ""),
            Status::Forbidden
        );
    }

    #[test]
    fn validation_errors() {
        let mut p = setup();
        assert!(p.add_user("alice", "", None).is_err());
        assert!(p
            .add_user("x", "", Some((true, &sv(&["bad/net"]))))
            .is_err());
        assert!(p.add_group("admins", &sv(&["alice"])).is_err());
        assert!(p.add_group("g2", &sv(&["ghost"])).is_err());
        assert!(p.add_group("g3", &sv(&["bob", "bob"])).is_err());
        assert!(p.check_pass("alice", "secret"));
        assert!(!p.check_pass("alice", "nope"));
        assert!(p.check_pass("bob", "anything"));
        assert!(!p.check_pass("ghost", ""));
        assert!(reverse_lookup("not-an-ip").is_none());
    }
}
