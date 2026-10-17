//! Claude Code's five-field cron in the user's local time zone: the
//! grammar (`*`, `*/n`, `a-b`, `a-b/n`, `n`, comma lists; day-of-week 7 is
//! Sunday), the next-match search, and the human-readable schedule.
//!
//! The zone is a seam ([`CronZone`]): the product reads the system zone
//! (`chrono::Local`, which honors `TZ`), tests pass a zone with fixed DST
//! rules. Daylight-saving edges follow Vixie cron, which Claude Code's
//! wall-clock walk approximates: a job pinned to specific hours fires once
//! in a repeated hour (its first copy) and, when its time falls in a
//! spring-forward gap, at the first instant after the gap; a job that runs
//! every hour runs in both copies of a repeated hour and skips the minutes a
//! gap removes.

use std::collections::BTreeSet;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike};

/// The minute-step bound of one next-match search (Claude Code's 527040: a
/// leap year of minutes; the walk skips whole months, days and hours, so the
/// reach is several years).
const SEARCH_STEPS: usize = 527_040;

/// One parsed expression: the matching values of each field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub minute: BTreeSet<u32>,
    pub hour: BTreeSet<u32>,
    pub day_of_month: BTreeSet<u32>,
    pub month: BTreeSet<u32>,
    /// 0 = Sunday (7 parses as 0).
    pub day_of_week: BTreeSet<u32>,
}

/// The UTC instants a local wall-clock time names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalInstants {
    /// The time falls in a spring-forward gap.
    None,
    Single(i64),
    /// A repeated hour: the earlier copy, then the later.
    Ambiguous(i64, i64),
}

/// The local time zone the expressions run in.
pub trait CronZone: Send + Sync {
    /// The wall-clock time at `utc_ms`.
    fn local(&self, utc_ms: i64) -> NaiveDateTime;
    /// The instants (epoch ms) a wall-clock time names.
    fn instants(&self, local: NaiveDateTime) -> LocalInstants;
}

/// Any chrono zone: `chrono::Local` in the product.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChronoZone<Tz>(pub Tz);

/// The system zone (`TZ`, else `/etc/localtime` or the platform's).
pub type SystemZone = ChronoZone<chrono::Local>;

impl SystemZone {
    #[must_use]
    pub fn system() -> Self {
        Self(chrono::Local)
    }
}

/// UTC (deterministic tests in the embedding crates).
pub type UtcZone = ChronoZone<chrono::Utc>;

impl UtcZone {
    #[must_use]
    pub fn utc() -> Self {
        Self(chrono::Utc)
    }
}

impl<Tz> CronZone for ChronoZone<Tz>
where
    Tz: TimeZone + Send + Sync,
{
    fn local(&self, utc_ms: i64) -> NaiveDateTime {
        match self.0.timestamp_millis_opt(utc_ms) {
            chrono::LocalResult::Single(time) | chrono::LocalResult::Ambiguous(time, _) => {
                time.naive_local()
            }
            chrono::LocalResult::None => chrono::DateTime::UNIX_EPOCH.naive_utc(),
        }
    }

    fn instants(&self, local: NaiveDateTime) -> LocalInstants {
        match self.0.from_local_datetime(&local) {
            chrono::LocalResult::None => LocalInstants::None,
            chrono::LocalResult::Single(time) => LocalInstants::Single(time.timestamp_millis()),
            chrono::LocalResult::Ambiguous(early, late) => {
                LocalInstants::Ambiguous(early.timestamp_millis(), late.timestamp_millis())
            }
        }
    }
}

/// Parse one expression (Claude Code's grammar). `None` when it is not five
/// valid fields.
#[must_use]
pub fn parse(expression: &str) -> Option<CronExpr> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
        return None;
    };
    Some(CronExpr {
        minute: parse_field(minute, 0, 59, false)?,
        hour: parse_field(hour, 0, 23, false)?,
        day_of_month: parse_field(day_of_month, 1, 31, false)?,
        month: parse_field(month, 1, 12, false)?,
        day_of_week: parse_field(day_of_week, 0, 6, true)?,
    })
}

fn parse_field(field: &str, min: u32, max: u32, weekday: bool) -> Option<BTreeSet<u32>> {
    let mut values = BTreeSet::new();
    for part in field.split(',') {
        if let Some(rest) = part.strip_prefix('*') {
            let step = match rest.strip_prefix('/') {
                Some(step) => number(step)?,
                None if rest.is_empty() => 1,
                None => return None,
            };
            if step < 1 {
                return None;
            }
            values.extend((min..=max).step_by(step as usize));
            continue;
        }
        if let Some((range, step)) = split_range(part) {
            let (low, high) = range;
            let step = match step {
                Some(step) => number(step)?,
                None => 1,
            };
            let top = if weekday { 7 } else { max };
            let (low, high) = (number(low)?, number(high)?);
            if low > high || step < 1 || low < min || high > top {
                return None;
            }
            values.extend(
                (low..=high)
                    .step_by(step as usize)
                    .map(|value| if weekday && value == 7 { 0 } else { value }),
            );
            continue;
        }
        let mut value = number(part)?;
        if weekday && value == 7 {
            value = 0;
        }
        if value < min || value > max {
            return None;
        }
        values.insert(value);
    }
    (!values.is_empty()).then_some(values)
}

/// `a-b` or `a-b/n`.
fn split_range(part: &str) -> Option<((&str, &str), Option<&str>)> {
    let (range, step) = match part.split_once('/') {
        Some((range, step)) => (range, Some(step)),
        None => (part, None),
    };
    let (low, high) = range.split_once('-')?;
    Some(((low, high), step))
}

fn number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl CronExpr {
    fn hour_is_wildcard(&self) -> bool {
        self.hour.len() == 24
    }

    /// Claude Code's day rule: both day fields restricted match either;
    /// one restricted matches that one; neither matches every day.
    fn matches_day(&self, date: NaiveDate) -> bool {
        let every_date = self.day_of_month.len() == 31;
        let every_weekday = self.day_of_week.len() == 7;
        let date_matches = self.day_of_month.contains(&date.day());
        let weekday_matches = self
            .day_of_week
            .contains(&date.weekday().num_days_from_sunday());
        match (every_date, every_weekday) {
            (true, true) => true,
            (true, false) => weekday_matches,
            (false, true) => date_matches,
            (false, false) => date_matches || weekday_matches,
        }
    }

    /// The first wall-clock minute at or after `from` whose fields match.
    fn next_wall_match(&self, from: NaiveDateTime, steps: &mut usize) -> Option<NaiveDateTime> {
        let mut at = from;
        while *steps > 0 {
            *steps -= 1;
            if !self.month.contains(&at.month()) {
                at = first_of_next_month(at)?;
                continue;
            }
            if !self.matches_day(at.date()) {
                at = at.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hour.contains(&at.hour()) {
                at = at.date().and_hms_opt(at.hour(), 0, 0)? + Duration::hours(1);
                continue;
            }
            if !self.minute.contains(&at.minute()) {
                at += Duration::minutes(1);
                continue;
            }
            return Some(at);
        }
        None
    }

    /// The first fire strictly after `after_ms`, in `zone`'s wall clock.
    ///
    /// Wall-clock order is not instant order across a fall-back, so the walk
    /// starts [`MAX_BACKWARD_SHIFT`] before the wall time of `after_ms` and
    /// keeps the earliest eligible instant, stopping once a wall match lies
    /// that far past the best one found.
    #[must_use]
    pub fn next_after(&self, after_ms: i64, zone: &dyn CronZone) -> Option<i64> {
        let local = zone.local(after_ms);
        let mut at = local.date().and_hms_opt(local.hour(), local.minute(), 0)?
            - Duration::minutes(MAX_BACKWARD_SHIFT_MINUTES);
        let mut steps = SEARCH_STEPS;
        let mut best: Option<(i64, NaiveDateTime)> = None;
        while let Some(wall) = self.next_wall_match(at, &mut steps) {
            if let Some((_, best_wall)) = best {
                if wall > best_wall + Duration::minutes(MAX_BACKWARD_SHIFT_MINUTES) {
                    break;
                }
            }
            for instant in self.eligible_instants(wall, zone, &mut steps) {
                if instant > after_ms && best.is_none_or(|(current, _)| instant < current) {
                    best = Some((instant, wall));
                }
            }
            at = wall + Duration::minutes(1);
        }
        best.map(|(instant, _)| instant)
    }

    /// The instants one matching wall-clock minute fires at (the module
    /// docs' daylight-saving rules).
    fn eligible_instants(
        &self,
        wall: NaiveDateTime,
        zone: &dyn CronZone,
        steps: &mut usize,
    ) -> Vec<i64> {
        match zone.instants(wall) {
            LocalInstants::Single(instant) => vec![instant],
            LocalInstants::Ambiguous(early, late) if self.hour_is_wildcard() => vec![early, late],
            LocalInstants::Ambiguous(early, _) => vec![early],
            LocalInstants::None if self.hour_is_wildcard() => Vec::new(),
            LocalInstants::None => first_instant_after_gap(wall, zone, steps)
                .into_iter()
                .collect(),
        }
    }
}

/// The largest backward wall-clock shift a zone transition makes (one hour
/// almost everywhere; the bound leaves room for historical oddities).
const MAX_BACKWARD_SHIFT_MINUTES: i64 = 3 * 60;

/// The first existing instant after a gap's wall-clock minute.
fn first_instant_after_gap(
    wall: NaiveDateTime,
    zone: &dyn CronZone,
    steps: &mut usize,
) -> Option<i64> {
    let mut at = wall;
    while *steps > 0 {
        *steps -= 1;
        at += Duration::minutes(1);
        match zone.instants(at) {
            LocalInstants::None => {}
            LocalInstants::Single(instant) | LocalInstants::Ambiguous(instant, _) => {
                return Some(instant);
            }
        }
    }
    None
}

fn first_of_next_month(at: NaiveDateTime) -> Option<NaiveDateTime> {
    let (year, month) = if at.month() == 12 {
        (at.year() + 1, 1)
    } else {
        (at.year(), at.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1)?.and_hms_opt(0, 0, 0)
}

/// The next fire of `expression` strictly after `after_ms`; `None` for an
/// invalid expression or one that never matches.
#[must_use]
pub fn next_fire(expression: &str, after_ms: i64, zone: &dyn CronZone) -> Option<i64> {
    parse(expression)?.next_after(after_ms, zone)
}

/// The wall-clock minute of an instant (the one-shot jitter keys on it).
#[must_use]
pub fn local_minute(utc_ms: i64, zone: &dyn CronZone) -> u32 {
    zone.local(utc_ms).minute()
}

/// The local wall-clock time of an instant as `Oct 17 2:30pm`.
#[must_use]
pub fn local_stamp(utc_ms: i64, zone: &dyn CronZone) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = zone.local(utc_ms);
    let month = MONTHS[local.month0() as usize];
    let (pm, hour) = local.hour12();
    let suffix = if pm { "pm" } else { "am" };
    format!(
        "{month} {} {hour}:{:02}{suffix}",
        local.day(),
        local.minute()
    )
}

/// The local wall-clock time of an instant as `14:30:05`.
#[must_use]
pub fn local_clock(utc_ms: i64, zone: &dyn CronZone) -> String {
    let local = zone.local(utc_ms);
    format!(
        "{:02}:{:02}:{:02}",
        local.hour(),
        local.minute(),
        local.second()
    )
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// `9:00 AM`.
fn clock_label(minute: u32, hour: u32) -> String {
    let (suffix, hour12) = match hour {
        0 => ("AM", 12),
        1..=11 => ("AM", hour),
        12 => ("PM", 12),
        _ => ("PM", hour - 12),
    };
    format!("{hour12}:{minute:02} {suffix}")
}

/// Claude Code's human-readable schedule (`Every 5 minutes`, `Weekdays at
/// 9:00 AM`); anything it has no phrase for comes back verbatim.
#[must_use]
pub fn human_schedule(expression: &str) -> String {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let [minute, hour, dom, month, dow] = fields.as_slice() else {
        return expression.to_string();
    };
    let step = |field: &str| field.strip_prefix("*/").and_then(number);
    let rest_any = *dom == "*" && *month == "*" && *dow == "*";
    if *hour == "*" && rest_any {
        if *minute == "*" {
            return "Every minute".to_string();
        }
        if let Some(every) = step(minute) {
            return if every == 1 {
                "Every minute".to_string()
            } else {
                format!("Every {every} minutes")
            };
        }
        if let Some(at) = number(minute) {
            return if at == 0 {
                "Every hour".to_string()
            } else {
                format!("Every hour at :{at:02}")
            };
        }
    }
    if let (Some(at), Some(every), true) = (number(minute), step(hour), rest_any) {
        let suffix = if at == 0 {
            String::new()
        } else {
            format!(" at :{at:02}")
        };
        return if every == 1 {
            format!("Every hour{suffix}")
        } else {
            format!("Every {every} hours{suffix}")
        };
    }
    let (Some(minute), Some(hour)) = (number(minute), number(hour)) else {
        return expression.to_string();
    };
    if hour > 23 || minute > 59 {
        return expression.to_string();
    }
    let at = clock_label(minute, hour);
    if rest_any {
        return format!("Every day at {at}");
    }
    if *dom == "*" && *month == "*" {
        if dow.len() == 1 {
            if let Some(day) = number(dow) {
                return format!("Every {} at {at}", WEEKDAYS[(day % 7) as usize]);
            }
        }
        if *dow == "1-5" {
            return format!("Weekdays at {at}");
        }
    }
    expression.to_string()
}

#[cfg(test)]
#[path = "expr_tests.rs"]
pub(crate) mod tests;
