//! Reads the times that storage records, such as `2026-10-21T14:05:09.123Z`,
//! for clients that want them as system times.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Reads an ISO 8601 time in UTC, with or without milliseconds. Returns
/// `None` for anything else, or for a time before 1970.
pub(crate) fn parse(text: &str) -> Option<SystemTime> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    let (time, millis) = match time.split_once('.') {
        Some((time, fraction)) if fraction.len() == 3 => (time, fraction.parse().ok()?),
        Some(_) => return None,
        None => (time, 0),
    };
    let mut time = time.split(':');
    let hour: u64 = time.next()?.parse().ok()?;
    let minute: u64 = time.next()?.parse().ok()?;
    let second: u64 = time.next()?.parse().ok()?;
    if date.next().is_some()
        || time.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = u64::try_from(days_since_1970(year, month, day)).ok()?;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis))
}

/// Days from January 1, 1970 to the given date, using Howard Hinnant's
/// `days_from_civil` algorithm.
fn days_since_1970(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(text: &str) -> Option<u64> {
        parse(text).map(|time| time.duration_since(UNIX_EPOCH).unwrap().as_secs())
    }

    #[test]
    fn reads_storage_times() {
        assert_eq!(secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(secs("2000-03-01T00:00:00.000Z"), Some(951_868_800));
        // `date -u -d 2026-10-21T14:05:09Z +%s`
        assert_eq!(secs("2026-10-21T14:05:09.999Z"), Some(1_792_591_509));
        let millis = parse("2026-10-21T14:05:09.250Z")
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_millis();
        assert_eq!(millis, 250);
    }

    #[test]
    fn refuses_other_text() {
        for bad in [
            "",
            "2026-10-21",
            "2026-10-21T14:05:09",
            "2026-13-01T00:00:00Z",
            "2026-10-21T24:00:00Z",
            "2026-10-21T14:05:09.5Z",
            "1969-12-31T23:59:59Z",
            "2026-10-21-01T14:05:09Z",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
