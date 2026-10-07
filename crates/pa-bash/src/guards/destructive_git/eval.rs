//! `eval` payloads: eval re-parses its argument, so quoted text the plain
//! scan treats as data still executes. Each payload is unquoted one layer at
//! a time and rescanned; a discard (or a relocation) found in any layer is
//! refused outright.

use super::sites::{find_discard_sites, has_discard};
use super::text::{
    contains, mask_quoted_spans, normalized, substitution_interiors, unquote_one_level,
};
use super::words::{
    plain_word_text, prefix_holds_directory_command, reveal_shell_command_words,
    shell_word_positions, AliasReading, Names,
};

/// Nesting past this many evals is refused rather than followed.
const MAX_EVAL_SCAN_DEPTH: usize = 10;

/// Each `(eval word position, payload)` a revealed command runs: from just
/// after a command-position `eval` to the next unquoted separator that is not
/// inside a substitution, unquoted one layer.
fn eval_payloads(revealed: &[char]) -> Vec<(usize, Vec<char>)> {
    let masked = mask_quoted_spans(revealed);
    let mut payloads = Vec::new();
    for word in shell_word_positions(revealed) {
        if !word.command
            || plain_word_text(&revealed[word.start..word.end]).as_deref() != Some("eval")
        {
            continue;
        }
        let interior: Vec<(usize, usize)> = substitution_interiors(&revealed[word.end..])
            .into_iter()
            .map(|(start, end)| (word.end + start, word.end + end))
            .collect();
        let region_end = (word.end..masked.len())
            .find(|&j| {
                ";&|\n".contains(masked[j])
                    && !interior.iter().any(|&(start, end)| start <= j && j < end)
            })
            .unwrap_or(revealed.len());
        payloads.push((
            word.start,
            unquote_one_level(&revealed[word.end..region_end]),
        ));
    }
    payloads
}

/// Whether a substitution in a payload can deliver a discard: its own text
/// (unquoted) discards, or it spells an alias whose value discards.
fn payload_substitution_hides_a_discard(payload: &[char], aliases: &Names) -> bool {
    let discarding: Vec<&String> = aliases
        .iter()
        .filter(|(_, value)| has_discard(&value.chars().collect::<Vec<_>>()))
        .map(|(name, _)| name)
        .collect();
    for (start, end) in substitution_interiors(payload) {
        let inner = &payload[start..end];
        if has_discard(&unquote_one_level(inner)) {
            return true;
        }
        if discarding.is_empty() {
            continue;
        }
        for word in shell_word_positions(inner) {
            if let Some(spelled) = plain_word_text(&inner[word.start..word.end]) {
                if discarding.contains(&&spelled) {
                    return true;
                }
            }
        }
    }
    false
}

fn revealed_payloads_hide_discard(
    revealed: &[char],
    depth: usize,
    aliases: &Names,
    assignments: &Names,
    eval_live: &std::collections::BTreeMap<usize, (Names, Names)>,
) -> bool {
    let none = Names::new();
    for (start, payload) in eval_payloads(revealed) {
        if let Some((live_aliases, live_assignments)) = eval_live.get(&start) {
            if !live_aliases.is_empty() || !live_assignments.is_empty() {
                if !find_discard_sites(&payload, live_aliases, live_assignments).is_empty() {
                    return true;
                }
                if payload_substitution_hides_a_discard(&payload, live_aliases) {
                    return true;
                }
            }
        }
        if !find_discard_sites(&payload, &none, &none).is_empty() {
            return true;
        }
        if payload_substitution_hides_a_discard(&payload, aliases) {
            return true;
        }
        if (!aliases.is_empty() || !assignments.is_empty())
            && !find_discard_sites(&payload, aliases, assignments).is_empty()
        {
            return true;
        }
        if contains(&payload, "eval")
            && payloads_hide_discard(&payload, depth + 1, aliases, assignments)
        {
            return true;
        }
    }
    false
}

/// Whether a quoted `eval` payload hides a destructive git discard. Only a
/// command-position eval runs its payload; aliases and assignments a caller
/// knows are carried in, because eval re-parses at run time.
pub(super) fn payloads_hide_discard(
    command: &[char],
    depth: usize,
    aliases: &Names,
    assignments: &Names,
) -> bool {
    if depth > MAX_EVAL_SCAN_DEPTH {
        return true;
    }
    let command = normalized(command).0;
    let expanded =
        reveal_shell_command_words(&command, AliasReading::Expanded, aliases, assignments);
    if revealed_payloads_hide_discard(
        &expanded.text,
        depth,
        &expanded.aliases,
        &expanded.assignments,
        &expanded.eval_live,
    ) {
        return true;
    }
    if contains(&command, "alias") {
        let as_written =
            reveal_shell_command_words(&command, AliasReading::AsWritten, aliases, assignments);
        if revealed_payloads_hide_discard(
            &as_written.text,
            depth,
            &expanded.aliases,
            &expanded.assignments,
            &as_written.eval_live,
        ) {
            return true;
        }
    }
    false
}

/// Whether a quoted `eval` payload in `command` can change directory, read
/// as written and with the names live at the eval (and the final ones)
/// expanded.
pub(super) fn payloads_relocate(command: &[char]) -> bool {
    let none = Names::new();
    let normalized = normalized(command).0;
    let revealed = reveal_shell_command_words(&normalized, AliasReading::Expanded, &none, &none);
    for (start, payload) in eval_payloads(&revealed.text) {
        if prefix_holds_directory_command(&payload) {
            return true;
        }
        let mut readings = vec![(&revealed.aliases, &revealed.assignments)];
        if let Some((aliases, assignments)) = revealed.eval_live.get(&start) {
            readings.push((aliases, assignments));
        }
        for (aliases, assignments) in readings {
            if aliases.is_empty() && assignments.is_empty() {
                continue;
            }
            let expanded =
                reveal_shell_command_words(&payload, AliasReading::Expanded, aliases, assignments)
                    .text;
            if expanded != payload && prefix_holds_directory_command(&expanded) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::text::chars;
    use super::*;

    /// `EvalPayloadDetectionTest` (`test_bash_git_guard.py`), which calls the
    /// Python `_eval_payloads_hide_destructive_git` directly.
    #[test]
    fn eval_payloads_hiding_discards() {
        let none = Names::new();
        let hiding = [
            "eval 'git reset --hard'",
            "eval \"git clean -f\"",
            "eval 'cd sub && git reset --hard'",
            "eval 'git checkout -- .'",
            "eval \"git restore .\"",
            "eval 'eval \"git reset --hard\"'",
            "GIT_DIR=sub/.git eval 'git reset --hard'",
            "eval 'git reset \\\n--hard'",
            "'eval' 'git reset --hard'",
            "E=eval; $E 'git reset --hard'",
            "{ eval 'git reset --hard'; }",
            "H='git reset --hard'; eval '$H'; H='echo hi'; $H",
            "H='git reset --hard' eval '$H'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\neval 'g'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\neval g",
            "shopt -s expand_aliases\nalias g='git reset --hard'\neval \"$(printf %s g)\"",
            "shopt -s expand_aliases\nalias g='git reset --hard'\nunalias -n g\neval 'g'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\nunalias -a -n\neval 'g'",
            "eval \"$(printf '%s' 'git reset --hard')\"",
            "X='git reset --hard'; X2=\"$X\"; eval \"$X2\"",
            "shopt -s expand_aliases\nalias g='git reset --hard'\neval 'g'\nalias g='echo hi'\ng",
        ];
        for command in hiding {
            assert!(
                payloads_hide_discard(&chars(command), 0, &none, &none),
                "{command:?}"
            );
        }
        let safe = [
            "eval",
            "eval 'echo hi'",
            "eval 'git status'",
            "eval 'echo \"git reset --hard\"'",
            "eval \"echo 'git reset --hard'\"",
            "echo 'eval git reset --hard'",
            "echo eval 'git reset --hard'",
            "npm run eval:suite",
            "shopt -s expand_aliases\nalias g='echo hi'\neval 'g'",
            "shopt -s expand_aliases\nalias g='git status'\neval 'g'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\neval 'echo g'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\nunalias g\neval 'g'",
            "shopt -s expand_aliases\nalias g='git reset --hard'\nunalias -- g\neval 'g'",
        ];
        for command in safe {
            assert!(
                !payloads_hide_discard(&chars(command), 0, &none, &none),
                "{command:?}"
            );
        }
    }
}
