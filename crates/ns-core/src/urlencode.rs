//! URL encoding and decoding (`nsd/urlencode.c`).
//!
//! AOLserver encodes every byte except ASCII letters and digits as `%xx`
//! (lower-case hex) and spaces as `+`.  Decoding turns `+` into a space and
//! valid `%xx` escapes into bytes; malformed escapes are copied literally.
//! An optional charset converts between UTF-8 and the wire encoding.

use encoding_rs::Encoding;

/// Look up a charset by name (`Ns_GetCharsetEncoding`).  Returns `None` for
/// unknown names.  `utf-8` is reported as `None` too because it is the
/// identity encoding for Rust strings.
pub fn charset_encoding(charset: &str) -> Option<&'static Encoding> {
    let enc = Encoding::for_label(charset.trim().as_bytes())?;
    if enc == encoding_rs::UTF_8 {
        None
    } else {
        Some(enc)
    }
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Percent-encode raw bytes (`Ns_EncodeUrl` core).
pub fn encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        if b.is_ascii_alphanumeric() {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
    out
}

/// Encode `s`, first converting it to `encoding` if given
/// (`Ns_EncodeUrlWithEncoding`).
pub fn encode_url(s: &str, encoding: Option<&'static Encoding>) -> String {
    match encoding {
        None => encode_bytes(s.as_bytes()),
        Some(enc) => {
            let (bytes, _, _) = enc.encode(s);
            encode_bytes(&bytes)
        }
    }
}

/// Decode percent escapes and `+` into raw bytes.
pub fn decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Decode a URL component into a UTF-8 string, interpreting the decoded
/// bytes in `encoding` (UTF-8 if `None`, lossily)
/// (`Ns_DecodeUrlWithEncoding`).
pub fn decode_url(s: &str, encoding: Option<&'static Encoding>) -> String {
    let bytes = decode_bytes(s);
    match encoding {
        None => String::from_utf8_lossy(&bytes).into_owned(),
        Some(enc) => enc.decode_without_bom_handling(&bytes).0.into_owned(),
    }
}

/// Parse a query string into ordered `(key, value)` pairs
/// (`Ns_QueryToSet` / form parsing of `application/x-www-form-urlencoded`).
pub fn parse_query(query: &str, encoding: Option<&'static Encoding>) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode_url(k, encoding), decode_url(v, encoding)),
            None => (decode_url(pair, encoding), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn encodes_like_aolserver() {
        assert_eq!(encode_url("a b&c=d/é", None), "a+b%26c%3dd%2f%c3%a9");
        assert_eq!(encode_url("Az09", None), "Az09");
        assert_eq!(encode_url("-_.~", None), "%2d%5f%2e%7e");
    }

    #[test]
    fn decodes_like_aolserver() {
        assert_eq!(decode_url("a+b%26c%3Dd", None), "a b&c=d");
        assert_eq!(decode_url("100%", None), "100%");
        assert_eq!(decode_url("%zz%4", None), "%zz%4");
        assert_eq!(decode_url("%41", None), "A");
        assert_eq!(decode_url("%c3%a9", None), "é");
    }

    #[test]
    fn charset_roundtrip() {
        let latin1 = charset_encoding("iso-8859-1");
        assert!(latin1.is_some());
        assert_eq!(encode_url("é", latin1), "%e9");
        assert_eq!(decode_url("%e9", latin1), "é");
        assert!(charset_encoding("utf-8").is_none());
        assert!(charset_encoding("no-such-charset").is_none());
    }

    #[test]
    fn query_parsing() {
        let q = parse_query("a=1&b=two+words&&c&a=3", None);
        assert_eq!(
            q,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "two words".into()),
                ("c".into(), String::new()),
                ("a".into(), "3".into()),
            ]
        );
    }

    proptest! {
        #[test]
        fn roundtrip(s in "\\PC{0,40}") {
            prop_assert_eq!(decode_url(&encode_url(&s, None), None), s);
        }
    }
}
