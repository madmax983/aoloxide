//! The configuration database (`nsd/config.c`).
//!
//! The config file is a Tcl script whose `ns_section` / `ns_param` commands
//! fill a [`Config`]: a collection of named sections, each an [`NsSet`].
//! Section names are normalized (trimmed, `\` becomes `/`, lower-cased);
//! parameter lookups are case-insensitive unless the `_exact` variant is
//! used.

use crate::set::NsSet;
use crate::text::{parse_bool, parse_int_prefix};

/// All configuration sections, in creation order.
#[derive(Debug, Clone, Default)]
pub struct Config {
    sections: Vec<NsSet>,
}

/// Normalize a section name the way `GetSection` does.
pub fn normalize_section(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c == '\\' {
                '/'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

impl Config {
    /// Empty config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create a section, returning its index.
    pub fn section_index_or_create(&mut self, name: &str) -> usize {
        let norm = normalize_section(name);
        if let Some(i) = self
            .sections
            .iter()
            .position(|s| s.name() == Some(norm.as_str()))
        {
            i
        } else {
            self.sections.push(NsSet::named(&norm));
            self.sections.len() - 1
        }
    }

    /// Mutable access to a section by index.
    pub fn section_mut(&mut self, idx: usize) -> Option<&mut NsSet> {
        self.sections.get_mut(idx)
    }

    /// Append `key value` to `section`, creating it (`ns_param`).
    pub fn put(&mut self, section: &str, key: &str, value: &str) {
        let i = self.section_index_or_create(section);
        self.sections[i].put_str(key, value);
    }

    /// Look up a section (`Ns_ConfigGetSection`).
    pub fn section(&self, name: &str) -> Option<&NsSet> {
        let norm = normalize_section(name);
        self.sections
            .iter()
            .find(|s| s.name() == Some(norm.as_str()))
    }

    /// All sections (`Ns_ConfigGetSections`).
    pub fn sections(&self) -> &[NsSet] {
        &self.sections
    }

    /// Case-insensitive parameter lookup (`Ns_ConfigGetValue`).
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.iget(key)
    }

    /// Case-sensitive parameter lookup (`Ns_ConfigGetValueExact`).
    pub fn get_exact(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.get(key)
    }

    /// Integer parameter (`Ns_ConfigGetInt`).
    pub fn get_int(&self, section: &str, key: &str) -> Option<i64> {
        parse_int_prefix(self.get(section, key)?)
    }

    /// Boolean parameter (`Ns_ConfigGetBool`).
    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        parse_bool(self.get(section, key)?)
    }

    /// String parameter with a default.
    pub fn string_or(&self, section: &str, key: &str, def: &str) -> String {
        self.get(section, key).unwrap_or(def).to_owned()
    }

    /// Integer parameter with a default.
    pub fn int_or(&self, section: &str, key: &str, def: i64) -> i64 {
        self.get_int(section, key).unwrap_or(def)
    }

    /// Boolean parameter with a default.
    pub fn bool_or(&self, section: &str, key: &str, def: bool) -> bool {
        self.get_bool(section, key).unwrap_or(def)
    }

    /// Build a section path `ns[/server/S][/module/M][/parts...]`
    /// (`Ns_ConfigGetPath` without the existence check).
    pub fn make_path(server: Option<&str>, module: Option<&str>, parts: &[&str]) -> String {
        let mut s = String::from("ns");
        if let Some(server) = server {
            s.push_str("/server/");
            s.push_str(server);
        }
        if let Some(module) = module {
            s.push_str("/module/");
            s.push_str(module);
        }
        for p in parts {
            let p = p.trim_matches('/');
            if !p.is_empty() {
                s.push('/');
                s.push_str(p);
            }
        }
        s
    }

    /// `Ns_ConfigGetPath`: the path if that section exists.
    pub fn get_path(
        &self,
        server: Option<&str>,
        module: Option<&str>,
        parts: &[&str],
    ) -> Option<String> {
        let p = Self::make_path(server, module, parts);
        self.section(&p).map(|_| normalize_section(&p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_normalized() {
        let mut c = Config::new();
        c.put("  NS\\Server\\S1 ", "PageRoot", "/www");
        assert_eq!(c.get("ns/server/s1", "pageroot"), Some("/www"));
        assert_eq!(c.get_exact("ns/server/s1", "pageroot"), None);
        assert_eq!(c.get_exact("ns/server/s1", "PageRoot"), Some("/www"));
        assert_eq!(c.sections().len(), 1);
    }

    #[test]
    fn typed_getters() {
        let mut c = Config::new();
        c.put("ns/parameters", "debug", "on");
        c.put("ns/parameters", "port", "8080x");
        c.put("ns/parameters", "bad", "zzz");
        assert_eq!(c.get_bool("ns/parameters", "debug"), Some(true));
        assert_eq!(c.get_int("ns/parameters", "port"), Some(8080));
        assert_eq!(c.get_int("ns/parameters", "bad"), None);
        assert_eq!(c.int_or("ns/parameters", "missing", 5), 5);
        assert!(!c.bool_or("ns/parameters", "missing", false));
        assert_eq!(c.string_or("nosuch", "x", "d"), "d");
    }

    #[test]
    fn paths() {
        assert_eq!(
            Config::make_path(Some("s1"), Some("nssock"), &[]),
            "ns/server/s1/module/nssock"
        );
        assert_eq!(
            Config::make_path(None, None, &["/parameters/"]),
            "ns/parameters"
        );
        let mut c = Config::new();
        c.put("ns/server/s1/adp", "map", "/*.adp");
        assert_eq!(
            c.get_path(Some("S1"), None, &["adp"]).as_deref(),
            Some("ns/server/s1/adp")
        );
        assert_eq!(c.get_path(Some("s1"), None, &["fastpath"]), None);
    }
}
