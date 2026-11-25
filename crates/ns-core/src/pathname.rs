//! Path utilities from `nsd/pathname.c`.

use std::path::{Path, PathBuf};

/// True for `/` (and `\` on Windows, as in `ISSLASH`).
fn is_slash(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Normalize a path (`Ns_NormalizePath`): collapse repeated slashes, drop
/// `.` components, and resolve `..` without ever climbing above the root.
/// The result always starts with `/` and never ends with one (except the
/// root itself).
pub fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for part in path.split(is_slash) {
        match part {
            "" | "." => {}
            ".." => {
                if let Some(i) = out.rfind('/') {
                    out.truncate(i);
                }
            }
            p => {
                out.push('/');
                out.push_str(p);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// True if `path` is absolute (`Ns_PathIsAbsolute`).
pub fn path_is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute() || path.starts_with('/')
}

/// Join path components with `/`, trimming redundant slashes between them
/// (`Ns_MakePath`).
pub fn make_path<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for (i, part) in parts.into_iter().enumerate() {
        let p = part.as_ref();
        let trimmed = if i == 0 {
            p.trim_end_matches('/')
        } else {
            p.trim_matches('/')
        };
        if trimmed.is_empty() && i > 0 {
            continue;
        }
        if i > 0 {
            out.push('/');
        }
        out.push_str(trimmed);
    }
    if out.is_empty() {
        "/".to_owned()
    } else {
        out
    }
}

/// Join `url` beneath `root`, refusing to escape it.  The URL is normalized
/// first, so `..` can never reach outside `root` (`Ns_UrlToFile` core).
pub fn url_to_path(root: &Path, url: &str) -> PathBuf {
    let norm = normalize_path(url);
    let mut p = root.to_path_buf();
    for seg in norm.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn normalize() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("//a//b/"), "/a/b");
        assert_eq!(normalize_path("/a/./b/../c"), "/a/c");
        assert_eq!(normalize_path("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_path("a/b"), "/a/b");
        assert_eq!(normalize_path("/a/b/.."), "/a");
    }

    #[test]
    fn make_paths() {
        assert_eq!(
            make_path(["/usr/local/", "/lib/", "x.tcl"]),
            "/usr/local/lib/x.tcl"
        );
        assert_eq!(make_path(["a", "", "b"]), "a/b");
        assert_eq!(make_path(Vec::<&str>::new()), "/");
    }

    #[test]
    fn absolute() {
        assert!(path_is_absolute("/x"));
        assert!(!path_is_absolute("x/y"));
    }

    #[test]
    fn url_to_path_is_contained() {
        let root = Path::new("/srv/www");
        assert_eq!(url_to_path(root, "/a/../../x"), PathBuf::from("/srv/www/x"));
        assert_eq!(url_to_path(root, "/"), PathBuf::from("/srv/www"));
    }

    proptest! {
        #[test]
        fn normalized_has_no_dot_segments(p in "[a./]{0,30}") {
            let n = normalize_path(&p);
            prop_assert!(n.starts_with('/'));
            prop_assert!(!n.contains("//"));
            for seg in n.split('/') {
                prop_assert!(seg != "." && seg != "..");
            }
            prop_assert_eq!(normalize_path(&n), n);
        }
    }
}
