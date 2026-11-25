//! Tcl-compatible glob matching (`Tcl_StringMatch` / `string match`).
//!
//! Supported syntax, identical to Tcl:
//!
//! * `*` matches any sequence of characters (including none);
//! * `?` matches exactly one character;
//! * `[chars]` matches one character from the set; `a-z` ranges are allowed
//!   in either order;
//! * `\x` matches `x` literally.
//!
//! The `*`/`?`/literal fragment is specified and proved in `verus/glob.rs`.

/// Match `s` against glob `pattern` (case-sensitive).
pub fn string_match(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    match_chars(&s, &p, false)
}

/// Match `s` against glob `pattern` ignoring case (`string match -nocase`).
pub fn string_match_nocase(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    match_chars(&s, &p, true)
}

/// True if `pattern` contains glob metacharacters.
pub fn has_glob_chars(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '\\'])
}

fn fold(c: char, nocase: bool) -> char {
    if nocase {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

fn match_chars(s: &[char], p: &[char], nocase: bool) -> bool {
    let (mut si, mut pi) = (0usize, 0usize);
    // Backtracking point for the most recent `*`.
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    // Collapse consecutive stars.
                    while pi < p.len() && p[pi] == '*' {
                        pi += 1;
                    }
                    if pi == p.len() {
                        return true;
                    }
                    star = Some((pi, si));
                    continue;
                }
                '?' if si < s.len() => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                '[' if si < s.len() => {
                    if let Some((matched, next)) = match_class(s[si], p, pi + 1, nocase) {
                        if matched {
                            pi = next;
                            si += 1;
                            continue;
                        }
                    } else {
                        // Unterminated class: Tcl treats it as a failed match.
                        return false;
                    }
                }
                '\\' if pi + 1 < p.len() && si < s.len() => {
                    if fold(p[pi + 1], nocase) == fold(s[si], nocase) {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c if si < s.len() && c != '?' && c != '[' && c != '\\' => {
                    if fold(c, nocase) == fold(s[si], nocase) {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                '\\' if pi + 1 == p.len() && si < s.len()
                    // Trailing backslash matches a literal backslash.
                    && s[si] == '\\' =>
                {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        } else if si == s.len() {
            return true;
        }
        // Mismatch: backtrack to the last star, consuming one more char.
        match star {
            Some((sp, ss)) if ss < s.len() => {
                star = Some((sp, ss + 1));
                pi = sp;
                si = ss + 1;
            }
            _ => return false,
        }
    }
}

/// Match `c` against the bracket expression starting at `p[start]` (just
/// past `[`).  Returns `(matched, index_after_closing_bracket)` or `None` if
/// the class is unterminated.
fn match_class(c: char, p: &[char], start: usize, nocase: bool) -> Option<(bool, usize)> {
    let c = fold(c, nocase);
    let mut i = start;
    let mut matched = false;
    loop {
        if i >= p.len() {
            return None;
        }
        if p[i] == ']' {
            return Some((matched, i + 1));
        }
        let mut lo = p[i];
        if lo == '\\' && i + 1 < p.len() {
            i += 1;
            lo = p[i];
        }
        i += 1;
        if i + 1 < p.len() && p[i] == '-' && p[i + 1] != ']' {
            let mut hi = p[i + 1];
            i += 2;
            if hi == '\\' && i < p.len() {
                hi = p[i];
                i += 1;
            }
            let (a, b) = (fold(lo, nocase), fold(hi, nocase));
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            if a <= c && c <= b {
                matched = true;
            }
        } else if fold(lo, nocase) == c {
            matched = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn basics() {
        assert!(string_match("", ""));
        assert!(string_match("", "*"));
        assert!(!string_match("", "?"));
        assert!(string_match("foo.html", "*.html"));
        assert!(!string_match("foo.htm", "*.html"));
        assert!(string_match("abc", "a?c"));
        assert!(string_match("abc", "a*"));
        assert!(string_match("abc", "*c"));
        assert!(string_match("abc", "**b**"));
        assert!(!string_match("abc", "ab"));
        assert!(string_match("a*c", r"a\*c"));
        assert!(!string_match("abc", r"a\*c"));
    }

    #[test]
    fn classes() {
        assert!(string_match("b", "[abc]"));
        assert!(!string_match("d", "[abc]"));
        assert!(string_match("m", "[a-z]"));
        assert!(string_match("m", "[z-a]"));
        assert!(!string_match("M", "[a-z]"));
        assert!(string_match("x1", "x[0-9]"));
        assert!(!string_match("x", "x[0-9]"));
        assert!(!string_match("a", "[abc"));
        assert!(string_match("]", r"[\]]"));
    }

    #[test]
    fn nocase() {
        assert!(string_match_nocase("FOO.HTML", "*.html"));
        assert!(string_match_nocase("M", "[a-z]"));
        assert!(!string_match("FOO.HTML", "*.html"));
    }

    #[test]
    fn has_globs() {
        assert!(has_glob_chars("*.adp"));
        assert!(!has_glob_chars("/foo/bar"));
    }

    /// Simple exponential reference implementation for `*`, `?`, literals.
    fn reference(s: &[char], p: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|k| reference(&s[k..], &p[1..])),
            Some('?') => !s.is_empty() && reference(&s[1..], &p[1..]),
            Some(c) => s.first() == Some(c) && reference(&s[1..], &p[1..]),
        }
    }

    proptest! {
        #[test]
        fn agrees_with_reference(s in "[ab]{0,8}", p in "[ab*?]{0,6}") {
            let sc: Vec<char> = s.chars().collect();
            let pc: Vec<char> = p.chars().collect();
            prop_assert_eq!(string_match(&s, &p), reference(&sc, &pc));
        }

        #[test]
        fn literal_matches_itself(s in "[a-zA-Z0-9./_-]{0,20}") {
            prop_assert!(string_match(&s, &s));
            prop_assert!(string_match(&s, "*"));
        }
    }
}
