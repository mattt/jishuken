use super::*;
use chrono::TimeZone;

fn date(input: &str) -> DateTime<Utc> {
    input.parse().unwrap()
}

#[test]
fn iso_examples_from_temporal() {
    let start = date("2024-01-01T00:00:00Z");
    for (input, end) in [
        ("P1Y1M1DT1H1M1.1S", "2025-02-02T01:01:01.1Z"),
        ("P40D", "2024-02-10T00:00:00Z"),
        ("P1Y1D", "2025-01-02T00:00:00Z"),
        ("P3DT4H59M", "2024-01-04T04:59:00Z"),
        ("PT2H30M", "2024-01-01T02:30:00Z"),
        ("P1M", "2024-02-01T00:00:00Z"),
        ("PT1M", "2024-01-01T00:01:00Z"),
        ("PT0.0021S", "2024-01-01T00:00:00.0021Z"),
        (
            "P1Y2M3W4DT5H6M7.987654321S",
            "2025-03-26T05:06:07.987654321Z",
        ),
    ] {
        assert_eq!(
            input.parse::<Duration>().unwrap().deadline(start),
            Some(date(end)),
            "{input}"
        );
    }
}

#[test]
fn iso_extensions_and_leniency() {
    for (input, equivalent) in [
        ("P3W1D", "P22D"),
        ("P1W2DT3H", "P9DT3H"),
        ("+P1M", "P1M"),
        (" \t+pt1h30m\n", "PT90M"),
        ("PT1,5H", "PT1H30M"),
        ("PT1.5M", "PT90S"),
        ("PT0.000000001H", "PT0.0000036S"),
        ("PT0,000000001M", "PT0.00000006S"),
        ("P0001DT0002H", "P1DT2H"),
        ("P0Y0M0W0DT0H0M1S", "PT1S"),
        ("PT100H100M100S", "P4DT5H41M40S"),
    ] {
        assert_eq!(
            input.parse::<Duration>(),
            equivalent.parse::<Duration>(),
            "{input}"
        );
        assert!(input.parse::<Duration>().is_ok(), "{input}");
    }
}

#[test]
fn shorthand_units_and_compounds() {
    for (input, iso) in [
        ("15m", "PT15M"),
        ("2w", "P14D"),
        ("90d", "P90D"),
        ("1h30m", "PT1H30M"),
        ("1h,30m", "PT1H30M"),
        ("1 hour, 30 minutes", "PT1H30M"),
        ("1 hour 30 minutes", "PT1H30M"),
        ("1 YEAR 2 months 3 weeks 4 days", "P1Y2M25D"),
        ("1yr 2mos 3wks 4d 5hrs 6mins 7secs", "P1Y2M25DT5H6M7S"),
        ("1y 1mo", "P1Y1M"),
        ("1m", "PT1M"),
        ("1.5d", "P1DT12H"),
        (".5w", "P3DT12H"),
        ("+ 2 h", "PT2H"),
        ("  +2 h \t30 min  ", "PT2H30M"),
        ("1,5 seconds", "PT1.5S"),
        ("1ms", "PT0.001S"),
        ("1 microsecond", "PT0.000001S"),
        ("1us 2µs 3μs", "PT0.000006S"),
        ("1ns", "PT0.000000001S"),
        ("0.1ms", "PT0.0001S"),
        ("1 nanosecond", "PT0.000000001S"),
        (
            "1 millisecond 2 microseconds 3 nanoseconds",
            "PT0.001002003S",
        ),
        ("60", "PT1M"),
        ("0.5", "PT0.5S"),
    ] {
        assert_eq!(
            input.parse::<Duration>().unwrap(),
            iso.parse::<Duration>().unwrap(),
            "{input}"
        );
    }
}

#[test]
fn rejects_invalid_iso_syntax() {
    for input in [
        "",
        " ",
        "P",
        "PT",
        "P1DT",
        "P1D1H",
        "P1H",
        "P1S",
        "P1MT",
        "P1D1Y",
        "P1M1Y",
        "P1W1M",
        "P1D1W",
        "PT1S1M",
        "PT1M1H",
        "P1Y1Y",
        "P1M1M",
        "P1W1W",
        "P1D1D",
        "PT1H1H",
        "PT1M1M",
        "PT1S1S",
        "P1DTT1H",
        "PTT1H",
        "P1Q",
        "PT1MS",
        "P1D trailing",
        "PT1S!",
        "P 1D",
        "P1 D",
        "PT1H 30M",
        "P1.5Y",
        "P1.5M",
        "P1.5W",
        "P1.5D",
        "PT.5S",
        "PT1.S",
        "PT1,S",
        "PT1.5.5S",
        "PT1,5.5S",
        "PT1e3S",
        "PT1.5H1M",
        "PT1.5H0S",
        "PT1.0M0S",
        "PT1M1.5H",
        "PT1.1234567890S",
        "P2024-01-01",
        "P0001-02-03T04:05:06",
        "Ｐ１Ｄ",
        "P١D",
        "PTNaNS",
    ] {
        assert!(input.parse::<Duration>().is_err(), "accepted {input:?}");
    }
}

#[test]
fn rejects_invalid_shorthand_and_nonpositive_values() {
    for input in [
        "0",
        "0d",
        "PT0S",
        "P0Y0M0W0DT0H0M0S",
        "0h 0m",
        "+P0D",
        "-P1D",
        "−P1D",
        "-1h",
        "PT-1H",
        "PT+1H",
        "1h-30m",
        "1h +30m",
        "--1s",
        "++1s",
        "NaN",
        "NaNs",
        "inf",
        "infinity",
        "∞",
        "1e3s",
        "1 fortnight",
        "one day",
        "1month2",
        "1h and 30m",
        "1h,",
        "1h junk",
        "1.5months",
        "0.5yr",
        "0.1ns",
        "0.0001us",
        "0.0000000001s",
        "never",
        "immutable",
        "days",
        "hours",
        "slow",
        "1s\0",
        "1,2,3s",
    ] {
        assert!(input.parse::<Duration>().is_err(), "accepted {input:?}");
    }
}

#[test]
fn checked_arithmetic_rejects_overflow() {
    for input in [
        "P4294967296M",
        "P4294967295Y",
        "P357913941Y4M",
        "PT18446744073709551615S",
        "9999999999999999999999999999999999999999999999ns",
        "340282366920938463463374607431768211455h",
        "9223372036854775s 1s",
    ] {
        assert!(input.parse::<Duration>().is_err(), "accepted {input:?}");
    }
}

#[test]
fn nanoseconds_are_not_rounded_through_floating_point() {
    let duration: Duration = "PT16777217.123456789S".parse().unwrap();
    assert_eq!(
        duration.fixed().unwrap(),
        StdDuration::new(16_777_217, 123_456_789)
    );
    assert_eq!(
        "PT0.000000001S"
            .parse::<Duration>()
            .unwrap()
            .fixed()
            .unwrap(),
        StdDuration::from_nanos(1)
    );
}

#[test]
fn calendar_arithmetic_handles_leap_years_and_month_ends() {
    for (start, duration, end) in [
        ("2024-01-31T12:00:00Z", "P1M", "2024-02-29T12:00:00Z"),
        ("2023-01-31T12:00:00Z", "P1M", "2023-02-28T12:00:00Z"),
        ("2024-02-29T12:00:00Z", "P1Y", "2025-02-28T12:00:00Z"),
        ("2024-02-29T12:00:00Z", "P1Y1M", "2025-03-29T12:00:00Z"),
        ("2023-12-31T23:00:00Z", "P2M1DT2H", "2024-03-02T01:00:00Z"),
        ("1999-03-01T00:00:00Z", "P1Y", "2000-03-01T00:00:00Z"),
        ("2099-03-01T00:00:00Z", "P1Y", "2100-03-01T00:00:00Z"),
        ("2024-02-29T00:00:00Z", "P400Y", "2424-02-29T00:00:00Z"),
        ("2024-01-31T00:00:00Z", "P400Y1M", "2424-02-29T00:00:00Z"),
    ] {
        assert_eq!(
            duration.parse::<Duration>().unwrap().deadline(date(start)),
            Some(date(end)),
            "{start} + {duration}"
        );
    }
}

#[test]
fn calendar_resolution_matches_chrono_across_gregorian_cycle() {
    for year in 1800..=2200 {
        for month in 1..=12 {
            let start = Utc.with_ymd_and_hms(year, month, 28, 12, 34, 56).unwrap();
            for months in [1, 2, 11, 12, 13, 4799, 4800, 4801, 9601] {
                let duration = Duration {
                    months,
                    time: StdDuration::ZERO,
                };
                let expected = start.checked_add_months(Months::new(months)).unwrap();
                assert_eq!(duration.deadline(start), Some(expected));
            }
        }
    }
}

#[test]
fn huge_calendar_durations_and_date_boundaries_do_not_panic() {
    for input in ["P4294967295M", "P357913941Y", "PT9223372036854775S"] {
        let duration: Duration = input.parse().unwrap();
        let start = date("2024-01-01T00:00:00Z");
        assert!(duration.at(start).as_secs_f64().is_finite());
        assert!(duration.deadline(start).is_none());
    }
    let month: Duration = "P1M".parse().unwrap();
    assert!(month.deadline(DateTime::<Utc>::MAX_UTC).is_none());
    assert!(month.deadline(DateTime::<Utc>::MIN_UTC).is_some());
}

#[test]
fn canonical_output_and_serde_roundtrip() {
    for (input, canonical) in [
        ("2w", "P14D"),
        ("12mo", "P1Y"),
        ("25mo", "P2Y1M"),
        ("1h30m", "PT1H30M"),
        ("24h", "P1D"),
        ("61s", "PT1M1S"),
        ("1ms", "PT0.001S"),
        ("1ns", "PT0.000000001S"),
        ("P1Y2M3W4DT5H6M7.987654321S", "P1Y2M25DT5H6M7.987654321S"),
    ] {
        let duration: Duration = input.parse().unwrap();
        assert_eq!(duration.to_string(), canonical);
        assert_eq!(canonical.parse::<Duration>().unwrap(), duration);
        let json = serde_json::to_string(&duration).unwrap();
        assert_eq!(serde_json::from_str::<Duration>(&json).unwrap(), duration);
    }
    for nanos in [1, 9, 10, 100, 999, 1_000, 123_456_789, 999_999_999] {
        for seconds in [0, 1, 59, 60, 3600, 86_400, 16_777_217] {
            let duration = Duration {
                months: 25,
                time: StdDuration::new(seconds, nanos),
            };
            assert_eq!(duration.to_string().parse::<Duration>().unwrap(), duration);
        }
    }
}

#[test]
fn never_is_explicit_and_independent_of_duration_parsing() {
    for input in ["never", "NEVER", " never "] {
        let half_life: HalfLife = input.parse().unwrap();
        assert_eq!(half_life, HalfLife::Never);
        assert_eq!(half_life.to_string(), "never");
        assert_eq!(half_life.seconds_at(Utc::now()), None);
        assert_eq!(half_life.deadline(Utc::now()), None);
        assert_eq!(
            serde_json::from_str::<HalfLife>("\"never\"").unwrap(),
            half_life
        );
    }
    for input in ["0s", "nonsense", "immutable", "-P1D"] {
        assert!(input.parse::<HalfLife>().is_err());
        assert!(serde_json::from_str::<HalfLife>(&format!("{input:?}")).is_err());
    }
    assert_eq!(HalfLife::default().to_string(), "P3D");
}

#[test]
fn elapsed_durations_reject_unanchored_calendar_units() {
    for input in ["P1M", "P1Y", "P1Y2DT3H", "never", "0s"] {
        assert!(input.parse::<FixedDuration>().is_err(), "{input}");
    }
    for input in ["P1W", "7d", "PT168H"] {
        assert_eq!(
            input.parse::<FixedDuration>().unwrap().as_secs_f64(),
            604_800.0
        );
    }
}
