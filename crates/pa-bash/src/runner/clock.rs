//! Wall-clock timestamps in the kernel's format: Python
//! `datetime.now(timezone.utc).isoformat()`.

use std::time::{SystemTime, UNIX_EPOCH};

/// `2026-10-07T05:12:34.123456+00:00`; the fraction is omitted when the
/// microseconds are zero, as `isoformat()` does.
pub(crate) fn iso_utc(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let micros = since.subsec_micros();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let clock = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    if micros == 0 {
        format!("{clock}+00:00")
    } else {
        format!("{clock}.{micros:06}+00:00")
    }
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn matches_python_isoformat() {
        let at = UNIX_EPOCH + Duration::from_micros(1_791_350_000_123_456);
        assert_eq!(iso_utc(at), "2026-10-07T05:13:20.123456+00:00");
        let whole = UNIX_EPOCH + Duration::from_hours(264_384);
        assert_eq!(iso_utc(whole), "2000-02-29T00:00:00+00:00");
    }
}
