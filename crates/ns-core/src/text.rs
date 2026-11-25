//! Small text codecs: HTML quoting (`quotehtml.c`) and the base64 variant
//! AOLserver calls "htuu" (`uuencode.c`).

/// Escape `< > & ' "` for safe inclusion in HTML (`Ns_QuoteHtml`).
pub fn quote_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

const SIX2PR: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn pr2six(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Base64-encode with `=` padding (`Ns_HtuuEncode`).
pub fn htuu_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(char::from(SIX2PR[usize::from(b0 >> 2)]));
        out.push(char::from(
            SIX2PR[usize::from(((b0 << 4) & 0o60) | ((b1 >> 4) & 0o17))],
        ));
        if chunk.len() > 1 {
            out.push(char::from(
                SIX2PR[usize::from(((b1 << 2) & 0o74) | ((b2 >> 6) & 0o3))],
            ));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(SIX2PR[usize::from(b2 & 0o77)]));
        } else {
            out.push('=');
        }
    }
    out
}

/// Base64-decode, skipping leading blanks and stopping at the first
/// character outside the alphabet (so padding is optional)
/// (`Ns_HtuuDecode`).
pub fn htuu_decode(input: &str) -> Vec<u8> {
    let bytes = input.trim_start_matches([' ', '\t']).as_bytes();
    let vals: Vec<u8> = bytes.iter().map_while(|&c| pr2six(c)).collect();
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() > 1 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
        }
        if chunk.len() > 2 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    out
}

/// Parse `Basic` credentials from an `Authorization` header value into
/// `(user, password)`.
pub fn parse_basic_auth(header: &str) -> Option<(String, String)> {
    let (scheme, rest) = header.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8_lossy(&htuu_decode(rest.trim())).into_owned();
    let (u, p) = decoded.split_once(':').unwrap_or((decoded.as_str(), ""));
    Some((u.to_owned(), p.to_owned()))
}

/// Parse a Tcl-style boolean (`1/0`, `yes/no`, `on/off`, `true/false`,
/// `y/n`, `t/f`; any integer) as used by `Ns_ConfigGetBool`.
pub fn parse_bool(s: &str) -> Option<bool> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "1" | "y" | "yes" | "on" | "t" | "true" => Some(true),
        "0" | "n" | "no" | "off" | "f" | "false" => Some(false),
        _ => t.parse::<i64>().ok().map(|i| i != 0),
    }
}

/// Parse a leading integer like `sscanf("%d")`: optional whitespace, sign,
/// digits; trailing junk is ignored.
pub fn parse_int_prefix(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|&(i, c)| c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')))
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    t[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn quoting() {
        assert_eq!(
            quote_html(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&#34;x&#34;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(quote_html("plain"), "plain");
    }

    #[test]
    fn htuu() {
        assert_eq!(
            htuu_encode(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(htuu_encode(b""), "");
        assert_eq!(htuu_encode(b"ab"), "YWI=");
        assert_eq!(
            htuu_decode("  QWxhZGRpbjpvcGVuIHNlc2FtZQ=="),
            b"Aladdin:open sesame"
        );
        assert_eq!(htuu_decode("YWI"), b"ab");
    }

    #[test]
    fn basic_auth() {
        assert_eq!(
            parse_basic_auth("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="),
            Some(("Aladdin".into(), "open sesame".into()))
        );
        assert_eq!(parse_basic_auth("Digest abc"), None);
        assert_eq!(
            parse_basic_auth("Basic dXNlcg=="),
            Some(("user".into(), String::new()))
        );
    }

    #[test]
    fn bools_and_ints() {
        assert_eq!(parse_bool("Yes"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("7"), Some(true));
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_int_prefix(" 42abc"), Some(42));
        assert_eq!(parse_int_prefix("-3"), Some(-3));
        assert_eq!(parse_int_prefix("x"), None);
    }

    proptest! {
        #[test]
        fn htuu_roundtrip(data in proptest::collection::vec(any::<u8>(), 0..64)) {
            prop_assert_eq!(htuu_decode(&htuu_encode(&data)), data);
        }
    }
}
