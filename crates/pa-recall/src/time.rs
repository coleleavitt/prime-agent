//! Wall-clock helpers in the TS product's representation: epoch
//! milliseconds and `Date.prototype.toISOString` text.

/// Milliseconds since the Unix epoch (`Date.now()`).
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `new Date(millis).toISOString()`.
#[must_use]
pub fn format_iso(millis: i64) -> String {
    pa_core::session::manager::format_iso(millis)
}

fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `Date.parse` for the ISO forms the product writes and reads:
/// `YYYY-MM-DDTHH:MM[:SS[.sss]]` with `Z` or a `±HH:MM` offset, or a bare
/// date. `None` where `Date.parse` would give `NaN`.
#[must_use]
pub fn parse_iso_millis(text: &str) -> Option<i64> {
    let (date, rest) = match text.split_once('T') {
        Some((date, rest)) => (date, Some(rest)),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let year = parts
        .next()
        .filter(|part| part.len() == 4)
        .and_then(digits)?;
    let month = parts
        .next()
        .filter(|part| part.len() == 2)
        .and_then(digits)?;
    let day = parts
        .next()
        .filter(|part| part.len() == 2)
        .and_then(digits)?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let Some(rest) = rest else {
        return Some(days * 86_400_000);
    };
    let (clock, offset_minutes) = if let Some(clock) = rest.strip_suffix('Z') {
        (clock, 0)
    } else {
        let split = rest.rfind(['+', '-'])?;
        let (clock, offset) = rest.split_at(split);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':')?;
        let hours = Some(hours)
            .filter(|part| part.len() == 2)
            .and_then(digits)?;
        let minutes = Some(minutes)
            .filter(|part| part.len() == 2)
            .and_then(digits)?;
        (clock, sign * (hours * 60 + minutes))
    };
    let (hms, fraction) = match clock.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (clock, None),
    };
    let mut fields = hms.split(':');
    let hour = fields
        .next()
        .filter(|part| part.len() == 2)
        .and_then(digits)?;
    let minute = fields
        .next()
        .filter(|part| part.len() == 2)
        .and_then(digits)?;
    let second = match fields.next() {
        Some(part) if part.len() == 2 => digits(part)?,
        Some(_) => return None,
        None => 0,
    };
    if fields.next().is_some() || hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    let millis = match fraction {
        Some(fraction) if !fraction.is_empty() => {
            let padded: String = fraction.chars().chain("000".chars()).take(3).collect();
            digits(&padded)?
        }
        Some(_) => return None,
        None => 0,
    };
    Some(
        days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis
            - offset_minutes * 60_000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_text_round_trips_through_epoch_milliseconds() {
        for millis in [0, 1_767_225_600_123, 1_000_000_000_000] {
            assert_eq!(parse_iso_millis(&format_iso(millis)), Some(millis));
        }
        assert_eq!(
            parse_iso_millis("2026-01-01T01:00:00+01:00"),
            parse_iso_millis("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(parse_iso_millis("not a date"), None);
    }
}
