//! The external-driver messages (`include/nsextmsg.h`, `nsext/msg.c`),
//! shared by the server's driver (`nsext`) and the proxy daemons.
//!
//! A request is `name\nlen\n` followed by `len` bytes of argument.  A
//! reply is a sequence of items: lines ended by `\n`, and lists of
//! `size\n` + `size` bytes elements ended by [`END_DATA`].

/// Status of a successful request.
pub const OK_STATUS: &str = "ok";
/// Status of a failure that should not be reported.
pub const SILENT_ERROR_STATUS: &str = "silentError";
/// `bestrowid` reply when there is none.
pub const NO_BESTROWID: &str = "__nobestrowid__";
/// Separates the fields of an argument (`code#message`, open args, ...).
pub const ARG_TOKEN_DELIMITER: char = '#';
/// `exec` reply for a statement returning rows (`NS_ROWS`).
pub const EXEC_RET_ROWS: &str = "exec_rows";
/// `exec` reply for a statement returning no rows (`NS_DML`).
pub const EXEC_RET_DML: &str = "exec_dml";
/// The list terminator (`nspd.h`).
pub const END_DATA: &str = "-1\n";

/// A message (`Ns_ExtDbCommandCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Msg {
    Exec = 0,
    BindRow = 1,
    GetRow = 2,
    Flush = 3,
    Cancel = 4,
    GetTableInfo = 5,
    TableList = 6,
    BestRowId = 7,
    ResultId = 8,
    ResultRows = 9,
    SetMaxRows = 10,
    Close = 11,
    Open = 12,
    Ping = 13,
    Identify = 14,
    TraceOn = 15,
    TraceOff = 16,
    GetTypes = 17,
    OpenF = 18,
    CloseF = 19,
    ReadF = 20,
    WriteF = 21,
    DeleteF = 22,
    CreateTmpF = 23,
    ResetHandle = 24,
    SpStart = 25,
    SpSetParam = 26,
    SpExec = 27,
    SpReturnCode = 28,
    SpGetParams = 29,
}

/// `dbcommandinfo`: code, whether an argument is required, name.
const TABLE: [(Msg, bool, &str); 30] = [
    (Msg::Exec, true, "exec"),
    (Msg::BindRow, false, "bindrow"),
    (Msg::GetRow, true, "getrow"),
    (Msg::Flush, false, "flush"),
    (Msg::Cancel, false, "cancel"),
    (Msg::GetTableInfo, true, "gettableinfo"),
    (Msg::TableList, true, "tablelist"),
    (Msg::BestRowId, true, "bestrowid"),
    (Msg::ResultId, false, "resultid"),
    (Msg::ResultRows, false, "resultrows"),
    (Msg::SetMaxRows, false, "setmaxrows"),
    (Msg::Close, false, "close"),
    (Msg::Open, true, "open"),
    (Msg::Ping, false, "ping"),
    (Msg::Identify, false, "identify"),
    (Msg::TraceOn, true, "traceon"),
    (Msg::TraceOff, false, "traceoff"),
    (Msg::GetTypes, false, "gettypes"),
    // Commands for daemons that share no file space with the server.
    (Msg::OpenF, true, "openfile"),
    (Msg::CloseF, true, "closefile"),
    (Msg::ReadF, true, "readfile"),
    (Msg::WriteF, true, "writefile"),
    (Msg::DeleteF, true, "deletefile"),
    (Msg::CreateTmpF, false, "createtmpfile"),
    (Msg::ResetHandle, false, "resethandle"),
    (Msg::SpStart, true, "spstart"),
    (Msg::SpSetParam, true, "spsetparam"),
    (Msg::SpExec, false, "spexec"),
    (Msg::SpReturnCode, false, "spreturncode"),
    (Msg::SpGetParams, false, "spgetparams"),
];

/// One item of a reply, as nsext reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// A status line: `ok`, or an error that ends the reply.
    Status,
    /// A plain line.
    Line,
    /// A size-prefixed list ended by `-1`.
    List,
}

impl Msg {
    /// Every message, in code order.
    pub fn all() -> impl Iterator<Item = Self> {
        TABLE.iter().map(|t| t.0)
    }

    /// Look a message up by name, ignoring case (`Ns_ExtDbMsgNameToCode`).
    pub fn from_name(name: &str) -> Option<Self> {
        TABLE
            .iter()
            .find(|t| t.2.eq_ignore_ascii_case(name))
            .map(|t| t.0)
    }

    /// The message with numeric `code`.
    pub fn from_code(code: i64) -> Option<Self> {
        usize::try_from(code)
            .ok()
            .and_then(|i| TABLE.get(i))
            .map(|t| t.0)
    }

    /// The numeric code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The wire name (`Ns_ExtDbMsgCodeToName`).
    pub const fn name(self) -> &'static str {
        TABLE[self as usize].2
    }

    /// Whether the message needs an argument (`Ns_ExtDbMsgRequiresArg`).
    pub const fn requires_arg(self) -> bool {
        TABLE[self as usize].1
    }

    /// The items of the reply nsext reads after sending this message,
    /// when its status (if any) is `ok`; a non-`ok` status ends the reply
    /// at once.  The table queries are never answered by `nspd` (and
    /// never sent by nsext).  Proved to agree with the daemon in
    /// `verus/extproto.rs`.
    pub const fn reply(self) -> &'static [Item] {
        use Item::{Line, List, Status};
        match self {
            Self::GetTableInfo | Self::TableList | Self::BestRowId => &[],
            Self::Exec | Self::SpExec | Self::OpenF | Self::CreateTmpF => &[Status, Line],
            Self::BindRow | Self::GetRow | Self::ReadF => &[Status, List],
            Self::SpGetParams => &[Status, List, List],
            Self::Flush
            | Self::Cancel
            | Self::Close
            | Self::Open
            | Self::CloseF
            | Self::WriteF
            | Self::DeleteF
            | Self::SpStart
            | Self::SpSetParam => &[Status],
            Self::ResultId
            | Self::ResultRows
            | Self::SetMaxRows
            | Self::Ping
            | Self::Identify
            | Self::TraceOn
            | Self::TraceOff
            | Self::GetTypes
            | Self::ResetHandle
            | Self::SpReturnCode => &[Line],
        }
    }
}

/// Encode a request (`DbProxySend`): `name\nlen\narg`.
pub fn encode(msg: Msg, arg: Option<&[u8]>) -> Vec<u8> {
    let arg = arg.unwrap_or_default();
    let mut v = format!("{}\n{}\n", msg.name(), arg.len()).into_bytes();
    v.extend_from_slice(arg);
    v
}

/// C `atoi`: optional leading whitespace and sign, then digits; 0 if
/// there are none.  Saturates instead of overflowing.
pub fn atoi(s: &str) -> i64 {
    scan_int(s).map_or(0, |(n, _)| n)
}

/// `sscanf("%d")`: the integer at the start of `s` (after whitespace)
/// and the rest, or `None` if there are no digits.
pub fn scan_int(s: &str) -> Option<(i64, &str)> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    if end == 0 {
        return None;
    }
    let mut n: i64 = 0;
    for b in body[..end].bytes() {
        n = n.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    Some((if neg { -n } else { n }, &body[end..]))
}

/// nsext's `AllDigits`: an optional `-` and then only digits (an empty
/// string qualifies).
pub fn all_digits(s: &str) -> bool {
    s.strip_prefix('-')
        .unwrap_or(s)
        .bytes()
        .all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_codes() {
        for (i, m) in Msg::all().enumerate() {
            assert_eq!(usize::from(m.code()), i);
            assert_eq!(Msg::from_code(i as i64), Some(m));
            assert_eq!(Msg::from_name(m.name()), Some(m));
            assert_eq!(Msg::from_name(&m.name().to_uppercase()), Some(m));
        }
        assert_eq!(Msg::all().count(), 30);
        assert_eq!(Msg::from_code(30), None);
        assert_eq!(Msg::from_code(-1), None);
        assert_eq!(Msg::from_name("nosuch"), None);
        assert!(Msg::Exec.requires_arg() && !Msg::Ping.requires_arg());
        assert_eq!(Msg::CreateTmpF.name(), "createtmpfile");
        assert_eq!(encode(Msg::Exec, Some(b"select 1")), b"exec\n8\nselect 1");
        assert_eq!(encode(Msg::Ping, None), b"ping\n0\n");
    }

    #[test]
    fn replies_mirror_the_verus_model() {
        // `client_items` of verus/extproto.rs.
        for m in Msg::all() {
            let r = m.reply();
            let n = r.len();
            match m.code() {
                5..=7 => assert_eq!(n, 0),
                29 => assert_eq!(n, 3),
                0 | 1 | 2 | 18 | 20 | 23 | 27 => assert_eq!(n, 2),
                _ => assert_eq!(n, 1),
            }
            if n > 1 {
                assert_eq!(r[0], Item::Status);
            }
        }
    }

    #[test]
    fn c_number_parsing() {
        assert_eq!(atoi(" 42x"), 42);
        assert_eq!(atoi("-7"), -7);
        assert_eq!(atoi("+3"), 3);
        assert_eq!(atoi("x"), 0);
        assert_eq!(atoi(""), 0);
        assert_eq!(atoi("99999999999999999999999"), i64::MAX);
        assert_eq!(scan_int("12#rest"), Some((12, "#rest")));
        assert_eq!(scan_int("#"), None);
        assert!(all_digits("") && all_digits("-") && all_digits("-12") && all_digits("0"));
        assert!(!all_digits("1a") && !all_digits("--1") && !all_digits(" 1"));
    }
}
