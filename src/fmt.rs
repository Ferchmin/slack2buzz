//! Small formatting helpers for operator-facing output.
//!
//! Dates are formatted here rather than pulled from a calendar crate: the only
//! requirement is a `YYYY-MM-DD` label in probe output and the picker, and a
//! date library is a lot of dependency for that. The conversion is Hinnant's
//! `civil_from_days`, which is exact for the whole proleptic Gregorian range.

/// Format unix seconds as `YYYY-MM-DD` (UTC).
pub fn date(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Format unix seconds as `YYYY-MM-DD HH:MM` (UTC).
pub fn datetime(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (h, min) = (secs_of_day / 3600, (secs_of_day % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}")
}

/// Days since 1970-01-01 → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Pluralise a count for log lines: `1 message`, `2 messages`.
///
/// Handles the few irregular words this tool actually uses rather than
/// pretending English is regular; anything else gets a bare `s`.
pub fn plural(n: usize, singular: &str) -> String {
    if n == 1 {
        return format!("{n} {singular}");
    }
    let plural = match singular {
        "person" => "people".to_string(),
        _ => format!("{singular}s"),
    };
    format!("{n} {plural}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(date(0), "1970-01-01");
    }

    #[test]
    fn fixture_timestamps_render_as_expected() {
        // The dates the fixture export claims to cover.
        assert_eq!(date(1_709_545_200), "2024-03-04");
        assert_eq!(date(1_709_631_700), "2024-03-05");
    }

    #[test]
    fn leap_day_is_handled() {
        // 2024-02-29T00:00:00Z
        assert_eq!(date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn century_non_leap_year_is_handled() {
        // 1900 was not a leap year: 1900-03-01, not 1900-02-29.
        let secs = -2_203_891_200;
        assert_eq!(date(secs), "1900-03-01");
    }

    #[test]
    fn pre_epoch_dates_do_not_wrap() {
        assert_eq!(date(-86_400), "1969-12-31");
        assert_eq!(datetime(-1), "1969-12-31 23:59");
    }

    #[test]
    fn datetime_includes_utc_time_of_day() {
        assert_eq!(datetime(0), "1970-01-01 00:00");
        assert_eq!(datetime(1_709_545_200), "2024-03-04 09:40");
    }

    #[test]
    fn irregular_plurals_are_not_mangled() {
        assert_eq!(plural(1, "person"), "1 person");
        assert_eq!(plural(3, "person"), "3 people");
    }

    #[test]
    fn plural_only_pluralises_when_needed() {
        assert_eq!(plural(1, "message"), "1 message");
        assert_eq!(plural(0, "message"), "0 messages");
        assert_eq!(plural(2, "message"), "2 messages");
    }
}
