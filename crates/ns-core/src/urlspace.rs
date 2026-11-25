//! URL-specific data (`Ns_UrlSpecific*`), ported from `nsd/urlspace.c`.
//!
//! A [`UrlSpace`] maps `(method, url)` registrations to data, with the same
//! resolution rules as AOLserver:
//!
//! * A URL is split into path segments (empty segments are ignored).
//! * If the last segment of a registered URL contains a glob character
//!   (`*` or `?`), that segment becomes a *channel filter* and is removed from
//!   the stored path; otherwise the registration goes in the implicit `*`
//!   channel.  E.g. `GET /foo/*.html` is stored under path `GET/foo` in the
//!   `*.html` channel.
//! * Channels are ordered from most to least specific filter, so `*.html`
//!   is consulted before `*`.
//! * A lookup tries each channel whose filter matches the URL's last segment
//!   and walks the channel's trie as deep as the URL allows.  The deepest
//!   registration found wins; *inheriting* registrations apply to every URL
//!   beneath them, *non-inheriting* ones (`NS_OP_NOINHERIT`) only to the
//!   exact URL.  The first channel that yields data wins.
//!
//! In C the urlspace is one global trie keyed additionally by server and an
//! allocated id; in Rust each server keeps a separate typed `UrlSpace<T>`
//! per purpose, which is equivalent and type-safe.

use std::collections::BTreeMap;

use crate::glob::string_match;

/// Registration flags (`NS_OP_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UrlFlags {
    /// Data applies only to the exact URL, not to URLs beneath it.
    pub noinherit: bool,
    /// For removal: remove this URL *and everything beneath it*.
    pub recurse: bool,
}

impl UrlFlags {
    /// Inheriting registration (the default).
    pub const INHERIT: Self = Self {
        noinherit: false,
        recurse: false,
    };
    /// Non-inheriting registration.
    pub const NOINHERIT: Self = Self {
        noinherit: true,
        recurse: false,
    };
}

#[derive(Debug, Clone)]
struct Node<T> {
    inherit: Option<T>,
    noinherit: Option<T>,
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Self {
            inherit: None,
            noinherit: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Trie<T> {
    branches: BTreeMap<String, Self>,
    node: Option<Node<T>>,
}

impl<T> Default for Trie<T> {
    fn default() -> Self {
        Self {
            branches: BTreeMap::new(),
            node: None,
        }
    }
}

impl<T> Trie<T> {
    fn add(&mut self, seq: &[String], data: T, noinherit: bool) -> Option<T> {
        match seq.split_first() {
            None => {
                let node = self.node.get_or_insert_with(Node::default);
                let slot = if noinherit {
                    &mut node.noinherit
                } else {
                    &mut node.inherit
                };
                slot.replace(data)
            }
            Some((word, rest)) => self
                .branches
                .entry(word.clone())
                .or_default()
                .add(rest, data, noinherit),
        }
    }

    /// `TrieFind`: deepest non-null match along `seq`.
    fn find(&self, seq: &[String]) -> Option<&T> {
        let mut data = self.node.as_ref().and_then(|node| {
            if seq.is_empty() && node.noinherit.is_some() {
                node.noinherit.as_ref()
            } else {
                node.inherit.as_ref()
            }
        });
        if let Some((word, rest)) = seq.split_first() {
            if let Some(branch) = self.branches.get(word) {
                if let Some(d) = branch.find(rest) {
                    data = Some(d);
                }
            }
        }
        data
    }

    fn walk(&self, seq: &[String]) -> Option<&Self> {
        match seq.split_first() {
            None => Some(self),
            Some((w, rest)) => self.branches.get(w)?.walk(rest),
        }
    }

    fn walk_mut(&mut self, seq: &[String]) -> Option<&mut Self> {
        match seq.split_first() {
            None => Some(self),
            Some((w, rest)) => self.branches.get_mut(w)?.walk_mut(rest),
        }
    }

    fn find_exact(&self, seq: &[String], noinherit: bool) -> Option<&T> {
        let node = self.walk(seq)?.node.as_ref()?;
        if noinherit {
            node.noinherit.as_ref()
        } else {
            node.inherit.as_ref()
        }
    }

    fn delete(&mut self, seq: &[String], noinherit: bool) -> Option<T> {
        let node = self.walk_mut(seq)?.node.as_mut()?;
        if noinherit {
            node.noinherit.take()
        } else {
            node.inherit.take()
        }
    }

    /// `TrieTrunc`: drop every node at and beneath this trie.
    fn trunc(&mut self) {
        self.node = None;
        for b in self.branches.values_mut() {
            b.trunc();
        }
        self.prune();
    }

    /// Remove empty branches so the trie does not grow without bound.
    fn prune(&mut self) -> bool {
        self.branches.retain(|_, b| !b.prune());
        let node_empty = self
            .node
            .as_ref()
            .is_none_or(|n| n.inherit.is_none() && n.noinherit.is_none());
        if node_empty {
            self.node = None;
        }
        node_empty && self.branches.is_empty()
    }

    fn collect<'a>(&'a self, path: &mut Vec<&'a str>, out: &mut Vec<(Vec<&'a str>, bool, &'a T)>) {
        if let Some(node) = &self.node {
            if let Some(d) = &node.inherit {
                out.push((path.clone(), false, d));
            }
            if let Some(d) = &node.noinherit {
                out.push((path.clone(), true, d));
            }
        }
        for (w, b) in &self.branches {
            path.push(w);
            b.collect(path, out);
            path.pop();
        }
    }
}

#[derive(Debug, Clone)]
struct Channel<T> {
    filter: String,
    trie: Trie<T>,
}

/// One registration returned by [`UrlSpace::entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlEntry<'a, T> {
    /// HTTP method (or other key) the data is registered for.
    pub method: &'a str,
    /// Registered URL, reconstructed from the trie path and filter.
    pub url: String,
    /// Whether the registration is non-inheriting.
    pub noinherit: bool,
    /// The registered data.
    pub data: &'a T,
}

/// URL-specific data store.  See the module documentation for semantics.
#[derive(Debug, Clone)]
pub struct UrlSpace<T> {
    channels: Vec<Channel<T>>,
}

impl<T> Default for UrlSpace<T> {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
        }
    }
}

/// Split a URL into its non-empty path segments (`MkSeq`).
pub fn url_segments(url: &str) -> Vec<String> {
    url.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Compute `(trie_sequence, channel_filter)` for a registration.
fn registration_key(method: &str, url: &str) -> (Vec<String>, String) {
    let mut segs = url_segments(url);
    let filter = match segs.last() {
        Some(last) if last.contains(['*', '?']) => segs.pop().unwrap_or_default(),
        _ => "*".to_owned(),
    };
    let mut seq = Vec::with_capacity(segs.len() + 1);
    seq.push(method.to_owned());
    seq.extend(segs);
    (seq, filter)
}

impl<T> UrlSpace<T> {
    /// Create an empty urlspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no channel holds any data.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    fn channel_index(&self, filter: &str) -> Option<usize> {
        self.channels.iter().position(|c| c.filter == filter)
    }

    /// Insert a channel keeping the most-specific-first order (`byuse`).
    fn insert_channel(&mut self, filter: String) -> usize {
        let pos = self
            .channels
            .iter()
            .position(|c| string_match(&filter, &c.filter) && !string_match(&c.filter, &filter))
            .unwrap_or(self.channels.len());
        self.channels.insert(
            pos,
            Channel {
                filter,
                trie: Trie::default(),
            },
        );
        pos
    }

    /// Register `data` for `method url` (`Ns_UrlSpecificSet`).  Returns the
    /// data previously registered under the same key and inheritance mode.
    pub fn set(&mut self, method: &str, url: &str, data: T, flags: UrlFlags) -> Option<T> {
        let (seq, filter) = registration_key(method, url);
        let idx = match self.channel_index(&filter) {
            Some(i) => i,
            None => self.insert_channel(filter),
        };
        self.channels[idx].trie.add(&seq, data, flags.noinherit)
    }

    /// Resolve the data that applies to `method url` (`Ns_UrlSpecificGet`).
    pub fn get(&self, method: &str, url: &str) -> Option<&T> {
        let segs = url_segments(url);
        let last = segs.last().map_or("", String::as_str);
        let mut seq = Vec::with_capacity(segs.len() + 1);
        seq.push(method.to_owned());
        seq.extend(segs.iter().cloned());
        self.channels
            .iter()
            .filter(|c| string_match(last, &c.filter))
            .find_map(|c| c.trie.find(&seq))
    }

    /// Data registered for exactly this key (`Ns_UrlSpecificGetExact`).
    pub fn get_exact(&self, method: &str, url: &str, flags: UrlFlags) -> Option<&T> {
        let (seq, filter) = registration_key(method, url);
        let idx = self.channel_index(&filter)?;
        self.channels[idx].trie.find_exact(&seq, flags.noinherit)
    }

    /// Remove a registration (`Ns_UrlSpecificDestroy`).  With
    /// `flags.recurse`, everything at or beneath `url` is removed from every
    /// channel and `None` is returned.
    pub fn remove(&mut self, method: &str, url: &str, flags: UrlFlags) -> Option<T> {
        if flags.recurse {
            let mut seq = vec![method.to_owned()];
            seq.extend(url_segments(url));
            for c in &mut self.channels {
                if let Some(t) = c.trie.walk_mut(&seq) {
                    t.trunc();
                }
                c.trie.prune();
            }
            self.channels
                .retain(|c| c.trie.node.is_some() || !c.trie.branches.is_empty());
            return None;
        }
        let (seq, filter) = registration_key(method, url);
        let idx = self.channel_index(&filter)?;
        let out = self.channels[idx].trie.delete(&seq, flags.noinherit);
        if self.channels[idx].trie.prune() {
            self.channels.remove(idx);
        }
        out
    }

    /// Every registration, in channel order then trie order.
    pub fn entries(&self) -> Vec<UrlEntry<'_, T>> {
        let mut out = Vec::new();
        for c in &self.channels {
            let mut raw = Vec::new();
            c.trie.collect(&mut Vec::new(), &mut raw);
            for (path, noinherit, data) in raw {
                let Some((method, rest)) = path.split_first() else {
                    continue;
                };
                let mut url = String::new();
                for seg in rest {
                    url.push('/');
                    url.push_str(seg);
                }
                if c.filter != "*" {
                    url.push('/');
                    url.push_str(&c.filter);
                }
                if url.is_empty() {
                    url.push('/');
                }
                out.push(UrlEntry {
                    method,
                    url,
                    noinherit,
                    data,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn inherit_applies_beneath() {
        let mut u = UrlSpace::new();
        u.set("GET", "/", "root", UrlFlags::INHERIT);
        u.set("GET", "/foo", "foo", UrlFlags::INHERIT);
        assert_eq!(u.get("GET", "/"), Some(&"root"));
        assert_eq!(u.get("GET", "/bar"), Some(&"root"));
        assert_eq!(u.get("GET", "/foo"), Some(&"foo"));
        assert_eq!(u.get("GET", "/foo/bar/baz"), Some(&"foo"));
        assert_eq!(u.get("POST", "/foo"), None);
    }

    #[test]
    fn noinherit_only_exact() {
        let mut u = UrlSpace::new();
        u.set("GET", "/", "root", UrlFlags::INHERIT);
        u.set("GET", "/foo", "exact", UrlFlags::NOINHERIT);
        assert_eq!(u.get("GET", "/foo"), Some(&"exact"));
        assert_eq!(u.get("GET", "/foo/x"), Some(&"root"));
    }

    #[test]
    fn noinherit_and_inherit_coexist() {
        let mut u = UrlSpace::new();
        u.set("GET", "/a", 1, UrlFlags::INHERIT);
        u.set("GET", "/a", 2, UrlFlags::NOINHERIT);
        assert_eq!(u.get("GET", "/a"), Some(&2));
        assert_eq!(u.get("GET", "/a/b"), Some(&1));
        assert_eq!(u.get_exact("GET", "/a", UrlFlags::INHERIT), Some(&1));
        assert_eq!(u.get_exact("GET", "/a", UrlFlags::NOINHERIT), Some(&2));
    }

    #[test]
    fn wildcard_channels() {
        let mut u = UrlSpace::new();
        u.set("GET", "/", "fastpath", UrlFlags::INHERIT);
        u.set("GET", "/*.adp", "adp", UrlFlags::INHERIT);
        u.set("GET", "/special/*.adp", "special", UrlFlags::INHERIT);
        assert_eq!(u.get("GET", "/index.adp"), Some(&"adp"));
        assert_eq!(u.get("GET", "/a/b/c.adp"), Some(&"adp"));
        assert_eq!(u.get("GET", "/special/x.adp"), Some(&"special"));
        assert_eq!(u.get("GET", "/special/deep/x.adp"), Some(&"special"));
        assert_eq!(u.get("GET", "/index.html"), Some(&"fastpath"));
        assert_eq!(u.get("GET", "/special/x.html"), Some(&"fastpath"));
    }

    #[test]
    fn more_specific_channel_wins() {
        let mut u = UrlSpace::new();
        u.set("GET", "/*", "any", UrlFlags::INHERIT);
        u.set("GET", "/*.tcl", "tcl", UrlFlags::INHERIT);
        assert_eq!(u.get("GET", "/x.tcl"), Some(&"tcl"));
        assert_eq!(u.get("GET", "/x.txt"), Some(&"any"));
    }

    #[test]
    fn replace_returns_old() {
        let mut u = UrlSpace::new();
        assert_eq!(u.set("GET", "/x", 1, UrlFlags::INHERIT), None);
        assert_eq!(u.set("GET", "/x", 2, UrlFlags::INHERIT), Some(1));
        assert_eq!(u.get("GET", "/x"), Some(&2));
    }

    #[test]
    fn remove_and_recurse() {
        let mut u = UrlSpace::new();
        u.set("GET", "/a", 1, UrlFlags::INHERIT);
        u.set("GET", "/a/b", 2, UrlFlags::INHERIT);
        u.set("GET", "/a/*.html", 3, UrlFlags::INHERIT);
        assert_eq!(u.remove("GET", "/a/b", UrlFlags::INHERIT), Some(2));
        assert_eq!(u.get("GET", "/a/b"), Some(&1));
        assert_eq!(u.remove("GET", "/a/b", UrlFlags::INHERIT), None);
        u.remove(
            "GET",
            "/a",
            UrlFlags {
                recurse: true,
                ..UrlFlags::INHERIT
            },
        );
        assert_eq!(u.get("GET", "/a/x.html"), None);
        assert_eq!(u.get("GET", "/a"), None);
        assert!(u.entries().is_empty(), "urlspace should be empty");
    }

    #[test]
    fn entries_roundtrip() {
        let mut u = UrlSpace::new();
        u.set("GET", "/", 0, UrlFlags::INHERIT);
        u.set("POST", "/a/b", 1, UrlFlags::NOINHERIT);
        u.set("GET", "/x/*.adp", 2, UrlFlags::INHERIT);
        let e = u.entries();
        let mut urls: Vec<(String, String, bool)> = e
            .iter()
            .map(|e| (e.method.to_owned(), e.url.clone(), e.noinherit))
            .collect();
        urls.sort();
        assert_eq!(
            urls,
            vec![
                ("GET".into(), "/".into(), false),
                ("GET".into(), "/x/*.adp".into(), false),
                ("POST".into(), "/a/b".into(), true),
            ]
        );
    }

    #[test]
    fn segments_ignore_empty() {
        assert_eq!(url_segments("//a///b/"), vec!["a", "b"]);
        assert_eq!(url_segments("/"), [] as [std::string::String; 0]);
    }

    proptest! {
        /// An inheriting registration at a prefix is found for any URL below
        /// it, unless a deeper registration exists.
        #[test]
        fn prefix_inheritance(prefix in proptest::collection::vec("[a-c]", 0..4),
                              suffix in proptest::collection::vec("[a-c]", 0..4)) {
            let mut u = UrlSpace::new();
            let p = format!("/{}", prefix.join("/"));
            u.set("GET", &p, 7, UrlFlags::INHERIT);
            let full = format!("{}/{}", p, suffix.join("/"));
            prop_assert_eq!(u.get("GET", &full), Some(&7));
            prop_assert_eq!(u.remove("GET", &p, UrlFlags::INHERIT), Some(7));
            prop_assert_eq!(u.get("GET", &full), None);
        }
    }
}
