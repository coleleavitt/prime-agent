//! Claude Code 2.1.316's `turn_duration` in-stream row (`showTurnDuration`
//! on by default): at every settled turn the TUI shows
//! `${verb} for ${d}${done ? ' \u{00B7} done ' + clock : ''}`, where the verb
//! is picked deterministically from the turn id by hashing it into CC's
//! eight-verb table (chunk-ah34pb7m.js:35, `Rf` / `kf`).
//!
//! The row is a `role: "custom"` message with `customType: "turn_duration"`
//! — the same persistence pathway compaction's `refinement_outcome` uses,
//! so it lands in the session journal and the TUI picks it up through the
//! ordinary custom-row decoder. The TUI chose to render it inline rather
//! than as footer chrome (user directive 2026-11-05).

use chrono::{Local, TimeZone};
use pa_types::ai::UserContent;
use pa_types::session::CustomMessage;
use serde_json::json;

use crate::session_engine::refine::now_millis;

/// The customType the row carries.
pub const TURN_DURATION_CUSTOM_TYPE: &str = "turn_duration";

/// Claude Code's eight-verb table in the order `kf` indexes with
/// `(zhe(uuid) >>> 0) % 8`. Keep the order byte-identical.
pub const TURN_DURATION_VERBS: [&str; 8] = [
    "Baked",
    "Brewed",
    "Churned",
    "Cogitated",
    "Cooked",
    "Crunched",
    "Saut\u{00E9}ed",
    "Worked",
];

/// Fallback when the uuid is empty (Claude Code's `RB`).
pub const TURN_DURATION_FALLBACK_VERB: &str = "Worked";

/// Pick the verb for one turn from its stable identifier. Claude Code uses
/// the final assistant message's uuid (`zhe(m) >>> 0`); we accept any
/// stable per-turn string so the host can pass whatever it has at settle
/// time (session id + turn start nanos is enough).
#[must_use]
pub fn pick_verb(stable_turn_id: &str) -> &'static str {
    if stable_turn_id.is_empty() {
        return TURN_DURATION_FALLBACK_VERB;
    }
    // FNV-1a 32-bit: deterministic, no dep, good enough for % 8 spread.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in stable_turn_id.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let index = (hash as usize) % TURN_DURATION_VERBS.len();
    TURN_DURATION_VERBS
        .get(index)
        .copied()
        .unwrap_or(TURN_DURATION_FALLBACK_VERB)
}

/// Format a turn duration the way Claude Code's `ln()` does: at least a
/// second ("1s"), minutes-and-seconds for sub-hour durations ("3m 4s"),
/// hours-and-minutes once an hour is reached ("1h 7m"). A zero duration
/// still prints "0s" so the row reads consistently.
#[must_use]
pub fn format_duration_ms(duration_ms: u64) -> String {
    let total_s = duration_ms / 1000;
    if total_s < 60 {
        return format!("{total_s}s");
    }
    let minutes = total_s / 60;
    let seconds = total_s % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds}s");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes}m")
}

/// Format a completion clock time the way Claude Code's `RWe()` does:
/// localised 12-hour clock with am/pm ("11:47 AM"). Reads the host's
/// `TZ`/system zone through [`chrono::Local`], same as `cron::tasks`.
#[must_use]
pub fn format_clock_time(done_at_unix_ms: i64) -> String {
    let seconds = done_at_unix_ms.div_euclid(1000);
    let nanos: u32 = u32::try_from(done_at_unix_ms.rem_euclid(1000))
        .unwrap_or(0)
        .saturating_mul(1_000_000);
    let Some(local) = Local.timestamp_opt(seconds, nanos).single() else {
        return String::new();
    };
    // "%-I" = hour without a leading zero, matching CC's "11:47 AM".
    local.format("%-I:%M %p").to_string()
}

/// Build the `turn_duration` custom row the worker injects at settle time.
///
/// The `content` carries the final text so a plain-text replay of the
/// session renders it legibly ("Churned for 3m 4s \u{00B7} done 11:47 AM").
/// `details` carries the typed fields so the TUI decoder can rebuild the
/// row without re-parsing the string.
#[must_use]
pub fn create_turn_duration_message(
    verb: &str,
    duration_ms: u64,
    done_at_unix_ms: i64,
) -> CustomMessage {
    let duration_text = format_duration_ms(duration_ms);
    let clock_text = format_clock_time(done_at_unix_ms);
    let text = if clock_text.is_empty() {
        format!("{verb} for {duration_text}")
    } else {
        format!("{verb} for {duration_text} \u{00B7} done {clock_text}")
    };
    // Carry both the raw inputs and the pre-formatted strings. The TUI
    // reads the formatted fields (it does not link `chrono`), and the raw
    // values let tests or future renderers reformat without parsing the
    // text content back.
    let details = json!({
        "verb": verb,
        "durationMs": duration_ms,
        "doneAtUnixMs": done_at_unix_ms,
        "durationText": duration_text,
        "clockText": clock_text,
    });
    CustomMessage {
        custom_type: TURN_DURATION_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(text),
        display: true,
        details: Some(details),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_match_claude_codes_ordering_byte_for_byte() {
        // Copied from `chunk-ah34pb7m.js:35` `Rf`. Any reorder here breaks
        // the contract with Claude Code's indexing (`kf(m) % 8`).
        assert_eq!(
            TURN_DURATION_VERBS,
            [
                "Baked",
                "Brewed",
                "Churned",
                "Cogitated",
                "Cooked",
                "Crunched",
                "Saut\u{00E9}ed",
                "Worked",
            ]
        );
    }

    #[test]
    fn an_empty_id_falls_back_to_worked() {
        assert_eq!(pick_verb(""), TURN_DURATION_FALLBACK_VERB);
    }

    #[test]
    fn verb_selection_is_deterministic_for_the_same_id() {
        let id = "01a1a9d1-3754-7673-a077-d187eb8388ed:1730000000000";
        assert_eq!(pick_verb(id), pick_verb(id));
    }

    #[test]
    fn durations_cover_the_subsecond_subminute_and_subhour_ranges() {
        assert_eq!(format_duration_ms(0), "0s");
        assert_eq!(format_duration_ms(999), "0s");
        assert_eq!(format_duration_ms(1_000), "1s");
        assert_eq!(format_duration_ms(59_000), "59s");
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(184_000), "3m 4s");
        assert_eq!(format_duration_ms(3_600_000), "1h 0m");
        assert_eq!(format_duration_ms(4_020_000), "1h 7m");
    }

    #[test]
    fn a_known_timestamp_formats_as_localised_twelve_hour_clock() {
        // 2026-11-05 11:47:00 EST = 2026-11-05 16:47:00 UTC = 1793893620000 ms.
        // Use a tz-agnostic sanity check: the output ends in " AM" or " PM"
        // and matches the "H:MM aP" shape.
        let text = format_clock_time(1_793_893_620_000);
        assert!(
            text.ends_with(" AM") || text.ends_with(" PM"),
            "unexpected clock text: {text}",
        );
        assert!(text.contains(':'), "missing minute separator: {text}");
    }

    #[test]
    fn the_message_text_includes_the_separator_when_the_clock_is_known() {
        let message = create_turn_duration_message("Churned", 184_000, 1_793_893_620_000);
        assert_eq!(message.custom_type, TURN_DURATION_CUSTOM_TYPE);
        let UserContent::Text(ref text) = message.content else {
            panic!("expected a text content block, got {:?}", message.content);
        };
        assert!(text.starts_with("Churned for 3m 4s \u{00B7} done "));
        let details = message.details.expect("details are always present");
        assert_eq!(details["verb"], "Churned");
        assert_eq!(details["durationMs"], 184_000);
        assert_eq!(details["doneAtUnixMs"], 1_793_893_620_000_i64);
    }
}
