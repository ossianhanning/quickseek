//! Date filter values for `dm:` / `dc:` / `rc:`:
//! `today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`,
//! `thisyear`, `lastyear`, `last<N><unit>` (rolling, e.g. `last3days`),
//! `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, ranges `a..b`, and `<`, `<=`, `>`, `>=`, `=`.
//!
//! Calendar math is done in local time (minutes since 1970-01-01 local), then
//! converted to UTC FILETIME through [`DateEnv::to_filetime`], so tests can
//! run with a fixed clock and zone.

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Storage::FileSystem::{FileTimeToLocalFileTime, LocalFileTimeToFileTime};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

/// FILETIME ticks per minute and the 1601→1970 offset.
const TICKS_PER_MIN: i64 = 600_000_000;
const EPOCH_1970: i64 = 116_444_736_000_000_000;

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub struct DateEnv {
    /// Current local time, minutes since 1970-01-01 local.
    pub now_local_min: i64,
    /// Local minutes since 1970 → UTC FILETIME.
    pub to_filetime: Box<dyn Fn(i64) -> i64 + Send + Sync>,
}

impl DateEnv {
    /// Uses the system clock and time zone (with DST).
    pub fn system() -> DateEnv {
        let mut now = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut local = now;
        unsafe {
            GetSystemTimeAsFileTime(&mut now);
            FileTimeToLocalFileTime(&now, &mut local);
        }
        let local_ft = ((local.dwHighDateTime as i64) << 32) | local.dwLowDateTime as i64;
        DateEnv {
            now_local_min: (local_ft - EPOCH_1970) / TICKS_PER_MIN,
            to_filetime: Box::new(|m| {
                let lft = m * TICKS_PER_MIN + EPOCH_1970;
                let l = FILETIME {
                    dwLowDateTime: lft as u32,
                    dwHighDateTime: (lft >> 32) as u32,
                };
                let mut u = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                if unsafe { LocalFileTimeToFileTime(&l, &mut u) } == 0 {
                    return lft;
                }
                ((u.dwHighDateTime as i64) << 32) | u.dwLowDateTime as i64
            }),
        }
    }

    /// Fixed clock in UTC (for tests).
    pub fn fixed_utc(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> DateEnv {
        DateEnv {
            now_local_min: days_from_civil(y, m, d) * 1440 + hh * 60 + mm,
            to_filetime: Box::new(|m| m * TICKS_PER_MIN + EPOCH_1970),
        }
    }
}

/// A local-time interval in minutes, half-open.
fn period(v: &str, env: &DateEnv) -> Option<(i64, i64)> {
    let v = v.trim().to_ascii_lowercase();
    let today = env.now_local_min.div_euclid(1440);
    let day = |d: i64| (d * 1440, (d + 1) * 1440);
    let (y, m, _) = civil_from_days(today);
    let month = |y: i64, m: u32| {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        (
            days_from_civil(y, m, 1) * 1440,
            days_from_civil(ny, nm, 1) * 1440,
        )
    };
    let year = |y: i64| {
        (
            days_from_civil(y, 1, 1) * 1440,
            days_from_civil(y + 1, 1, 1) * 1440,
        )
    };
    let monday = today - (today + 3).rem_euclid(7);
    Some(match v.as_str() {
        "today" => day(today),
        "yesterday" => day(today - 1),
        "thisweek" => (monday * 1440, (monday + 7) * 1440),
        "lastweek" => ((monday - 7) * 1440, monday * 1440),
        "thismonth" => month(y, m),
        "lastmonth" => {
            if m == 1 {
                month(y - 1, 12)
            } else {
                month(y, m - 1)
            }
        }
        "thisyear" => year(y),
        "lastyear" => year(y - 1),
        _ => {
            if let Some(rest) = v.strip_prefix("last").or_else(|| v.strip_prefix("past")) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                let n: i64 = digits.parse().ok()?;
                let unit = rest[digits.len()..].trim_end_matches('s');
                let mins = match unit {
                    "minute" | "min" => n,
                    "hour" => n * 60,
                    "day" => n * 1440,
                    "week" => n * 7 * 1440,
                    "month" => n * 30 * 1440,
                    "year" => n * 365 * 1440,
                    _ => return None,
                };
                return Some((env.now_local_min - mins, i64::MAX / 4));
            }
            let parts: Vec<&str> = v.split('-').collect();
            let num = |s: &str| s.parse::<i64>().ok();
            match parts.as_slice() {
                [yy] if yy.len() == 4 => year(num(yy)?),
                [yy, mm] if yy.len() == 4 => {
                    let mm = num(mm)? as u32;
                    if !(1..=12).contains(&mm) {
                        return None;
                    }
                    month(num(yy)?, mm)
                }
                [yy, mm, dd] if yy.len() == 4 => {
                    let (mm, dd) = (num(mm)? as u32, num(dd)? as u32);
                    if !(1..=12).contains(&mm) || !(1..=31).contains(&dd) {
                        return None;
                    }
                    day(days_from_civil(num(yy)?, mm, dd))
                }
                _ => return None,
            }
        }
    })
}

/// Parses a date filter value to a half-open UTC FILETIME interval.
pub fn parse_date_filter(v: &str, env: &DateEnv) -> Option<(i64, i64)> {
    const OPEN_HI: i64 = i64::MAX;
    const OPEN_LO: i64 = i64::MIN;
    let ft = |m: i64| {
        if m >= i64::MAX / 8 {
            OPEN_HI
        } else {
            (env.to_filetime)(m)
        }
    };
    let (lo, hi) = if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() {
            OPEN_LO
        } else {
            ft(period(a, env)?.0)
        };
        let hi = if b.is_empty() {
            OPEN_HI
        } else {
            ft(period(b, env)?.1)
        };
        (lo, hi)
    } else if let Some(x) = v.strip_prefix(">=") {
        (ft(period(x, env)?.0), OPEN_HI)
    } else if let Some(x) = v.strip_prefix("<=") {
        (OPEN_LO, ft(period(x, env)?.1))
    } else if let Some(x) = v.strip_prefix('>') {
        (ft(period(x, env)?.1), OPEN_HI)
    } else if let Some(x) = v.strip_prefix('<') {
        (OPEN_LO, ft(period(x, env)?.0))
    } else {
        let (a, b) = period(v.strip_prefix('=').unwrap_or(v), env)?;
        (ft(a), ft(b))
    };
    Some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ft(y: i64, m: u32, d: u32) -> i64 {
        days_from_civil(y, m, d) * 1440 * TICKS_PER_MIN + EPOCH_1970
    }

    #[test]
    fn civil_roundtrip() {
        for z in [-800_000i64, -1, 0, 1, 19_000, 20_745, 2_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn periods() {
        // Tuesday 2026-10-20 14:30.
        let env = DateEnv::fixed_utc(2026, 10, 20, 14, 30);
        let p = |s| parse_date_filter(s, &env).unwrap();
        assert_eq!(p("today"), (ft(2026, 10, 20), ft(2026, 10, 21)));
        assert_eq!(p("yesterday"), (ft(2026, 10, 19), ft(2026, 10, 20)));
        assert_eq!(p("thisweek"), (ft(2026, 10, 19), ft(2026, 10, 26)));
        assert_eq!(p("lastweek"), (ft(2026, 10, 12), ft(2026, 10, 19)));
        assert_eq!(p("thismonth"), (ft(2026, 10, 1), ft(2026, 11, 1)));
        assert_eq!(p("lastmonth"), (ft(2026, 9, 1), ft(2026, 10, 1)));
        assert_eq!(p("lastyear"), (ft(2025, 1, 1), ft(2026, 1, 1)));
        assert_eq!(p("2024"), (ft(2024, 1, 1), ft(2025, 1, 1)));
        assert_eq!(p("2024-02"), (ft(2024, 2, 1), ft(2024, 3, 1)));
        assert_eq!(p("2024-02-29"), (ft(2024, 2, 29), ft(2024, 3, 1)));
        assert_eq!(p("2026-01..2026-06"), (ft(2026, 1, 1), ft(2026, 7, 1)));
        assert_eq!(p(">2026-10-19"), (ft(2026, 10, 20), i64::MAX));
        assert_eq!(p("<2026"), (i64::MIN, ft(2026, 1, 1)));
        assert_eq!(p("<=2026"), (i64::MIN, ft(2027, 1, 1)));
        let now = ft(2026, 10, 20) + (14 * 60 + 30) * TICKS_PER_MIN;
        assert_eq!(p("last3days"), (now - 3 * 1440 * TICKS_PER_MIN, i64::MAX));
        assert_eq!(p("last2hours").0, now - 120 * TICKS_PER_MIN);
        // January → last month is December of the previous year.
        let jan = DateEnv::fixed_utc(2026, 1, 5, 0, 0);
        assert_eq!(
            parse_date_filter("lastmonth", &jan).unwrap(),
            (ft(2025, 12, 1), ft(2026, 1, 1))
        );
        assert!(parse_date_filter("2026-13", &env).is_none());
        assert!(parse_date_filter("soon", &env).is_none());
    }
}
