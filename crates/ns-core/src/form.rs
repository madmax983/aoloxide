//! Form and query parsing (`nsd/form.c`).
//!
//! * Non-`POST` requests take form data from the URL query string.
//! * `POST` requests take it from the body: `multipart/form-data` bodies are
//!   split on the boundary, anything else is parsed as
//!   `application/x-www-form-urlencoded`.
//! * File parts are recorded as [`FormFile`]s (offset/length into the body)
//!   and their `filename` becomes the field's value, exactly like
//!   `ns_conn files` / `ns_conn fileoffset` in AOLserver.

use encoding_rs::Encoding;

use crate::request::{parse_header, HeaderCase};
use crate::set::NsSet;
use crate::urlencode::decode_url;

/// An uploaded file part in a multipart form (`Ns_ConnFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFile {
    /// Form field name.
    pub name: String,
    /// The part's headers (keys lower-cased).
    pub headers: NsSet,
    /// Byte offset of the file data within the request body.
    pub offset: usize,
    /// Length of the file data.
    pub length: usize,
}

/// The result of parsing a request's form data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    /// Field name/value pairs in order.  Keys without `=` have NULL values.
    pub fields: NsSet,
    /// Uploaded files, keyed by field name (first wins, as in C).
    pub files: Vec<FormFile>,
}

impl Form {
    /// Look up an uploaded file by field name.
    pub fn file(&self, name: &str) -> Option<&FormFile> {
        self.files.iter().find(|f| f.name == name)
    }
}

/// Parse `a=1&b=2` into `set` (`ParseQuery`).  A piece without `=` gets a
/// NULL value.
pub fn parse_query_into(set: &mut NsSet, form: &str, encoding: Option<&'static Encoding>) {
    for piece in form.split('&') {
        match piece.split_once('=') {
            Some((k, v)) => {
                set.put(&decode_url(k, encoding), Some(&decode_url(v, encoding)));
            }
            None => {
                set.put(&decode_url(piece, encoding), None);
            }
        }
    }
}

/// Case-insensitive substring search (`Ns_StrCaseFind`).
pub fn find_nocase(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.to_ascii_lowercase();
    h.find(&needle.to_ascii_lowercase())
}

/// Extract the multipart boundary from a `Content-Type`, prefixed with `--`.
pub fn multipart_boundary(content_type: &str) -> Option<String> {
    find_nocase(content_type, "multipart/form-data")?;
    let i = find_nocase(content_type, "boundary=")? + 9;
    let rest = &content_type[i..];
    let end = rest
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(rest.len());
    let b = rest[..end].trim_matches('"');
    Some(format!("--{b}"))
}

/// Extract `att=value` (quoted or not) from a header value (`GetValue`).
pub fn header_attr<'a>(hdr: &'a str, att: &str) -> Option<&'a str> {
    let i = find_nocase(hdr, att)? + att.len();
    let rest = &hdr[i..];
    if let Some(q @ ('"' | '\'')) = rest.chars().next() {
        let inner = &rest[1..];
        let end = inner.find(q).unwrap_or(inner.len());
        Some(&inner[..end])
    } else {
        let end = rest
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn to_utf8(bytes: &[u8], encoding: Option<&'static Encoding>) -> String {
    match encoding {
        None => String::from_utf8_lossy(bytes).into_owned(),
        Some(e) => e.decode_without_bom_handling(bytes).0.into_owned(),
    }
}

fn parse_part(
    form: &mut Form,
    body: &[u8],
    start: usize,
    mut end: usize,
    encoding: Option<&'static Encoding>,
) {
    if end > start && body[end - 1] == b'\n' {
        end -= 1;
    }
    if end > start && body[end - 1] == b'\r' {
        end -= 1;
    }
    let mut headers = NsSet::default();
    let mut pos = start;
    while let Some(nl) = body[pos..end].iter().position(|&b| b == b'\n') {
        let line_end = pos + nl;
        let mut e = line_end;
        if e > pos && body[e - 1] == b'\r' {
            e -= 1;
        }
        let line = String::from_utf8_lossy(&body[pos..e]).into_owned();
        pos = line_end + 1;
        if line.is_empty() {
            break;
        }
        let _ = parse_header(&mut headers, &line, HeaderCase::ToLower);
    }
    let pos = pos.min(end);
    let Some(disp) = headers.get("content-disposition").map(str::to_owned) else {
        return;
    };
    let Some(name) = header_attr(&disp, "name=") else {
        return;
    };
    let key = name.to_owned();
    if let Some(filename) = header_attr(&disp, "filename=") {
        let value = filename.to_owned();
        if form.file(&key).is_none() {
            form.files.push(FormFile {
                name: key.clone(),
                headers,
                offset: pos,
                length: end - pos,
            });
        }
        form.fields.put(&key, Some(&value));
    } else {
        form.fields
            .put(&key, Some(&to_utf8(&body[pos..end], encoding)));
    }
}

/// Parse a request's form (`Ns_ConnGetQuery`).
pub fn parse_form(
    method: &str,
    url_query: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
    encoding: Option<&'static Encoding>,
) -> Form {
    let mut form = Form::default();
    if method != "POST" {
        if let Some(q) = url_query {
            parse_query_into(&mut form.fields, q, encoding);
        }
        return form;
    }
    if body.is_empty() {
        return form;
    }
    match content_type.and_then(multipart_boundary) {
        None => parse_query_into(&mut form.fields, &String::from_utf8_lossy(body), encoding),
        Some(boundary) => {
            let b = boundary.as_bytes();
            let mut s = find_bytes(body, b, 0);
            while let Some(start) = s {
                let mut p = start + b.len();
                if body.get(p) == Some(&b'\r') {
                    p += 1;
                }
                if body.get(p) == Some(&b'\n') {
                    p += 1;
                }
                let e = find_bytes(body, b, p);
                if let Some(end) = e {
                    parse_part(&mut form, body, p, end, encoding);
                }
                s = e;
            }
        }
    }
    form
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_uses_url_query() {
        let f = parse_form("GET", Some("a=1&b&c=x+y"), None, b"ignored=1", None);
        assert_eq!(f.fields.get("a"), Some("1"));
        assert_eq!(f.fields.value(1), Some(None));
        assert_eq!(f.fields.get("c"), Some("x y"));
        assert_eq!(f.fields.len(), 3);
    }

    #[test]
    fn post_urlencoded_ignores_url_query() {
        let f = parse_form(
            "POST",
            Some("q=1"),
            Some("application/x-www-form-urlencoded"),
            b"name=Mark&v=%41",
            None,
        );
        assert_eq!(f.fields.get("name"), Some("Mark"));
        assert_eq!(f.fields.get("v"), Some("A"));
        assert_eq!(f.fields.get("q"), None);
        assert_eq!(parse_form("POST", None, None, b"", None).fields.len(), 0);
    }

    #[test]
    fn multipart_with_file() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHello\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"up\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nfile data\r\n\
--XyZ--\r\n";
        let f = parse_form(
            "POST",
            None,
            Some("multipart/form-data; boundary=XyZ"),
            body,
            None,
        );
        assert_eq!(f.fields.get("title"), Some("Hello"));
        assert_eq!(f.fields.get("up"), Some("a.txt"));
        let file = f.file("up").expect("file part");
        assert_eq!(&body[file.offset..file.offset + file.length], b"file data");
        assert_eq!(file.headers.get("content-type"), Some("text/plain"));
        assert!(f.file("title").is_none());
    }

    #[test]
    fn helpers() {
        assert_eq!(
            multipart_boundary("Multipart/Form-Data; boundary=abc def").as_deref(),
            Some("--abc")
        );
        assert_eq!(multipart_boundary("text/plain"), None);
        assert_eq!(header_attr("form-data; name=x y", "name="), Some("x"));
        assert_eq!(header_attr("form-data; NAME='q r'", "name="), Some("q r"));
        assert_eq!(header_attr("form-data", "name="), None);
        assert_eq!(find_nocase("ABC", "bc"), Some(1));
    }
}
