"""Generate pa-computer-use's Python-parity Unicode tables from this CPython."""
import sys
import unicodedata

out = []
out.append("//! Unicode tables for Python-parity text formatting, generated from `CPython`")
out.append(f"//! {sys.version_info.major}.{sys.version_info.minor} (Unicode {unicodedata.unidata_version}), the kernel's interpreter, so")
out.append("//! `repr()` and `str.casefold()` stay byte-identical to the Python skill they replace.")
out.append("//!")
out.append("//! Regenerate with `python3 crates/pa-computer-use/scripts/gen_pyfmt_tables.py >")
out.append("//! crates/pa-computer-use/src/pyfmt/tables.rs` (run with the kernel's Python).")
out.append("")
out.append("#![allow(clippy::unreadable_literal)] // generated data, hex as CPython prints it")
out.append("")
ranges = []
start = None
for c in range(sys.maxunicode + 2):
    np = c <= sys.maxunicode and not chr(c).isprintable()
    if np and start is None:
        start = c
    if not np and start is not None:
        ranges.append((start, c - 1))
        start = None
out.append("/// Inclusive code point ranges `str.isprintable()` rejects, sorted.")
out.append(f"pub(super) const NON_PRINTABLE: [(u32, u32); {len(ranges)}] = [")
line = "   "
for a, b in ranges:
    item = f" (0x{a:X}, 0x{b:X}),"
    if len(line) + len(item) > 100:
        out.append(line)
        line = "   "
    line += item
out.append(line)
out.append("];")
out.append("")
diff = [(c, chr(c).casefold()) for c in range(sys.maxunicode + 1) if not (0xD800 <= c <= 0xDFFF) and chr(c).casefold() != chr(c).lower()]
out.append("/// Code points whose `str.casefold()` differs from their per-character `str.lower()`, sorted.")
out.append(f"pub(super) const CASEFOLD_EXCEPTIONS: [(char, &str); {len(diff)}] = [")
for c, folded in diff:
    escaped = "".join(f"\\u{{{ord(ch):X}}}" for ch in folded)
    out.append(f"    ('\\u{{{c:X}}}', \"{escaped}\"),")
out.append("];")
print("\n".join(out))
