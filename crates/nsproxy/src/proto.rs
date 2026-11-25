//! Wire protocol between the server and its worker processes: each message
//! is a big-endian `u32` length followed by the payload.  Requests are the
//! script text (empty = ping); replies are [`Reply::encode`]d.
//!
//! AOLserver's `Req`/`Res` headers carry a major/minor version; the worker
//! binary ships with the server here, so the format is private.

use std::io::{self, Read, Write};

/// Largest message either side accepts.  A corrupt length prefix fails
/// before anything is allocated.
pub const MAX_MSG: usize = 256 << 20;

/// Write one message.
pub fn write_msg(w: &mut dyn Write, data: &[u8]) -> io::Result<()> {
    let n = u32::try_from(data.len())
        .ok()
        .filter(|&n| n as usize <= MAX_MSG)
        .ok_or_else(|| io::Error::other("message too large"))?;
    w.write_all(&n.to_be_bytes())?;
    w.write_all(data)
}

/// Read one message; `None` at a clean EOF.
pub fn read_msg(r: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {n} exceeds {MAX_MSG}"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// A worker's reply (`Res` + `Export`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Tcl completion code.
    pub code: i32,
    /// `errorCode`.
    pub error_code: String,
    /// `errorInfo`.
    pub error_info: String,
    /// Result.
    pub result: String,
}

impl Reply {
    /// A successful reply.
    pub const fn ok(result: String) -> Self {
        Self {
            code: 0,
            error_code: String::new(),
            error_info: String::new(),
            result,
        }
    }

    /// Encode: code, then three length-prefixed strings.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&self.code.to_be_bytes());
        for s in [&self.error_code, &self.error_info, &self.result] {
            let n = u32::try_from(s.len()).unwrap_or(u32::MAX);
            v.extend_from_slice(&n.to_be_bytes());
            v.extend_from_slice(s.as_bytes());
        }
        v
    }

    /// Decode; `None` if malformed.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let code = i32::from_be_bytes(b.get(0..4)?.try_into().ok()?);
        let mut pos = 4;
        let mut take = || -> Option<String> {
            let n = u32::from_be_bytes(b.get(pos..pos + 4)?.try_into().ok()?) as usize;
            let s = String::from_utf8_lossy(b.get(pos + 4..pos + 4 + n)?).into_owned();
            pos += 4 + n;
            Some(s)
        };
        Some(Self {
            code,
            error_code: take()?,
            error_info: take()?,
            result: take()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = Reply {
            code: 1,
            error_code: "E".into(),
            error_info: "info".into(),
            result: "msg é".into(),
        };
        assert_eq!(Reply::decode(&r.encode()), Some(r));
        assert_eq!(Reply::decode(b"x"), None);
        assert_eq!(Reply::decode(&[0, 0, 0, 0, 0, 0, 0, 9]), None);
        let mut buf = Vec::new();
        write_msg(&mut buf, b"hello").expect("w");
        write_msg(&mut buf, b"").expect("w");
        let mut rd: &[u8] = &buf;
        assert_eq!(read_msg(&mut rd).expect("r"), Some(b"hello".to_vec()));
        assert_eq!(read_msg(&mut rd).expect("r"), Some(Vec::new()));
        assert_eq!(read_msg(&mut rd).expect("eof"), None);
        let mut short: &[u8] = &[0, 0, 0, 5, b'a'];
        assert!(read_msg(&mut short).is_err());
    }

    #[test]
    fn oversized_length_is_rejected() {
        // Stray script output ("hell") read as a length prefix.
        let mut garbage: &[u8] = b"hello\n";
        let e = read_msg(&mut garbage).expect_err("too large");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let len = u32::try_from(MAX_MSG + 1).expect("fits").to_be_bytes();
        let mut over: &[u8] = &len;
        assert!(read_msg(&mut over).is_err());
    }
}
