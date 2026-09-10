//! Positive durations with ISO 8601 and human-readable input.
//!
//! ISO input follows Temporal's duration-string grammar, including combined
//! weeks and a leading `+`. Fractions are allowed on the final time component.
//! Calendar months and years are resolved relative to a UTC date, with the day
//! constrained to the destination month's last day. Days always have 24 hours.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeDelta, Utc};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SECOND: u128 = 1_000_000_000;
const MINUTE: u128 = 60 * SECOND;
const HOUR: u128 = 60 * MINUTE;
const DAY: u128 = 24 * HOUR;
const WEEK: u128 = 7 * DAY;

/// A positive duration, precise to one nanosecond.
/// Parse ISO 8601 (`P1DT2H`, `PT0.5S`) or shorthand (`1d2h`, `500ms`).
/// Shorthand cannot contain whitespace, including leading or trailing whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration {
    months: u32,
    time: StdDuration,
}

/// Invalid syntax, precision, sign, or magnitude in a duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseDurationError(&'static str);

const SYNTAX: ParseDurationError =
    ParseDurationError("expected an ISO 8601 duration (PT1H30M) or a duration with units (1h30m)");
const RANGE: ParseDurationError = ParseDurationError("duration is too large");
const POSITIVE: ParseDurationError = ParseDurationError("duration must be greater than zero");
const PRECISION: ParseDurationError =
    ParseDurationError("duration must be exact to nanoseconds, with at most nine decimal places");

static ISO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^P(?:(\d+)Y)?(?:(\d+)M)?(?:(\d+)W)?(?:(\d+)D)?",
        r"(?:T(?:(\d+(?:[.,]\d{1,9})?)H)?",
        r"(?:(\d+(?:[.,]\d{1,9})?)M)?(?:(\d+(?:[.,]\d{1,9})?)S)?)?$"
    ))
    .expect("valid ISO duration pattern")
});

static HUMAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^([0-9]+(?:[.,][0-9]+)?|[.,][0-9]+)([a-zµμ]+)")
        .expect("valid shorthand duration pattern")
});

#[derive(Default)]
struct Parts {
    months: u32,
    nanos: u128,
}

impl Parts {
    fn calendar(&mut self, number: &str, months: u32) -> Result<(), ParseDurationError> {
        let count = number.parse::<u32>().map_err(|_| {
            ParseDurationError("years and months must be whole numbers within range")
        })?;
        self.months = self
            .months
            .checked_add(count.checked_mul(months).ok_or(RANGE)?)
            .ok_or(RANGE)?;
        Ok(())
    }

    fn fixed(&mut self, number: &str, unit: u128) -> Result<(), ParseDurationError> {
        let (whole, fraction) = number.split_once(['.', ',']).unwrap_or((number, ""));
        if fraction.len() > 9 {
            return Err(PRECISION);
        }
        let whole = if whole.is_empty() {
            0
        } else {
            whole.parse::<u128>().map_err(|_| RANGE)?
        };
        let mut nanos = whole.checked_mul(unit).ok_or(RANGE)?;
        if !fraction.is_empty() {
            let numerator = fraction
                .parse::<u128>()
                .map_err(|_| SYNTAX)?
                .checked_mul(unit)
                .ok_or(RANGE)?;
            let denominator = 10u128.pow(fraction.len() as u32);
            if numerator % denominator != 0 {
                return Err(PRECISION);
            }
            nanos = nanos.checked_add(numerator / denominator).ok_or(RANGE)?;
        }
        self.nanos = self.nanos.checked_add(nanos).ok_or(RANGE)?;
        Ok(())
    }

    fn finish(self) -> Result<Duration, ParseDurationError> {
        if self.months == 0 && self.nanos == 0 {
            return Err(POSITIVE);
        }
        // Leave room for calendar months when converting to std::time::Duration.
        // Chrono's narrower timestamp range is checked separately by deadline().
        if self.nanos / SECOND > i64::MAX as u128 / 1000 {
            return Err(RANGE);
        }
        Ok(Duration {
            months: self.months,
            time: StdDuration::new((self.nanos / SECOND) as u64, (self.nanos % SECOND) as u32),
        })
    }
}

impl Duration {
    fn iso(input: &str) -> Result<Self, ParseDurationError> {
        let captures = ISO.captures(input).ok_or(SYNTAX)?;
        let mut parts = Parts::default();
        let mut any = false;
        let mut time = false;
        let mut fractional = false;
        for index in 1..=7 {
            let Some(component) = captures.get(index) else {
                continue;
            };
            if fractional {
                return Err(ParseDurationError(
                    "only the final ISO time component may have a fraction",
                ));
            }
            let number = component.as_str();
            any = true;
            time |= index >= 5;
            fractional = number.contains(['.', ',']);
            match index {
                1 => parts.calendar(number, 12)?,
                2 => parts.calendar(number, 1)?,
                3 => parts.fixed(number, WEEK)?,
                4 => parts.fixed(number, DAY)?,
                5 => parts.fixed(number, HOUR)?,
                6 => parts.fixed(number, MINUTE)?,
                _ => parts.fixed(number, SECOND)?,
            }
        }
        if !any || (input.contains(['T', 't']) && !time) {
            return Err(SYNTAX);
        }
        parts.finish()
    }

    fn human(mut input: &str) -> Result<Self, ParseDurationError> {
        let mut parts = Parts::default();
        // Bare numbers retain the CLI's conventional interpretation as seconds.
        if input
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b',')
        {
            parts.fixed(input, SECOND)?;
        } else {
            while !input.is_empty() {
                let captures = HUMAN.captures(input).ok_or(SYNTAX)?;
                let number = &captures[1];
                match captures[2].to_ascii_lowercase().as_str() {
                    "y" | "yr" | "yrs" | "year" | "years" => parts.calendar(number, 12)?,
                    "mo" | "mos" | "month" | "months" => parts.calendar(number, 1)?,
                    "w" | "wk" | "wks" | "week" | "weeks" => parts.fixed(number, WEEK)?,
                    "d" | "day" | "days" => parts.fixed(number, DAY)?,
                    "h" | "hr" | "hrs" | "hour" | "hours" => parts.fixed(number, HOUR)?,
                    "m" | "min" | "mins" | "minute" | "minutes" => parts.fixed(number, MINUTE)?,
                    "s" | "sec" | "secs" | "second" | "seconds" => parts.fixed(number, SECOND)?,
                    "ms" | "millisecond" | "milliseconds" => parts.fixed(number, 1_000_000)?,
                    "us" | "µs" | "μs" | "microsecond" | "microseconds" => {
                        parts.fixed(number, 1_000)?;
                    }
                    "ns" | "nanosecond" | "nanoseconds" => parts.fixed(number, 1)?,
                    _ => return Err(SYNTAX),
                }
                input = &input[captures[0].len()..];
                if let Some(rest) = input.strip_prefix(',') {
                    input = rest;
                    if input.is_empty() {
                        return Err(SYNTAX);
                    }
                }
            }
        }
        parts.finish()
    }

    /// Convert to elapsed time when the duration contains no calendar units.
    ///
    /// # Errors
    /// Years and months require a reference date; use [`Self::at`] for those.
    pub fn fixed(self) -> Result<StdDuration, ParseDurationError> {
        if self.months == 0 {
            Ok(self.time)
        } else {
            Err(ParseDurationError(
                "years and months require a reference date; use days or smaller units here",
            ))
        }
    }

    /// Resolve calendar units from `start`, constraining the day to the target
    /// month's last day. UTC days are 24 hours, including across DST changes.
    pub fn at(self, start: DateTime<Utc>) -> StdDuration {
        self.time + StdDuration::from_secs(calendar_days(self.months, start) * 86_400)
    }

    /// The end of the duration, or `None` if it exceeds Chrono's date range.
    pub fn deadline(self, start: DateTime<Utc>) -> Option<DateTime<Utc>> {
        start.checked_add_signed(TimeDelta::from_std(self.at(start)).ok()?)
    }
}

fn calendar_days(months: u32, start: DateTime<Utc>) -> u64 {
    if months == 0 {
        return 0;
    }
    // Gregorian dates repeat every 400 years. Resolve the remainder in a
    // safe year range so even a duration beyond Chrono's maximum date has
    // a finite half-life, without overflowing timestamp arithmetic.
    let date = NaiveDate::from_ymd_opt(
        2000 + start.year().rem_euclid(400),
        start.month(),
        start.day(),
    )
    .expect("the same date exists in an equivalent Gregorian year");
    let end = date
        .checked_add_months(Months::new(months % 4800))
        .expect("remainder stays within years 2000 through 2799");
    u64::from(months / 4800) * 146_097 + (end - date).num_days().unsigned_abs()
}

impl FromStr for Duration {
    type Err = ParseDurationError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let normalized = input.trim();
        let normalized = normalized
            .strip_prefix('+')
            .unwrap_or(normalized)
            .trim_start();
        if normalized.starts_with(['-', '−']) {
            return Err(POSITIVE);
        }
        if normalized.starts_with(['P', 'p']) {
            Self::iso(normalized)
        } else if input.contains(char::is_whitespace) {
            Err(ParseDurationError(
                "shorthand durations cannot contain whitespace",
            ))
        } else {
            Self::human(normalized)
        }
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P")?;
        for (value, unit) in [
            (u64::from(self.months / 12), 'Y'),
            (u64::from(self.months % 12), 'M'),
            (self.time.as_secs() / 86_400, 'D'),
        ] {
            if value != 0 {
                write!(f, "{value}{unit}")?;
            }
        }
        let seconds = self.time.as_secs() % 86_400;
        let nanos = self.time.subsec_nanos();
        if seconds != 0 || nanos != 0 {
            f.write_str("T")?;
            for (value, unit) in [(seconds / 3600, 'H'), (seconds % 3600 / 60, 'M')] {
                if value != 0 {
                    write!(f, "{value}{unit}")?;
                }
            }
            if nanos != 0 {
                let fraction = format!("{nanos:09}");
                write!(f, "{}.{}S", seconds % 60, fraction.trim_end_matches('0'))?;
            } else if !seconds.is_multiple_of(60) {
                write!(f, "{}S", seconds % 60)?;
            }
        }
        Ok(())
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A positive elapsed duration for timeouts and scheduler intervals.
/// Calendar years and months require an anchor and are rejected here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FixedDuration(StdDuration);

impl FixedDuration {
    pub fn as_secs_f64(self) -> f64 {
        self.0.as_secs_f64()
    }
}

impl FromStr for FixedDuration {
    type Err = ParseDurationError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input.parse::<Duration>()?.fixed().map(Self)
    }
}

impl TryFrom<String> for FixedDuration {
    type Error = ParseDurationError;
    fn try_from(input: String) -> Result<Self, Self::Error> {
        input.parse()
    }
}

impl From<FixedDuration> for String {
    fn from(duration: FixedDuration) -> Self {
        duration.to_string()
    }
}

impl fmt::Display for FixedDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Duration {
            months: 0,
            time: self.0,
        }
        .fmt(f)
    }
}

/// A fact's half-life. `Never` disables aging without asserting truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalfLife {
    After(Duration),
    Never,
}

impl Default for HalfLife {
    fn default() -> Self {
        Self::After(Duration {
            months: 0,
            time: StdDuration::from_secs(3 * 86_400),
        })
    }
}

impl HalfLife {
    pub fn seconds_at(self, start: DateTime<Utc>) -> Option<f64> {
        match self {
            Self::Never => None,
            Self::After(duration) => Some(duration.at(start).as_secs_f64()),
        }
    }

    /// `None` means no decay, or a deadline beyond Chrono's date range.
    pub fn deadline(self, start: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Never => None,
            Self::After(duration) => duration.deadline(start),
        }
    }
}

impl FromStr for HalfLife {
    type Err = ParseDurationError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.trim().eq_ignore_ascii_case("never") {
            Ok(Self::Never)
        } else {
            input.parse().map(Self::After)
        }
    }
}

impl fmt::Display for HalfLife {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Never => f.write_str("never"),
            Self::After(duration) => duration.fmt(f),
        }
    }
}

impl Serialize for HalfLife {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HalfLife {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
