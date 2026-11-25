//! HTTP date formatting and parsing (`nsd/httptime.c`).
//!
//! Formats RFC 1123 dates (`Sun, 06 Nov 1994 08:49:37 GMT`) and parses the
//! three formats HTTP/1.1 requires recipients to accept: RFC 1123, RFC 850
//! (`Sunday, 06-Nov-94 08:49:37 GMT`) and ANSI C `asctime()`
//! (`Sun Nov  6 08:49:37 1994`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Broken-down UTC time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tm {
    /// Full year, e.g. 1994.
    pub year: i64,
    /// Month 1..=12.
    pub month: u32,
    /// Day of month 1..=31.
    pub day: u32,
    /// Hour 0..=23.
    pub hour: u32,
    /// Minute 0..=59.
    pub min: u32,
    /// Second 0..=60.
    pub sec: u32,
    /// Day of week, 0 = Sunday.
    pub wday: u32,
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Break a Unix timestamp into UTC fields (`gmtime`).
pub fn gmtime(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Tm {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        min: ((secs % 3600) / 60) as u32,
        sec: (secs % 60) as u32,
        wday: (days + 4).rem_euclid(7) as u32,
    }
}

/// Inverse of [`gmtime`] (`timegm`).
pub fn timegm(tm: &Tm) -> i64 {
    days_from_civil(tm.year, tm.month, tm.day) * 86_400
        + i64::from(tm.hour) * 3600
        + i64::from(tm.min) * 60
        + i64::from(tm.sec)
}

/// Seconds since the epoch for a `SystemTime` (negative before 1970).
pub fn unix_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// `SystemTime` from seconds since the epoch.
pub fn system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// Current Unix time in seconds.
pub fn now() -> i64 {
    unix_time(SystemTime::now())
}

/// Format a timestamp as an RFC 1123 HTTP date (`Ns_HttpTime`).
pub fn http_time(t: i64) -> String {
    let tm = gmtime(t);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[tm.wday as usize],
        tm.day,
        MONTHS[(tm.month - 1) as usize],
        tm.year,
        tm.hour,
        tm.min,
        tm.sec
    )
}

fn month_index(s: &str) -> Option<u32> {
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(s))
        .map(|i| i as u32 + 1)
}

fn parse_hms(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.split(':');
    let h = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let sec = it.next()?.parse().ok()?;
    if it.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some((h, m, sec))
}

fn make(year: i64, month: u32, day: u32, hms: (u32, u32, u32)) -> Option<i64> {
    if !(1..=31).contains(&day) {
        return None;
    }
    let tm = Tm {
        year,
        month,
        day,
        hour: hms.0,
        min: hms.1,
        sec: hms.2,
        wday: 0,
    };
    Some(timegm(&tm))
}

/// Parse an HTTP date in any of the three standard formats
/// (`Ns_ParseHttpTime`).  Returns `None` if unparseable.
pub fn parse_http_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some((_, rest)) = s.split_once(',') {
        let rest = rest.trim();
        // RFC 850: "06-Nov-94 08:49:37 GMT"
        if rest.as_bytes().get(2) == Some(&b'-') {
            let mut parts = rest.split_whitespace();
            let date = parts.next()?;
            let time = parts.next()?;
            let mut d = date.split('-');
            let day = d.next()?.parse().ok()?;
            let month = month_index(d.next()?)?;
            let yy: i64 = d.next()?.parse().ok()?;
            let year = if yy < 100 {
                if yy < 70 {
                    2000 + yy
                } else {
                    1900 + yy
                }
            } else {
                yy
            };
            return make(year, month, day, parse_hms(time)?);
        }
        // RFC 1123: "06 Nov 1994 08:49:37 GMT"
        let mut parts = rest.split_whitespace();
        let day = parts.next()?.parse().ok()?;
        let month = month_index(parts.next()?)?;
        let year = parts.next()?.parse().ok()?;
        let hms = parse_hms(parts.next()?)?;
        return make(year, month, day, hms);
    }
    // asctime: "Sun Nov  6 08:49:37 1994"
    let mut parts = s.split_whitespace();
    let _wday = parts.next()?;
    let month = month_index(parts.next()?)?;
    let day = parts.next()?.parse().ok()?;
    let hms = parse_hms(parts.next()?)?;
    let year = parts.next()?.parse().ok()?;
    make(year, month, day, hms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const T: i64 = 784_111_777; // Sun, 06 Nov 1994 08:49:37 GMT

    #[test]
    fn formats_rfc1123() {
        assert_eq!(http_time(T), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_time(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn parses_all_three_formats() {
        assert_eq!(parse_http_time("Sun, 06 Nov 1994 08:49:37 GMT"), Some(T));
        assert_eq!(parse_http_time("Sunday, 06-Nov-94 08:49:37 GMT"), Some(T));
        assert_eq!(parse_http_time("Sun Nov  6 08:49:37 1994"), Some(T));
        assert_eq!(
            parse_http_time("Thu, 01 Jan 2032 00:00:00 GMT"),
            Some(1_956_528_000)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_http_time(""), None);
        assert_eq!(parse_http_time("yesterday"), None);
        assert_eq!(parse_http_time("Sun, 06 Foo 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_time("Sun, 06 Nov 1994 25:49:37 GMT"), None);
    }

    #[test]
    fn system_time_roundtrip() {
        assert_eq!(unix_time(system_time(T)), T);
        assert_eq!(unix_time(system_time(-5)), -5);
        assert!(now() > T);
    }

    proptest! {
        #[test]
        fn roundtrip(t in 0i64..4_102_444_800) {
            prop_assert_eq!(parse_http_time(&http_time(t)), Some(t));
            prop_assert_eq!(timegm(&gmtime(t)), t);
        }
    }
}
