//! Time for syncs: timestamps kept as Unix seconds and shown as RFC 3339 in
//! UTC.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
}
