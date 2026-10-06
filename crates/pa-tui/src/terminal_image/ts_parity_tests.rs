//! Byte parity with the TS encoders and detection: `ts_goldens.json` is
//! the output of `git show v0.9.8:packages/tui/src/terminal-image.ts` run
//! under node (`ts_goldens.gen.mts` regenerates it).

use super::*;
use base64::Engine;
use serde_json::Value;

fn goldens() -> Value {
    serde_json::from_str(include_str!("ts_goldens.json")).expect("the goldens parse")
}

fn golden(name: &str) -> String {
    goldens()[name]
        .as_str()
        .unwrap_or_else(|| panic!("golden {name}"))
        .to_string()
}

/// The generator's payload: `n` bytes of `(i * 7 + 3) & 0xff`, base64.
fn payload(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn kitty_encodes_byte_identically_to_ts() {
    let small = payload(30);
    assert_eq!(
        encode_kitty(&small, &KittyOptions::default()),
        golden("kitty_small_plain")
    );
    assert_eq!(
        encode_kitty(
            &small,
            &KittyOptions {
                columns: Some(60),
                rows: Some(12),
                image_id: Some(4242),
                move_cursor: false,
            }
        ),
        golden("kitty_small_opts")
    );
    // 9336 base64 characters: three chunks (`m=1`, `m=1`, `m=0`).
    assert_eq!(
        encode_kitty(
            &payload(7000),
            &KittyOptions {
                columns: Some(40),
                rows: Some(9),
                image_id: Some(7),
                move_cursor: false,
            }
        ),
        golden("kitty_big_opts")
    );
    // Exactly one chunk's worth stays a single command; twice that splits in two.
    let one_id = |id| KittyOptions {
        image_id: Some(id),
        ..KittyOptions::default()
    };
    assert_eq!(
        encode_kitty(&"A".repeat(4096), &one_id(1)),
        golden("kitty_exact_4096")
    );
    assert_eq!(
        encode_kitty(&"B".repeat(8192), &one_id(2)),
        golden("kitty_8192")
    );
}

#[test]
fn iterm2_encodes_byte_identically_to_ts() {
    let small = payload(30);
    assert_eq!(
        encode_iterm2(&small, &Iterm2Options::default()),
        golden("iterm_plain")
    );
    assert_eq!(
        encode_iterm2(
            &small,
            &Iterm2Options {
                width: Some(Iterm2Size::Cells(60)),
                height: Some(Iterm2Size::Auto),
                ..Iterm2Options::default()
            }
        ),
        golden("iterm_render")
    );
    assert_eq!(
        encode_iterm2(
            &small,
            &Iterm2Options {
                width: Some(Iterm2Size::Cells(10)),
                height: Some(Iterm2Size::Cells(5)),
                name: Some("shot \u{e9}.png".to_string()),
                preserve_aspect_ratio: false,
                inline: false,
            }
        ),
        golden("iterm_named")
    );
}

#[test]
fn kitty_delete_matches_ts() {
    assert_eq!(delete_kitty_image(4242), golden("delete_one"));
}

#[test]
fn image_rows_match_ts() {
    let goldens = goldens();
    for case in goldens["rows"].as_array().expect("rows") {
        let n = |at: usize| case[at].as_u64().expect("number") as u32;
        assert_eq!(
            calculate_image_rows(
                ImageDimensions {
                    width_px: n(1),
                    height_px: n(2),
                },
                n(3),
                CellDimensions {
                    width_px: n(4),
                    height_px: n(5),
                },
            ),
            n(0),
            "{case}"
        );
    }
}

/// Every TS detection case: kitty, Ghostty, `WezTerm`, iTerm2, the
/// fallback terminals, tmux and screen, and empty variables.
#[test]
fn detection_matches_ts_per_environment() {
    let goldens = goldens();
    for case in goldens["detect"].as_array().expect("detect") {
        let env = case[0].as_object().expect("env");
        let expected = match case[1].as_str() {
            Some("kitty") => Some(ImageProtocol::Kitty),
            Some("iterm2") => Some(ImageProtocol::Iterm2),
            None => None,
            Some(other) => panic!("unknown protocol {other}"),
        };
        let detected =
            detect_image_protocol(|name| env.get(name).and_then(Value::as_str).map(str::to_string));
        assert_eq!(detected, expected, "{env:?}");
    }
}
