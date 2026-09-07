use std::time::{SystemTime, UNIX_EPOCH};

use crate::value::Nanos;

pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
pub const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
pub const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;
pub const NANOS_PER_DAY: i64 = 24 * NANOS_PER_HOUR;
pub const NANOS_PER_WEEK: i64 = 7 * NANOS_PER_DAY;

pub fn now() -> Nanos {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Nanos(i64::try_from(since_epoch.as_nanos()).unwrap_or(i64::MAX))
}

pub fn from_parts(seconds: i64, nanoseconds: u32) -> Nanos {
    Nanos(
        seconds
            .saturating_mul(NANOS_PER_SECOND)
            .saturating_add(i64::from(nanoseconds)),
    )
}

pub fn parse_iso(text: &str) -> Option<Nanos> {
    let text = text.trim();
    let (date, rest) = match text.find(['T', ' ']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let mut nanos = days.checked_mul(NANOS_PER_DAY)?;
    let Some(rest) = rest else {
        return Some(Nanos(nanos));
    };
    let (clock, offset) = split_offset(rest);
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second_text = clock_parts.next().unwrap_or("0");
    if clock_parts.next().is_some() || hour > 23 || minute > 59 {
        return None;
    }
    let (whole, fraction) = match second_text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (second_text, ""),
    };
    let second: i64 = whole.parse().ok()?;
    if second > 60 || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fraction_nanos: i64 = if fraction.is_empty() {
        0
    } else {
        let padded = format!("{fraction:0<9}");
        padded.parse().ok()?
    };
    nanos = nanos
        .checked_add(hour * NANOS_PER_HOUR)?
        .checked_add(minute * NANOS_PER_MINUTE)?
        .checked_add(second * NANOS_PER_SECOND)?
        .checked_add(fraction_nanos)?;
    nanos = nanos.checked_sub(offset?)?;
    Some(Nanos(nanos))
}

fn split_offset(rest: &str) -> (&str, Option<i64>) {
    if let Some(clock) = rest.strip_suffix('Z') {
        return (clock, Some(0));
    }
    let Some(index) = rest.rfind(['+', '-']) else {
        return (rest, Some(0));
    };
    let (clock, offset) = rest.split_at(index);
    let sign: i64 = if offset.starts_with('-') { -1 } else { 1 };
    let digits = &offset[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (h, m),
        None if digits.len() == 4 => digits.split_at(2),
        None => (digits, "0"),
    };
    let parsed = hours
        .parse::<i64>()
        .ok()
        .zip(minutes.parse::<i64>().ok())
        .map(|(h, m)| sign * (h * NANOS_PER_HOUR + m * NANOS_PER_MINUTE));
    (clock, parsed)
}

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day = i64::from(day);
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

pub fn format_iso(nanos: Nanos) -> String {
    let days = nanos.0.div_euclid(NANOS_PER_DAY);
    let rem = nanos.0.rem_euclid(NANOS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / NANOS_PER_HOUR;
    let minute = (rem % NANOS_PER_HOUR) / NANOS_PER_MINUTE;
    let second = (rem % NANOS_PER_MINUTE) / NANOS_PER_SECOND;
    let fraction = rem % NANOS_PER_SECOND;
    if fraction == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:09}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_round_trips_across_leap_years() {
        for days in [-1_000_000, -1, 0, 59, 60, 365, 10_957, 20_000, 1_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn parses_date_only_as_midnight_utc() {
        assert_eq!(parse_iso("2026-07-30"), Some(Nanos(20_664 * NANOS_PER_DAY)));
    }

    #[test]
    fn parses_datetime_with_zulu_and_fraction() {
        let expected =
            20_664 * NANOS_PER_DAY + 13 * NANOS_PER_HOUR + 47 * NANOS_PER_MINUTE + 500_000_000;
        assert_eq!(parse_iso("2026-07-30T13:47:00.5Z"), Some(Nanos(expected)));
        assert_eq!(parse_iso("2026-07-30 13:47:00.5"), Some(Nanos(expected)));
    }

    #[test]
    fn applies_numeric_offsets() {
        let utc = parse_iso("2026-07-30T12:00:00Z").expect("utc");
        assert_eq!(parse_iso("2026-07-30T14:00:00+02:00"), Some(utc));
        assert_eq!(parse_iso("2026-07-30T14:00:00+0200"), Some(utc));
        assert_eq!(parse_iso("2026-07-30T10:00:00-02:00"), Some(utc));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso("yesterday"), None);
        assert_eq!(parse_iso("0001-01-01"), None);
        assert_eq!(parse_iso("3000-01-01"), None);
        assert_eq!(parse_iso("2026-13-01"), None);
        assert_eq!(parse_iso("2026-07-30T25:00:00"), None);
        assert_eq!(parse_iso("2026-07-30T12:00:00.1234567890"), None);
    }

    #[test]
    fn formats_round_trip() {
        for text in [
            "2026-07-30T13:47:00Z",
            "1969-12-31T23:59:59.000000001Z",
            "1900-01-01T00:00:00Z",
        ] {
            let nanos = parse_iso(text).expect(text);
            assert_eq!(format_iso(nanos), text);
        }
    }
}
