//! Time for syncs: a clock that tests can control, timestamps kept as Unix
//! seconds and shown as RFC 3339 in UTC, and sync intervals as people write
//! them (`30s`, `5m`, `1h`).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Where the time comes from. The service reads the system's; tests set it.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The system's wall clock. Syncs are due by it rather than by a monotonic
/// clock, so a laptop that slept catches up when it wakes.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when a test moves it.
#[cfg(test)]
pub struct ManualClock(std::sync::Mutex<SystemTime>);

#[cfg(test)]
impl ManualClock {
    pub fn at(t: Timestamp) -> Self {
        let since_epoch = Duration::from_secs(u64::try_from(t.unix()).unwrap());
        Self(std::sync::Mutex::new(UNIX_EPOCH + since_epoch))
    }
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// A point in time to the second, kept as Unix seconds and shown as RFC 3339
/// in UTC, such as `2026-10-12T08:00:00Z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

/// Seconds in a day.
const DAY: i64 = 86_400;

impl Timestamp {
    pub fn from_unix(seconds: i64) -> Self {
        Self(seconds)
    }
    pub fn unix(self) -> i64 {
        self.0
    }
    /// `t`, to the second.
    pub fn of(t: SystemTime) -> Self {
        let seconds = |d: Duration| i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        match t.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(seconds(after)),
            Err(before) => Self(-seconds(before.duration())),
        }
    }
    /// This time plus `d`, saturating.
    pub fn after(self, d: Duration) -> Self {
        let d = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        Self(self.0.saturating_add(d))
    }
    /// How long after `earlier` this is, or zero if it isn't.
    pub fn since(self, earlier: Self) -> Duration {
        Duration::from_secs(u64::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(0))
    }
    /// A time as [`Display`](fmt::Display) writes it, and nothing else.
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        let shape = b.len() == 20
            && [4, 7].iter().all(|&i| b[i] == b'-')
            && b[10] == b'T'
            && [13, 16].iter().all(|&i| b[i] == b':')
            && b[19] == b'Z';
        if !shape {
            return None;
        }
        let number = |from: usize, to: usize| -> Option<i64> {
            let digits = text.get(from..to)?;
            digits
                .bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| digits.parse().ok())?
        };
        let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
        let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
        let days = days_from_civil(year, month, day);
        if !(1..=12).contains(&month)
            || civil_from_days(days) != (year, month, day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }
        Some(Self(days * DAY + hour * 3600 + minute * 60 + second))
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm, as in C++20's `<chrono>`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date `days` after 1970-01-01, as (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.0.div_euclid(DAY));
        let second = self.0.rem_euclid(DAY);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            second / 3600,
            second / 60 % 60,
            second % 60
        )
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("not a UTC time: {text}")))
    }
}

/// Units of a sync interval, as people write them.
const UNITS: [(&str, u64); 4] = [("s", 1), ("m", 60), ("h", 3600), ("d", 86_400)];

/// A sync interval such as `30s`, `5m`, `1h` or `1d`.
pub fn parse_interval(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let invalid = || {
        format!("'{text}' is not an interval; use a number and a unit, such as 30s, 5m, 1h or 1d")
    };
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let (_, seconds) = UNITS
        .iter()
        .find(|(name, _)| *name == unit)
        .ok_or_else(invalid)?;
    number
        .checked_mul(*seconds)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

/// An interval as [`parse_interval`] reads it, in its largest whole unit.
pub fn format_interval(d: Duration) -> String {
    let seconds = d.as_secs();
    let (name, size) = UNITS
        .iter()
        .rev()
        .find(|(_, size)| seconds.is_multiple_of(*size) && seconds >= *size)
        .unwrap_or(&UNITS[0]);
    format!("{}{name}", seconds / size)
}

/// When `t` is, seen from `now`, as people say it: `in 4 min`, `3 h ago`.
pub fn relative(t: Timestamp, now: Timestamp) -> String {
    let (span, future) = if t >= now {
        (t.since(now).as_secs(), true)
    } else {
        (now.since(t).as_secs(), false)
    };
    let amount = match span {
        0..=59 => {
            return if future {
                "within a minute"
            } else {
                "just now"
            }
            .into()
        }
        60..=7_199 => format!("{} min", span / 60),
        7_200..=172_799 => format!("{} h", span / 3600),
        _ => format!("{} d", span / 86_400),
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_rfc3339() {
        for (unix, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (-1, "1969-12-31T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_791_763_200, "2026-10-12T00:00:00Z"),
            (4_107_587_696, "2100-03-01T12:34:56Z"),
        ] {
            let t = Timestamp::from_unix(unix);
            assert_eq!(t.to_string(), text);
            assert_eq!(Timestamp::parse(text), Some(t), "{text}");
            let json = serde_json::to_value(t).unwrap();
            assert_eq!(json, text);
            assert_eq!(serde_json::from_value::<Timestamp>(json).unwrap(), t);
        }
        let second = Duration::from_secs(1);
        assert_eq!(Timestamp::of(UNIX_EPOCH + second), Timestamp::from_unix(1));
        assert_eq!(Timestamp::of(UNIX_EPOCH - second), Timestamp::from_unix(-1));
        // Every day for a few centuries, both ways.
        for days in (-200 * 366..300 * 366).step_by(7) {
            let t = Timestamp::from_unix(days * DAY + 3723);
            assert_eq!(Timestamp::parse(&t.to_string()), Some(t), "{t}");
        }
        for wrong in [
            "",
            "2026-10-12",
            "2026-10-12T00:00:00",
            "2026-10-12 00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-10-12T24:00:00Z",
            "2026-10-12T00:60:00Z",
            "+026-10-12T00:00:00Z",
            "2026-10-12T00:00:00+00:00",
        ] {
            assert_eq!(Timestamp::parse(wrong), None, "{wrong}");
        }
        assert!(serde_json::from_value::<Timestamp>("yesterday".into()).is_err());
    }

    #[test]
    fn timestamps_count_seconds() {
        let t = Timestamp::from_unix(100);
        assert_eq!(t.after(Duration::from_secs(5)), Timestamp::from_unix(105));
        assert_eq!(t.after(Duration::MAX), Timestamp::from_unix(i64::MAX));
        assert_eq!(t.since(Timestamp::from_unix(40)), Duration::from_secs(60));
        assert_eq!(t.since(Timestamp::from_unix(400)), Duration::ZERO);
    }

    #[test]
    fn relative_times() {
        let now = Timestamp::from_unix(1_791_763_200);
        for (offset, text) in [
            (0, "within a minute"),
            (59, "within a minute"),
            (-59, "just now"),
            (60, "in 1 min"),
            (-300, "5 min ago"),
            (7_199, "in 119 min"),
            (7_200, "in 2 h"),
            (-90_000, "25 h ago"),
            (3 * 86_400, "in 3 d"),
        ] {
            let t = Timestamp::from_unix(now.unix() + offset);
            assert_eq!(relative(t, now), text, "{offset}");
        }
    }

    #[test]
    fn intervals_as_people_write_them() {
        for (text, seconds, shown) in [
            ("30s", 30, "30s"),
            ("90s", 90, "90s"),
            ("120s", 120, "2m"),
            ("5m", 300, "5m"),
            (" 1h ", 3600, "1h"),
            ("2d", 172_800, "2d"),
        ] {
            let d = parse_interval(text).unwrap();
            assert_eq!(d, Duration::from_secs(seconds), "{text}");
            assert_eq!(format_interval(d), shown, "{text}");
        }
        for wrong in [
            "",
            "5",
            "m",
            "5 m",
            "5min",
            "-5m",
            "1.5h",
            "99999999999999999999d",
        ] {
            let error = parse_interval(wrong).unwrap_err();
            assert!(error.contains("such as 30s"), "{wrong}: {error}");
        }
        assert_eq!(format_interval(Duration::ZERO), "0s");
    }
}
