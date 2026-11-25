//! Local time conversion for log timestamps.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Broken-down local time with its UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTm {
    /// Full year.
    pub year: i64,
    /// Month 1..=12.
    pub month: u32,
    /// Day of month.
    pub day: u32,
    /// Hour.
    pub hour: u32,
    /// Minute.
    pub min: u32,
    /// Second.
    pub sec: u32,
    /// Offset east of UTC in seconds.
    pub gmtoff: i64,
}

impl LocalTm {
    /// Convert a Unix timestamp to local time (`localtime_r`).
    pub fn from_unix(t: i64) -> Self {
        #[cfg(unix)]
        {
            // SAFETY: `tm` is a plain C struct that localtime_r fully
            // initializes on success; we pass valid pointers to locals.
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            let tt: libc::time_t = t as libc::time_t;
            // SAFETY: both pointers reference live stack locals.
            let ok = unsafe { !libc::localtime_r(&raw const tt, &raw mut tm).is_null() };
            if ok {
                return Self {
                    year: i64::from(tm.tm_year) + 1900,
                    month: (tm.tm_mon + 1) as u32,
                    day: tm.tm_mday as u32,
                    hour: tm.tm_hour as u32,
                    min: tm.tm_min as u32,
                    sec: tm.tm_sec as u32,
                    gmtoff: tm.tm_gmtoff,
                };
            }
        }
        let g = crate::httptime::gmtime(t);
        Self {
            year: g.year,
            month: g.month,
            day: g.day,
            hour: g.hour,
            min: g.min,
            sec: g.sec,
            gmtoff: 0,
        }
    }

    /// Current local time and the sub-second microseconds.
    pub fn now() -> (Self, u32) {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        (Self::from_unix(d.as_secs() as i64), d.subsec_micros())
    }
}

/// Format `dd/Mon/yyyy:hh:mm:ss`, optionally followed by ` +hhmm` as in the
/// Common Log Format.
pub fn format_clf_time(tm: &LocalTm, with_offset: bool) -> String {
    let mut s = format!(
        "{:02}/{}/{}:{:02}:{:02}:{:02}",
        tm.day,
        MONTHS[(tm.month.clamp(1, 12) - 1) as usize],
        tm.year,
        tm.hour,
        tm.min,
        tm.sec
    );
    if with_offset {
        let sign = if tm.gmtoff < 0 { '-' } else { '+' };
        let off = tm.gmtoff.unsigned_abs();
        let _ = write!(s, " {sign}{:02}{:02}", off / 3600, (off % 3600) / 60);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clf_format() {
        let tm = LocalTm {
            year: 2000,
            month: 1,
            day: 2,
            hour: 3,
            min: 4,
            sec: 5,
            gmtoff: -28_800,
        };
        assert_eq!(format_clf_time(&tm, false), "02/Jan/2000:03:04:05");
        assert_eq!(format_clf_time(&tm, true), "02/Jan/2000:03:04:05 -0800");
        let tm = LocalTm {
            gmtoff: 19_800,
            ..tm
        };
        assert_eq!(format_clf_time(&tm, true), "02/Jan/2000:03:04:05 +0530");
    }

    #[test]
    fn conversion_is_consistent_with_offset() {
        let t = 1_700_000_000;
        let l = LocalTm::from_unix(t);
        let g = crate::httptime::gmtime(t + l.gmtoff);
        assert_eq!(
            (l.year, l.month, l.day, l.hour, l.min),
            (g.year, g.month, g.day, g.hour, g.min)
        );
        let (_, us) = LocalTm::now();
        assert!(us < 1_000_000);
    }
}
