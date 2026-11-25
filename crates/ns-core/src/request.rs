//! HTTP request-line and header parsing (`nsd/request.c`).

use encoding_rs::Encoding;

use crate::pathname::normalize_path;
use crate::set::NsSet;
use crate::urlencode::decode_url;

/// How header keys are cased when parsed (`Ns_HeaderCaseDisposition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderCase {
    /// Keep keys as sent.
    #[default]
    Preserve,
    /// Lower-case keys.
    ToLower,
    /// Upper-case keys.
    ToUpper,
}

impl HeaderCase {
    /// Parse the `headercase` config value.
    pub fn from_config(s: &str) -> Self {
        if s.eq_ignore_ascii_case("tolower") {
            Self::ToLower
        } else if s.eq_ignore_ascii_case("toupper") {
            Self::ToUpper
        } else {
            Self::Preserve
        }
    }
}

/// A parsed HTTP request line (`Ns_Request`).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The trimmed original request line, kept for logging.
    pub line: String,
    /// Request method, e.g. `GET`.
    pub method: String,
    /// Protocol from an absolute URL (`http` in `http://host/x`).
    pub protocol: Option<String>,
    /// Host from an absolute URL.
    pub host: Option<String>,
    /// Port from an absolute URL (0 if absent).
    pub port: u16,
    /// Decoded, normalized URL path.  Retains a trailing `/` if the client
    /// sent one (other than the root).
    pub url: String,
    /// Raw (still encoded) query string, if any.
    pub query: Option<String>,
    /// Path segments of `url`.
    pub urlv: Vec<String>,
    /// HTTP version, e.g. `1.1`; `0.0` for a version-less (HTTP/0.9) line.
    pub version: f64,
    /// Major HTTP version.
    pub major: u32,
    /// Minor HTTP version.
    pub minor: u32,
}

/// Locate the last ` HTTP/x.y` token in a request line (`NsFindVersion`).
/// Returns the byte offset of the leading space and the version numbers.
pub fn find_version(line: &str) -> Option<(usize, u32, u32)> {
    let idx = line.rfind(" HTTP/")?;
    let ver = &line[idx + 6..];
    let (maj, rest) = ver.split_once('.')?;
    let min: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let major = maj.trim().parse().ok()?;
    let minor = min.parse().ok()?;
    Some((idx, major, minor))
}

fn version_float(major: u32, minor: u32) -> f64 {
    let mut div = 10u64;
    while u64::from(minor) / div > 0 {
        div *= 10;
    }
    f64::from(major) + f64::from(minor) / div as f64
}

impl Request {
    /// Parse a request line (`Ns_ParseRequestEx`).  Returns `None` for
    /// malformed lines.  `encoding` controls URL decoding.
    pub fn parse(line: &str, encoding: Option<&'static Encoding>) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let split = line.find(|c: char| c.is_ascii_whitespace())?;
        let method = &line[..split];
        let mut rest = line[split..].trim_start();
        if rest.is_empty() {
            return None;
        }
        let (mut major, mut minor, mut version) = (0, 9, 0.0);
        if let Some((idx, ma, mi)) = find_version(&format!(" {rest}")) {
            // Offset is relative to the prefixed string.
            rest = &rest[..idx.saturating_sub(1).min(rest.len())];
            major = ma;
            minor = mi;
            version = version_float(ma, mi);
        }
        let mut url = rest.trim();
        if url.is_empty() {
            return None;
        }
        let (mut protocol, mut host, mut port) = (None, None, 0u16);
        if !url.starts_with('/') {
            if let Some(colon) = url.find([':', '/']).filter(|&i| url.as_bytes()[i] == b':') {
                protocol = Some(url[..colon].to_owned());
                let after = &url[colon + 1..];
                url = after;
                if after.len() > 3 && after.starts_with("//") && !after[2..].starts_with('/') {
                    let hp_and_path = &after[2..];
                    let (hp, path) = match hp_and_path.find('/') {
                        Some(i) => (&hp_and_path[..i], &hp_and_path[i + 1..]),
                        None => (hp_and_path, ""),
                    };
                    let h = if let Some((h, p)) = hp.split_once(':') {
                        port = p.parse().ok()?;
                        h
                    } else {
                        hp
                    };
                    host = Some(h.to_owned());
                    url = path;
                }
            }
        }
        let mut req = Self {
            line: line.to_owned(),
            method: method.to_owned(),
            protocol,
            host,
            port,
            url: String::new(),
            query: None,
            urlv: Vec::new(),
            version,
            major,
            minor,
        };
        req.set_url(url, encoding);
        Some(req)
    }

    /// Replace the URL, re-deriving `url`, `query` and `urlv`
    /// (`Ns_SetRequestUrl`).  A query string in `url` replaces any existing
    /// one.
    pub fn set_url(&mut self, url: &str, encoding: Option<&'static Encoding>) {
        let path = match url.split_once('?') {
            Some((p, q)) => {
                self.query = Some(q.to_owned());
                p
            }
            None => url,
        };
        let decoded = decode_url_path(path, encoding);
        let mut norm = normalize_path(&decoded);
        let stripped = path.trim_start_matches('/');
        if !stripped.is_empty() && stripped.ends_with('/') && norm != "/" {
            norm.push('/');
        }
        self.urlv = norm
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        self.url = norm;
    }

    /// The URL with the first `n` segments removed (`Ns_SkipUrl`).
    pub fn skip_url(&self, n: usize) -> Option<String> {
        if n > self.urlv.len() {
            return None;
        }
        let mut s = String::new();
        for seg in &self.urlv[n..] {
            s.push('/');
            s.push_str(seg);
        }
        if self.url.ends_with('/') && self.url.len() > 1 {
            s.push('/');
        }
        if s.is_empty() {
            s.push('/');
        }
        Some(s)
    }
}

/// Decode a URL *path*: unlike form data, `+` stays a literal plus.
pub fn decode_url_path(path: &str, encoding: Option<&'static Encoding>) -> String {
    if path.contains('+') {
        // Protect '+' from the form-style decoder.
        decode_url(&path.replace('+', "%2b"), encoding)
    } else {
        decode_url(path, encoding)
    }
}

/// Error from [`parse_header`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    /// A continuation line appeared before any header.
    #[error("header continuation before first header")]
    OrphanContinuation,
    /// A line without a `:` separator.
    #[error("malformed header line")]
    Malformed,
}

/// Parse one header line into `set` (`Ns_ParseHeader`).  Lines starting with
/// whitespace continue the previous header's value.
pub fn parse_header(set: &mut NsSet, line: &str, case: HeaderCase) -> Result<(), HeaderError> {
    if line.starts_with(|c: char| c.is_ascii_whitespace()) {
        let idx = set.last_index().ok_or(HeaderError::OrphanContinuation)?;
        let cont = line.trim_start();
        if !cont.is_empty() {
            let prev = set.value(idx).flatten().unwrap_or("").to_owned();
            set.put_value(idx, Some(&format!("{prev} {cont}")));
        }
        return Ok(());
    }
    let (key, value) = line.split_once(':').ok_or(HeaderError::Malformed)?;
    let key = match case {
        HeaderCase::Preserve => key.to_owned(),
        HeaderCase::ToLower => key.to_ascii_lowercase(),
        HeaderCase::ToUpper => key.to_ascii_uppercase(),
    };
    set.put(&key, Some(value.trim_start()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_get() {
        let r = Request::parse("GET /a/b.html?x=1 HTTP/1.1\r\n", None).expect("parse");
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "/a/b.html");
        assert_eq!(r.query.as_deref(), Some("x=1"));
        assert_eq!(r.urlv, vec!["a", "b.html"]);
        assert!((r.version - 1.1).abs() < f64::EPSILON);
        assert_eq!((r.major, r.minor), (1, 1));
        assert_eq!(r.line, "GET /a/b.html?x=1 HTTP/1.1");
    }

    #[test]
    fn http09_and_trailing_slash() {
        let r = Request::parse("GET /dir/", None).expect("parse");
        assert!(r.version.abs() < f64::EPSILON);
        assert_eq!(r.url, "/dir/");
        assert_eq!(r.urlv, vec!["dir"]);
        let root = Request::parse("GET / HTTP/1.0", None).expect("parse");
        assert_eq!(root.url, "/");
        assert_eq!(root.urlv, [] as [std::string::String; 0]);
    }

    #[test]
    fn absolute_url_proxy_form() {
        let r = Request::parse("GET http://example.com:8080/x/y HTTP/1.0", None).expect("parse");
        assert_eq!(r.protocol.as_deref(), Some("http"));
        assert_eq!(r.host.as_deref(), Some("example.com"));
        assert_eq!(r.port, 8080);
        assert_eq!(r.url, "/x/y");
        let r2 = Request::parse("GET http://example.com HTTP/1.0", None).expect("parse");
        assert_eq!(r2.url, "/");
        assert!(Request::parse("GET http://h:bad/ HTTP/1.0", None).is_none());
    }

    #[test]
    fn decodes_and_normalizes() {
        let r = Request::parse("GET /a%20b/../c+d HTTP/1.0", None).expect("parse");
        assert_eq!(r.url, "/c+d");
        let r = Request::parse("GET /../../etc/passwd HTTP/1.0", None).expect("parse");
        assert_eq!(r.url, "/etc/passwd");
    }

    #[test]
    fn malformed() {
        assert!(Request::parse("", None).is_none());
        assert!(Request::parse("GET", None).is_none());
        assert!(Request::parse("   ", None).is_none());
    }

    #[test]
    fn version_parsing() {
        assert_eq!(find_version("GET / HTTP/1.0"), Some((5, 1, 0)));
        assert_eq!(find_version("GET /HTTP/x HTTP/1.1"), Some((11, 1, 1)));
        assert_eq!(find_version("GET / HTTP/x"), None);
        assert!((version_float(1, 10) - 1.1).abs() < 1e-9);
    }

    #[test]
    fn skip_url() {
        let r = Request::parse("GET /a/b/c HTTP/1.0", None).expect("parse");
        assert_eq!(r.skip_url(1).as_deref(), Some("/b/c"));
        assert_eq!(r.skip_url(3).as_deref(), Some("/"));
        assert_eq!(r.skip_url(4), None);
    }

    #[test]
    fn headers() {
        let mut s = NsSet::default();
        parse_header(&mut s, "Content-Type:  text/html", HeaderCase::ToLower).expect("hdr");
        parse_header(&mut s, "  charset=x", HeaderCase::ToLower).expect("cont");
        parse_header(&mut s, "X-A: 1", HeaderCase::ToUpper).expect("hdr");
        assert_eq!(s.get("content-type"), Some("text/html charset=x"));
        assert_eq!(s.get("X-A"), Some("1"));
        assert_eq!(
            parse_header(&mut s, "bogus", HeaderCase::Preserve),
            Err(HeaderError::Malformed)
        );
        let mut e = NsSet::default();
        assert_eq!(
            parse_header(&mut e, " cont", HeaderCase::Preserve),
            Err(HeaderError::OrphanContinuation)
        );
        assert_eq!(HeaderCase::from_config("ToLower"), HeaderCase::ToLower);
    }
}
