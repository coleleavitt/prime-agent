//! `String.prototype.localeCompare` for the ids this crate sorts.
//!
//! The TS product sorted tree and experiment ids with ICU's root collation.
//! For the ASCII ids it writes (`<task>-s<seed>-i<n>-<clock>`, `...-i0p<k>-...`)
//! that order is: punctuation (in ICU's order) before digits before letters,
//! letters compared case-insensitively first, lowercase before uppercase on a
//! tie. Non-ASCII characters sort after letters by code point, which is an
//! approximation the product's ids never reach.

use std::cmp::Ordering;

/// ICU root order of the ASCII punctuation and symbols.
const PUNCTUATION: &str = "\t\n\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if let Some(index) = PUNCTUATION.find(c) {
        return (0, u32::try_from(index).unwrap_or(u32::MAX));
    }
    if c.is_ascii_digit() {
        return (1, u32::from(c));
    }
    if c.is_ascii_alphabetic() {
        return (2, u32::from(c.to_ascii_lowercase()));
    }
    (3, u32::from(c))
}

/// Compare like `a.localeCompare(b)` under the root locale, for ASCII ids.
#[must_use]
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let primary_order = a.chars().map(primary).cmp(b.chars().map(primary));
    if primary_order != Ordering::Equal {
        return primary_order;
    }
    // Tertiary: lowercase sorts before uppercase.
    let case = |c: char| u8::from(c.is_ascii_uppercase());
    a.chars().map(case).cmp(b.chars().map(case))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_punctuation_digits_letters_and_case_like_icu() {
        let mut ids = vec!["b", "B", "a", "_x", "-x", "1", "a-1", "a_1", "a1"];
        ids.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(ids, ["_x", "-x", "1", "a", "a_1", "a-1", "a1", "b", "B"]);
        assert_eq!(
            locale_compare(
                "circle-packing-s7-i0-1789842143996",
                "circle-packing-s7-i0p0-1789842143996"
            ),
            Ordering::Less
        );
    }
}
