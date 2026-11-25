//! `Ns_Set`: an ordered multimap of string keys to optional string values.
//!
//! Ported from `nsd/set.c`.  An [`NsSet`] keeps fields in insertion order,
//! permits duplicate keys, and distinguishes a `NULL` value (`None`) from an
//! empty string.  Lookups come in case-sensitive and case-insensitive
//! (`i`-prefixed) flavours, exactly like the C API.
//!
//! The behavioural contract of this module is specified and proved in
//! `verus/ns_set.rs`: `find` returns the *first* matching index, `put`
//! appends and returns the new index, `delete` preserves the relative order
//! of the remaining fields, and `trunc` keeps exactly the prefix.

use std::fmt;

/// A single key/value field in an [`NsSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Field key.
    pub key: String,
    /// Field value; `None` mirrors a C `NULL` value.
    pub value: Option<String>,
}

/// An ordered, duplicate-permitting set of key/value pairs (`Ns_Set`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NsSet {
    name: Option<String>,
    fields: Vec<Field>,
}

impl NsSet {
    /// Create an empty set (`Ns_SetCreate`).
    pub fn new(name: Option<&str>) -> Self {
        Self {
            name: name.map(str::to_owned),
            fields: Vec::new(),
        }
    }

    /// Create an empty set with the given name.
    pub fn named(name: &str) -> Self {
        Self::new(Some(name))
    }

    /// The set's name, if any (`Ns_SetName`).
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Rename the set.
    pub fn set_name(&mut self, name: Option<&str>) {
        self.name = name.map(str::to_owned);
    }

    /// Number of fields (`Ns_SetSize`).
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// True if the set has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Index of the last field, if any (`Ns_SetLast`).
    pub fn last_index(&self) -> Option<usize> {
        self.fields.len().checked_sub(1)
    }

    /// All fields in order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Iterate `(key, value)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.fields
            .iter()
            .map(|f| (f.key.as_str(), f.value.as_deref()))
    }

    /// Key at `index` (`Ns_SetKey`).
    pub fn key(&self, index: usize) -> Option<&str> {
        self.fields.get(index).map(|f| f.key.as_str())
    }

    /// Value at `index` (`Ns_SetValue`).  The outer `Option` is `None` when
    /// the index is out of range; the inner is `None` for a NULL value.
    pub fn value(&self, index: usize) -> Option<Option<&str>> {
        self.fields.get(index).map(|f| f.value.as_deref())
    }

    /// Append a field and return its index (`Ns_SetPut`).
    pub fn put(&mut self, key: &str, value: Option<&str>) -> usize {
        self.fields.push(Field {
            key: key.to_owned(),
            value: value.map(str::to_owned),
        });
        self.fields.len() - 1
    }

    /// Append a field with a non-NULL value.
    pub fn put_str(&mut self, key: &str, value: &str) -> usize {
        self.put(key, Some(value))
    }

    /// Delete every field with `key` then append (`Ns_SetUpdate`).
    pub fn update(&mut self, key: &str, value: Option<&str>) -> usize {
        self.delete_key(key);
        self.put(key, value)
    }

    /// Delete every field matching `key` case-insensitively then append.
    pub fn iupdate(&mut self, key: &str, value: Option<&str>) -> usize {
        while self.idelete_key(key) {}
        self.put(key, value)
    }

    /// First index whose key equals `key` exactly (`Ns_SetFind`).
    pub fn find(&self, key: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.key == key)
    }

    /// First index whose key equals `key` ignoring ASCII case (`Ns_SetIFind`).
    pub fn ifind(&self, key: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.key.eq_ignore_ascii_case(key))
    }

    /// Value of the first field named `key` (`Ns_SetGet`).  NULL values and
    /// missing keys both yield `None`, as in C.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.find(key).and_then(|i| self.fields[i].value.as_deref())
    }

    /// Case-insensitive [`NsSet::get`] (`Ns_SetIGet`).
    pub fn iget(&self, key: &str) -> Option<&str> {
        self.ifind(key)
            .and_then(|i| self.fields[i].value.as_deref())
    }

    /// True if `key` occurs exactly once (`Ns_SetUnique`).
    pub fn unique(&self, key: &str) -> bool {
        self.fields.iter().filter(|f| f.key == key).count() == 1
    }

    /// Case-insensitive [`NsSet::unique`] (`Ns_SetIUnique`).
    pub fn iunique(&self, key: &str) -> bool {
        self.fields
            .iter()
            .filter(|f| f.key.eq_ignore_ascii_case(key))
            .count()
            == 1
    }

    /// Keep only the first `size` fields (`Ns_SetTrunc`).
    pub fn trunc(&mut self, size: usize) {
        self.fields.truncate(size);
    }

    /// Remove the field at `index`, preserving order (`Ns_SetDelete`).
    /// Out-of-range indexes are ignored.
    pub fn delete(&mut self, index: usize) {
        if index < self.fields.len() {
            self.fields.remove(index);
        }
    }

    /// Replace the value at `index` (`Ns_SetPutValue`).
    pub fn put_value(&mut self, index: usize, value: Option<&str>) {
        if let Some(f) = self.fields.get_mut(index) {
            f.value = value.map(str::to_owned);
        }
    }

    /// Delete the first field named `key`; returns whether one was removed
    /// (`Ns_SetDeleteKey`).
    pub fn delete_key(&mut self, key: &str) -> bool {
        self.find(key).is_some_and(|i| {
            self.fields.remove(i);
            true
        })
    }

    /// Case-insensitive [`NsSet::delete_key`] (`Ns_SetIDeleteKey`).
    pub fn idelete_key(&mut self, key: &str) -> bool {
        self.ifind(key).is_some_and(|i| {
            self.fields.remove(i);
            true
        })
    }

    /// Add every field from `low` whose key is absent in `self`
    /// (`Ns_SetMerge`).
    pub fn merge(&mut self, low: &Self) {
        for f in &low.fields {
            if self.find(&f.key).is_none() {
                self.fields.push(f.clone());
            }
        }
    }

    /// Move all fields of `from` to the end of `self`, emptying `from`
    /// (`Ns_SetMove`).
    pub fn move_from(&mut self, from: &mut Self) {
        self.fields.append(&mut from.fields);
    }

    /// Split a set on `sep` in keys into several named sets (`Ns_SetSplit`).
    ///
    /// A key `a.b` goes to the set named `a` with key `b`; keys without the
    /// separator go to an unnamed set.  Sets are returned in order of first
    /// appearance.
    pub fn split(&self, sep: char) -> Vec<Self> {
        let mut out: Vec<Self> = Vec::new();
        for f in &self.fields {
            let (name, key) = match f.key.split_once(sep) {
                Some((n, k)) => (Some(n), k),
                None => (None, f.key.as_str()),
            };
            let idx = if let Some(i) = out.iter().position(|s| s.name.as_deref() == name) {
                i
            } else {
                out.push(Self::new(name));
                out.len() - 1
            };
            out[idx].put(key, f.value.as_deref());
        }
        out
    }

    /// Render as a flat key/value list suitable for `array set`.
    pub fn to_pairs(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(self.fields.len() * 2);
        for f in &self.fields {
            v.push(f.key.clone());
            v.push(f.value.clone().unwrap_or_default());
        }
        v
    }
}

impl fmt::Display for NsSet {
    /// Equivalent of `Ns_SetPrint`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}:", self.name.as_deref().unwrap_or("<Unamed set>"))?;
        for fld in &self.fields {
            writeln!(
                f,
                "\t{} = {}",
                fld.key,
                fld.value.as_deref().unwrap_or("(null)")
            )?;
        }
        Ok(())
    }
}

impl<'a> FromIterator<(&'a str, &'a str)> for NsSet {
    fn from_iter<I: IntoIterator<Item = (&'a str, &'a str)>>(iter: I) -> Self {
        let mut s = Self::default();
        for (k, v) in iter {
            s.put(k, Some(v));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn put_find_get() {
        let mut s = NsSet::named("hdrs");
        assert_eq!(s.put_str("Content-Type", "text/html"), 0);
        assert_eq!(s.put_str("X", "1"), 1);
        assert_eq!(s.put_str("x", "2"), 2);
        assert_eq!(s.find("x"), Some(2));
        assert_eq!(s.ifind("x"), Some(1));
        assert_eq!(s.get("X"), Some("1"));
        assert_eq!(s.iget("content-type"), Some("text/html"));
        assert!(s.unique("X"));
        assert!(!s.iunique("X"));
        assert_eq!(s.name(), Some("hdrs"));
    }

    #[test]
    fn null_values_are_distinct_from_empty() {
        let mut s = NsSet::default();
        s.put("a", None);
        s.put("b", Some(""));
        assert_eq!(s.get("a"), None);
        assert_eq!(s.value(0), Some(None));
        assert_eq!(s.get("b"), Some(""));
        assert_eq!(s.value(5), None);
    }

    #[test]
    fn delete_trunc_update() {
        let mut s: NsSet = [("a", "1"), ("b", "2"), ("a", "3")].into_iter().collect();
        s.delete(0);
        assert_eq!(s.get("a"), Some("3"));
        s.delete(99); // ignored
        assert_eq!(s.len(), 2);
        s.update("b", Some("9"));
        assert_eq!(s.key(1), Some("b"));
        assert_eq!(s.get("b"), Some("9"));
        s.trunc(1);
        assert_eq!(s.len(), 1);
        s.trunc(10);
        assert_eq!(s.len(), 1);
        assert!(s.delete_key("a"));
        assert!(!s.delete_key("a"));
        assert!(s.is_empty());
        assert_eq!(s.last_index(), None);
    }

    #[test]
    fn iupdate_removes_all_case_variants() {
        let mut s: NsSet = [("A", "1"), ("a", "2"), ("b", "x")].into_iter().collect();
        s.iupdate("a", Some("3"));
        assert_eq!(s.len(), 2);
        assert_eq!(s.iget("A"), Some("3"));
    }

    #[test]
    fn merge_and_move() {
        let mut high: NsSet = std::iter::once(("a", "1")).collect();
        let low: NsSet = [("a", "x"), ("b", "2")].into_iter().collect();
        high.merge(&low);
        assert_eq!(high.to_pairs(), vec!["a", "1", "b", "2"]);
        let mut from = low.clone();
        high.move_from(&mut from);
        assert!(from.is_empty());
        assert_eq!(high.len(), 4);
    }

    #[test]
    fn split_groups_by_prefix() {
        let s: NsSet = [("a.x", "1"), ("y", "2"), ("a.z", "3"), ("b.q", "4")]
            .into_iter()
            .collect();
        let parts = s.split('.');
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].name(), Some("a"));
        assert_eq!(parts[0].to_pairs(), vec!["x", "1", "z", "3"]);
        assert_eq!(parts[1].name(), None);
        assert_eq!(parts[2].name(), Some("b"));
    }

    #[test]
    fn display_matches_ns_set_print() {
        let mut s = NsSet::default();
        s.put("k", None);
        assert_eq!(s.to_string(), "<Unamed set>:\n\tk = (null)\n");
    }

    proptest! {
        #[test]
        fn find_returns_first_match(keys in proptest::collection::vec("[a-c]", 0..12), probe in "[a-c]") {
            let mut s = NsSet::default();
            for k in &keys { s.put_str(k, "v"); }
            let expected = keys.iter().position(|k| *k == probe);
            prop_assert_eq!(s.find(&probe), expected);
        }

        #[test]
        fn delete_preserves_order(keys in proptest::collection::vec("[a-z]{1,3}", 1..12), idx in 0usize..12) {
            let mut s = NsSet::default();
            for k in &keys { s.put_str(k, "v"); }
            s.delete(idx);
            let mut expect = keys.clone();
            if idx < expect.len() { expect.remove(idx); }
            let got: Vec<String> = s.iter().map(|(k, _)| k.to_owned()).collect();
            prop_assert_eq!(got, expect);
        }
    }
}
