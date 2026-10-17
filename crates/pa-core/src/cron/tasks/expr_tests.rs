use chrono::{
    DateTime,
    Datelike,
    FixedOffset,
    LocalResult,
    NaiveDate,
    NaiveDateTime,
    Offset,
    TimeZone,
    Utc,
    Weekday,
};

use super::*;

/// A zone with the US Eastern rules: EST (-5) and EDT (-4) from the second
/// Sunday of March at 02:00 standard to the first Sunday of November at
/// 02:00 daylight.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UsEastern;

fn nth_sunday(year: i32, month: u32, n: u32) -> NaiveDate {
    NaiveDate::from_weekday_of_month_opt(year, month, Weekday::Sun, n as u8).unwrap()
}

impl UsEastern {
    fn offset_at_utc(utc: &NaiveDateTime) -> FixedOffset {
        let year = utc.date().year();
        let start = nth_sunday(year, 3, 2).and_hms_opt(7, 0, 0).unwrap();
        let end = nth_sunday(year, 11, 1).and_hms_opt(6, 0, 0).unwrap();
        let hours = if *utc >= start && *utc < end { -4 } else { -5 };
        FixedOffset::east_opt(hours * 3600).unwrap()
    }
}

impl TimeZone for UsEastern {
    type Offset = FixedOffset;

    fn from_offset(_offset: &FixedOffset) -> Self {
        UsEastern
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
        self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
    }

    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
        let candidates: Vec<FixedOffset> = [-4, -5]
            .into_iter()
            .map(|hours| FixedOffset::east_opt(hours * 3600).unwrap())
            .filter(|offset| {
                let utc = *local - chrono::Duration::seconds(i64::from(offset.local_minus_utc()));
                Self::offset_at_utc(&utc) == *offset
            })
            .collect();
        match candidates.as_slice() {
            [] => LocalResult::None,
            [one] => LocalResult::Single(*one),
            [daylight, standard, ..] => LocalResult::Ambiguous(*daylight, *standard),
        }
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
        Self::offset_at_utc(&utc.and_hms_opt(0, 0, 0).unwrap())
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
        Self::offset_at_utc(utc).fix()
    }
}

pub(crate) fn utc_zone() -> ChronoZone<Utc> {
    ChronoZone(Utc)
}

pub(crate) fn eastern() -> ChronoZone<UsEastern> {
    ChronoZone(UsEastern)
}

pub(crate) fn ms(iso: &str) -> i64 {
    DateTime::parse_from_rfc3339(iso)
        .unwrap_or_else(|error| panic!("{iso}: {error}"))
        .timestamp_millis()
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// An expression and its expected minutes and weekdays (`None`: invalid).
type GrammarCase = (&'static str, Option<(&'static [u32], &'static [u32])>);

#[test]
fn the_grammar_matches_claude_codes() {
    let set = |values: &[u32]| {
        values
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    };
    let cases: [GrammarCase; 14] = [
        (
            "*/5 * * * *",
            Some((
                &[0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55],
                &[0, 1, 2, 3, 4, 5, 6],
            )),
        ),
        ("0 9 * * 1-5", Some((&[0], &[1, 2, 3, 4, 5]))),
        ("0 0 * * 7", Some((&[0], &[0]))),
        ("0 0 * * 5-7", Some((&[0], &[0, 5, 6]))),
        (
            "1-10/3 * * * 0-7",
            Some((&[1, 4, 7, 10], &[0, 1, 2, 3, 4, 5, 6])),
        ),
        (
            "5,10,15 * * * *",
            Some((&[5, 10, 15], &[0, 1, 2, 3, 4, 5, 6])),
        ),
        ("  7   *  * * *  ", Some((&[7], &[0, 1, 2, 3, 4, 5, 6]))),
        ("60 * * * *", None),
        ("* * * *", None),
        ("* * * * * *", None),
        ("*/0 * * * *", None),
        ("5-1 * * * *", None),
        ("0 0 0 * *", None),
        ("MON * * * *", None),
    ];
    for (expression, expected) in cases {
        let parsed = parse(expression);
        match expected {
            None => assert_eq!(parsed, None, "{expression}"),
            Some((minutes, weekdays)) => {
                let parsed = parsed.unwrap_or_else(|| panic!("{expression} parses"));
                assert_eq!(parsed.minute, set(minutes), "{expression} minutes");
                assert_eq!(parsed.day_of_week, set(weekdays), "{expression} weekdays");
            }
        }
    }
}

#[test]
fn the_next_match_in_utc() {
    let zone = utc_zone();
    let cases = [
        (
            "*/5 * * * *",
            "2026-10-17T10:02:30Z",
            "2026-10-17T10:05:00Z",
        ),
        (
            "*/5 * * * *",
            "2026-10-17T10:05:00Z",
            "2026-10-17T10:10:00Z",
        ),
        ("0 9 * * *", "2026-10-17T09:00:00Z", "2026-10-18T09:00:00Z"),
        (
            "0 9 * * 1-5",
            "2026-10-17T10:00:00Z",
            "2026-10-19T09:00:00Z",
        ),
        (
            "30 14 28 2 *",
            "2026-10-17T10:00:00Z",
            "2027-02-28T14:30:00Z",
        ),
        ("0 0 29 2 *", "2026-03-01T00:00:00Z", "2028-02-29T00:00:00Z"),
        // Both day fields restricted: the 13th OR a Friday.
        ("0 0 13 * 5", "2026-10-17T00:00:00Z", "2026-10-23T00:00:00Z"),
        ("0 0 13 * 5", "2026-11-07T00:00:00Z", "2026-11-13T00:00:00Z"),
        (
            "59 23 31 12 *",
            "2026-12-31T23:59:00Z",
            "2027-12-31T23:59:00Z",
        ),
    ];
    for (expression, after, expected) in cases {
        assert_eq!(
            next_fire(expression, ms(after), &zone).map(iso),
            Some(expected.to_string()),
            "{expression} after {after}"
        );
    }
    assert_eq!(
        next_fire("0 0 31 2 *", ms("2026-01-01T00:00:00Z"), &zone),
        None
    );
    assert_eq!(next_fire("nope", 0, &zone), None);
}

#[test]
fn the_expression_runs_in_local_time() {
    let india = ChronoZone(FixedOffset::east_opt(5 * 3600 + 1800).unwrap());
    assert_eq!(
        next_fire("0 9 * * *", ms("2026-10-17T00:00:00Z"), &india).map(iso),
        Some("2026-10-17T03:30:00Z".to_string())
    );
    assert_eq!(local_minute(ms("2026-10-17T03:00:00Z"), &india), 30);
}

/// Daylight saving (US Eastern, 2026: spring forward March 8 at 02:00,
/// fall back November 1 at 02:00): a pinned time in the gap fires at the
/// first instant after it; an every-hour job skips the gap's minutes; a
/// pinned time in the repeated hour fires once, an every-hour job in both
/// copies.
#[test]
fn daylight_saving_edges() {
    let zone = eastern();
    let cases = [
        // 00:00 EST -> 02:30 does not exist -> 03:00 EDT.
        ("30 2 * * *", "2026-03-08T05:00:00Z", "2026-03-08T07:00:00Z"),
        ("30 2 * * *", "2026-03-08T07:00:00Z", "2026-03-09T06:30:00Z"),
        // 01:45 EST -> 02:00-02:45 are gone -> 03:00 EDT.
        (
            "*/15 * * * *",
            "2026-03-08T06:45:00Z",
            "2026-03-08T07:00:00Z",
        ),
        // A daily 09:00 keeps its wall time across the change.
        ("0 9 * * *", "2026-03-07T14:00:00Z", "2026-03-08T13:00:00Z"),
        // Fall back: 01:30 EDT once, then the next day's 01:30 EST.
        ("30 1 * * *", "2026-11-01T04:00:00Z", "2026-11-01T05:30:00Z"),
        ("30 1 * * *", "2026-11-01T05:30:00Z", "2026-11-02T06:30:00Z"),
        // Inside the second copy, a pinned 01:15 already ran.
        ("15 1 * * *", "2026-11-01T06:05:00Z", "2026-11-02T06:15:00Z"),
        // Every half hour runs in both copies of 01:xx.
        (
            "*/30 * * * *",
            "2026-11-01T05:00:00Z",
            "2026-11-01T05:30:00Z",
        ),
        (
            "*/30 * * * *",
            "2026-11-01T05:30:00Z",
            "2026-11-01T06:00:00Z",
        ),
        (
            "*/30 * * * *",
            "2026-11-01T06:00:00Z",
            "2026-11-01T06:30:00Z",
        ),
        (
            "*/30 * * * *",
            "2026-11-01T06:30:00Z",
            "2026-11-01T07:00:00Z",
        ),
        // Wall order is not instant order: from 01:10 EDT the next half hour
        // is 01:30 EDT, not the later copy of 01:00.
        (
            "*/30 * * * *",
            "2026-11-01T05:10:00Z",
            "2026-11-01T05:30:00Z",
        ),
    ];
    for (expression, after, expected) in cases {
        assert_eq!(
            next_fire(expression, ms(after), &zone).map(iso),
            Some(expected.to_string()),
            "{expression} after {after}"
        );
    }
}

#[test]
fn the_local_stamps() {
    let zone = eastern();
    assert_eq!(
        local_stamp(ms("2026-10-17T18:30:00Z"), &zone),
        "Oct 17 2:30pm"
    );
    assert_eq!(
        local_stamp(ms("2026-01-02T05:05:00Z"), &zone),
        "Jan 2 12:05am"
    );
    assert_eq!(local_clock(ms("2026-10-17T18:30:05Z"), &zone), "14:30:05");
}

#[test]
fn the_human_schedules() {
    let cases = [
        ("* * * * *", "Every minute"),
        ("*/1 * * * *", "Every minute"),
        ("*/5 * * * *", "Every 5 minutes"),
        ("0 * * * *", "Every hour"),
        ("7 * * * *", "Every hour at :07"),
        ("0 */2 * * *", "Every 2 hours"),
        ("15 */1 * * *", "Every hour at :15"),
        ("0 9 * * *", "Every day at 9:00 AM"),
        ("30 14 * * *", "Every day at 2:30 PM"),
        ("0 0 * * *", "Every day at 12:00 AM"),
        ("0 12 * * *", "Every day at 12:00 PM"),
        ("57 8 * * 1", "Every Monday at 8:57 AM"),
        ("0 9 * * 7", "Every Sunday at 9:00 AM"),
        ("0 9 * * 1-5", "Weekdays at 9:00 AM"),
        ("30 14 28 2 *", "30 14 28 2 *"),
        ("bad", "bad"),
    ];
    for (expression, expected) in cases {
        assert_eq!(human_schedule(expression), expected, "{expression}");
    }
}
