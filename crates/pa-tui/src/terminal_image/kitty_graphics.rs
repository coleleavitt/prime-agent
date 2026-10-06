//! kitty graphics commands the way kitty's own client side writes them
//! (`tools/tui/graphics/command.go`, driven by `kittens/icat`), for the
//! paths TS never had: the tmux passthrough and unicode placeholders.
//!
//! The TS-parity encoders in the parent module stay byte-identical to TS for
//! the direct path; everything here follows icat byte for byte instead: the
//! key order of `serialize_non_default_fields`, the unpadded base64 and the
//! 128 KiB chunks of `WriteWithPayloadTo` (continuation chunks repeat `a`
//! and `q`, the last one omits the default `m=0`), the per-chunk
//! `ESC Ptmux;` wrap with doubled escapes, and `write_unicode_placeholder`'s
//! cells (`U+10EEEE` plus the row, column and id-high-byte diacritics, the
//! id's low 24 bits in the foreground colour).

use std::fmt::Write as _;

/// kitty's serialization order of the control keys
/// (`serialize_non_default_fields`).
const KEY_ORDER: &[u8] = b"aqftomCUdNsvSOxywhXYcriIpz";
/// `WriteWithPayloadTo`: payloads up to this many bytes go in one command.
const SINGLE_COMMAND_BYTES: usize = 2048;
/// `WriteWithPayloadTo`'s chunk size in base64 characters.
const CHUNK_CHARS: usize = 128 * 1024;
/// The unicode placeholder character (kitty's `ImagePlaceholderChar`).
pub(crate) const PLACEHOLDER: char = '\u{10EEEE}';

/// One graphics command's control keys (values already formatted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Command {
    keys: Vec<(u8, String)>,
}

impl Command {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Set `key` (replacing an earlier value).
    #[must_use]
    pub(crate) fn key(mut self, key: u8, value: impl std::fmt::Display) -> Self {
        self.keys.retain(|(existing, _)| *existing != key);
        self.keys.push((key, value.to_string()));
        self
    }

    fn get(&self, key: u8) -> Option<&str> {
        self.keys
            .iter()
            .find(|(existing, _)| *existing == key)
            .map(|(_, value)| value.as_str())
    }

    /// `serialize_to`: the APC with its keys in kitty's order, wrapped in
    /// tmux's passthrough.
    fn serialize(&self, chunk: &str) -> String {
        let mut keys: Vec<&(u8, String)> = self.keys.iter().collect();
        keys.sort_by_key(|(key, _)| KEY_ORDER.iter().position(|k| k == key));
        let mut apc = String::from("\x1b_G");
        for (index, (key, value)) in keys.iter().enumerate() {
            if index > 0 {
                apc.push(',');
            }
            let _ = write!(apc, "{}={value}", char::from(*key));
        }
        if !chunk.is_empty() {
            apc.push(';');
            apc.push_str(chunk);
        }
        apc.push_str("\x1b\\");
        tmux_passthrough(&apc)
    }
}

/// Wrap `sequence` in tmux's passthrough (icat's `WrapPrefix`/`WrapSuffix`
/// and `EncodeSerializedDataFunc`): tmux forwards the body to the outer
/// terminal when the pane's `allow-passthrough` is on.
pub(crate) fn tmux_passthrough(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// `WriteWithPayloadTo` through tmux's passthrough, over a payload already
/// in standard base64 (the preview's own encoding): kitty writes unpadded
/// base64, one command for a payload of at most 2048 bytes, else 128 KiB
/// chunks, each chunk wrapped on its own.
pub(crate) fn tmux_write_with_payload(command: &Command, base64: &str) -> String {
    let data = base64.trim_end_matches('=');
    if data.is_empty() {
        return command.serialize("");
    }
    let bytes = data.len() * 3 / 4;
    if bytes <= SINGLE_COMMAND_BYTES {
        return command.serialize(data);
    }
    let mut out = String::with_capacity(data.len() + data.len() / 1024 + 128);
    let mut first = command.clone();
    let mut rest = data;
    while !rest.is_empty() {
        let take = rest.len().min(CHUNK_CHARS);
        // Base64 is ASCII: every byte offset is a char boundary.
        let (chunk, after) = rest.split_at(take);
        rest = after;
        let mut current = std::mem::take(&mut first);
        if current.keys.is_empty() {
            // Continuation chunks carry only `q` and `a` (and `m`).
            for key in *b"qa" {
                if let Some(value) = command.get(key) {
                    current = current.key(key, value);
                }
            }
        }
        if !rest.is_empty() {
            current = current.key(b'm', 1);
        }
        out.push_str(&current.serialize(chunk));
    }
    out
}

/// The diacritics that number placeholder rows, columns, and the image id's
/// high byte (kitty's `gen/rowcolumn-diacritics.txt`: the class-230
/// combining marks of `UnicodeData.txt` without decompositions, in order).
#[rustfmt::skip]
pub(crate) const ROWCOLUMN_DIACRITICS: [char; 297] = [
    '\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}',
    '\u{346}', '\u{34a}', '\u{34b}', '\u{34c}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35b}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36a}', '\u{36b}', '\u{36c}', '\u{36d}', '\u{36e}', '\u{36f}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59c}', '\u{59d}', '\u{59e}', '\u{59f}', '\u{5a0}', '\u{5a1}',
    '\u{5a8}', '\u{5a9}', '\u{5ab}', '\u{5ac}', '\u{5af}', '\u{5c4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
    '\u{659}', '\u{65a}', '\u{65b}', '\u{65d}', '\u{65e}', '\u{6d6}', '\u{6d7}', '\u{6d8}',
    '\u{6d9}', '\u{6da}', '\u{6db}', '\u{6dc}', '\u{6df}', '\u{6e0}', '\u{6e1}', '\u{6e2}',
    '\u{6e4}', '\u{6e7}', '\u{6e8}', '\u{6eb}', '\u{6ec}', '\u{730}', '\u{732}', '\u{733}',
    '\u{735}', '\u{736}', '\u{73a}', '\u{73d}', '\u{73f}', '\u{740}', '\u{741}', '\u{743}',
    '\u{745}', '\u{747}', '\u{749}', '\u{74a}', '\u{7eb}', '\u{7ec}', '\u{7ed}', '\u{7ee}',
    '\u{7ef}', '\u{7f0}', '\u{7f1}', '\u{7f3}', '\u{816}', '\u{817}', '\u{818}', '\u{819}',
    '\u{81b}', '\u{81c}', '\u{81d}', '\u{81e}', '\u{81f}', '\u{820}', '\u{821}', '\u{822}',
    '\u{823}', '\u{825}', '\u{826}', '\u{827}', '\u{829}', '\u{82a}', '\u{82b}', '\u{82c}',
    '\u{82d}', '\u{951}', '\u{953}', '\u{954}', '\u{f82}', '\u{f83}', '\u{f86}', '\u{f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}',
    '\u{1d187}', '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}',
    '\u{1d242}', '\u{1d243}', '\u{1d244}',
];

/// The most rows or columns a placeholder block can number.
pub(crate) const MAX_PLACEHOLDER_CELLS: u32 = ROWCOLUMN_DIACRITICS.len() as u32;

/// One placeholder cell (icat's `write_unicode_placeholder`): the
/// placeholder plus the row, column, and id-high-byte diacritics. The
/// cell's foreground colour carries the id's low 24 bits
/// ([`placeholder_rgb`]).
pub(crate) fn placeholder_cell(image_id: u32, row: u32, column: u32) -> String {
    let diacritic = |index: u32| ROWCOLUMN_DIACRITICS[index as usize];
    let mut cell = String::with_capacity(16);
    cell.push(PLACEHOLDER);
    cell.push(diacritic(row));
    cell.push(diacritic(column));
    cell.push(diacritic(image_id >> 24));
    cell
}

/// The foreground colour a placeholder cell carries: the id's low 24 bits.
pub(crate) fn placeholder_rgb(image_id: u32) -> (u8, u8, u8) {
    (
        (image_id >> 16) as u8,
        (image_id >> 8) as u8,
        image_id as u8,
    )
}

/// A placeholder image id derived from `seed` (an image's content key):
/// icat's rejection rule — a non-zero high byte (the third diacritic) and a
/// non-zero middle (true colour needed to express it), so the id never
/// collides with an application limited to 256 colours.
pub(crate) fn placeholder_image_id(seed: u64) -> u32 {
    let mut state = seed;
    loop {
        // splitmix64: a deterministic stream, so the row layout and the
        // painter agree on the id without sharing state.
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        let id = (z ^ (z >> 31)) as u32;
        if id & 0xff00_0000 != 0 && id & 0x00ff_ff00 != 0 {
            return id;
        }
    }
}

/// Whether `symbol` is one placeholder cell (the text dumps blank them).
pub(crate) fn is_placeholder_cell(symbol: &str) -> bool {
    symbol.starts_with(PLACEHOLDER)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goldens written by kitty's own serializer: `tools/tui/graphics/
    /// command.go` and `rowcolumn_diacritics.go` (copied verbatim minus
    /// their tty/loop helpers, the stringer regenerated with kitty's
    /// `gen/go_code.py`), driven the way `kittens/icat` drives them under
    /// tmux (`fixtures/icat/README`).
    fn golden(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/src/terminal_image/fixtures/icat/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    const ID: u32 = 0x2A0B_0C0D;

    fn icat_transmit() -> Command {
        // icat's `gc_for_image`, frame 0, unicode placeholder, PNG as is.
        Command::new()
            .key(b'a', 'T')
            .key(b'q', 2)
            .key(b'f', 100)
            .key(b'U', 1)
            .key(b'c', 3)
            .key(b'r', 2)
            .key(b'i', ID)
    }

    fn base64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn the_tmux_transmit_matches_icat_byte_for_byte() {
        let small = tmux_write_with_payload(&icat_transmit(), &base64(b"\x89PNG-tiny-payload"));
        assert_eq!(small.into_bytes(), golden("transmit_small.golden"));
        // Over 2048 bytes: 128 KiB chunks, continuation keys `a,q`, the last
        // chunk without `m`.
        let big: Vec<u8> = (0..100 * 1024).map(|i: usize| (i * 7) as u8).collect();
        let chunked = tmux_write_with_payload(&icat_transmit(), &base64(&big));
        let mut expected = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(golden("transmit_big.golden.zz").as_slice()),
            &mut expected,
        )
        .expect("the golden inflates");
        assert_eq!(chunked.into_bytes(), expected);
    }

    #[test]
    fn deletes_and_virtual_placements_match_icat() {
        let base = Command::new().key(b'q', 2).key(b'i', ID);
        assert_eq!(
            tmux_write_with_payload(&base.clone().key(b'a', 'd').key(b'd', 'I'), "").into_bytes(),
            golden("delete.golden")
        );
        assert_eq!(
            tmux_write_with_payload(&base.clone().key(b'a', 'd').key(b'd', 'i'), "").into_bytes(),
            golden("delete_placements.golden")
        );
        let place = base.key(b'a', 'p').key(b'U', 1).key(b'c', 3).key(b'r', 2);
        assert_eq!(
            tmux_write_with_payload(&place, "").into_bytes(),
            golden("virtual_place.golden")
        );
    }

    #[test]
    fn placeholder_cells_match_icat() {
        // icat's `write_unicode_placeholder` for a 3x2 block (its colour
        // escape is the `38:2` form; the frame paints the same colour as a
        // cell style).
        let (r, g, b) = placeholder_rgb(ID);
        let mut rows = Vec::new();
        for row in 0..2 {
            let cells: String = (0..3).map(|col| placeholder_cell(ID, row, col)).collect();
            rows.push(cells);
        }
        let written = format!("\x1b[38:2:{r}:{g}:{b}m{}\x1b[39m", rows.join("\n\r"));
        assert_eq!(written.into_bytes(), golden("placeholder_3x2.golden"));
        let table: Vec<String> = ROWCOLUMN_DIACRITICS
            .iter()
            .map(|c| format!("{:x}", u32::from(*c)))
            .collect();
        assert_eq!(table.join(",").into_bytes(), golden("diacritics.golden"));
    }

    #[test]
    fn the_documented_placeholder_example_decodes() {
        // graphics-protocol.rst: image 42 in the low byte, `U+0305` is 0,
        // `U+030D` is 1, and `U+030E` (2) as the third diacritic makes the
        // id `42 + (2 << 24)`.
        let id = 42 + (2 << 24);
        assert_eq!(
            placeholder_cell(id, 0, 1),
            "\u{10EEEE}\u{305}\u{30D}\u{30E}"
        );
        assert_eq!(
            placeholder_cell(id, 1, 0),
            "\u{10EEEE}\u{30D}\u{305}\u{30E}"
        );
        assert_eq!(placeholder_rgb(id), (0, 0, 42));
    }

    #[test]
    fn placeholder_ids_follow_icats_rejection_rule_and_are_stable() {
        for seed in [0u64, 1, 42, u64::MAX, 0x1234_5678_9abc_def0] {
            let id = placeholder_image_id(seed);
            assert_ne!(id & 0xff00_0000, 0, "{seed}: {id:#x}");
            assert_ne!(id & 0x00ff_ff00, 0, "{seed}: {id:#x}");
            assert_eq!(placeholder_image_id(seed), id);
        }
        assert_ne!(placeholder_image_id(1), placeholder_image_id(2));
    }

    /// Each cell is one column to the width code and one grapheme to the
    /// cell grid: the diacritics are all zero-width combining marks.
    #[test]
    fn a_placeholder_cell_is_one_column_wide() {
        use unicode_segmentation::UnicodeSegmentation;
        for index in 0..MAX_PLACEHOLDER_CELLS {
            let cell = placeholder_cell((ID & 0x00ff_ffff) | (index.min(255) << 24), index, index);
            assert_eq!(crate::width::str_width(&cell), 1, "{index}: {cell:?}");
            assert_eq!(cell.graphemes(true).count(), 1, "{index}: {cell:?}");
        }
    }
}
