"""Text-file fidelity for the edit skill: encodings, BOMs, line endings, quotes.

A file is decoded as Claude Code's Edit tool decodes it (UTF-16LE when it
starts with that BOM, else strict UTF-8 with an optional BOM) and encoded back
the same way, with no newline translation in either direction: an edit
changes the bytes of the replaced text and nothing else, so a CRLF file stays
CRLF and a BOM stays in place.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

_UTF8_BOM = b"\xef\xbb\xbf"
_UTF16LE_BOM = b"\xff\xfe"

# Claude Code's refusal for a file that is not valid UTF-8 (writing it back
# would replace every undecodable byte).
NOT_UTF8_MESSAGE = (
    "File is not valid UTF-8. It may use a legacy encoding such as Windows-1252, "
    "Shift-JIS or GBK, or be binary. This tool saves the whole file as UTF-8, which "
    "would replace every byte it cannot decode with U+FFFD. Nothing was written. Make "
    "the change with a shell command that reads and writes the file in its own "
    "encoding, or ask the user whether to convert the file to UTF-8 first."
)

_LEFT_SINGLE = "‘"
_RIGHT_SINGLE = "’"
_LEFT_DOUBLE = "“"
_RIGHT_DOUBLE = "”"
_BARE_LF = re.compile(r"(?<!\r)\n")


@dataclass(frozen=True)
class Decoded:
    """A file's text and how to write it back byte-for-byte."""

    text: str
    encoding: str
    bom: bytes

    def encode(self, text: str) -> bytes:
        return self.bom + text.encode(self.encoding)


def decode(raw: bytes) -> Decoded:
    """Decode a file's bytes without newline translation.

    Raises:
        ValueError: The bytes are not valid in the detected encoding.
    """
    if raw.startswith(_UTF16LE_BOM):
        encoding, bom = "utf-16-le", _UTF16LE_BOM
    elif raw.startswith(_UTF8_BOM):
        encoding, bom = "utf-8", _UTF8_BOM
    else:
        encoding, bom = "utf-8", b""
    try:
        text = raw[len(bom) :].decode(encoding)
    except UnicodeDecodeError as error:
        raise ValueError(NOT_UTF8_MESSAGE) from error
    return Decoded(text=text, encoding=encoding, bom=bom)


def new_file() -> Decoded:
    return Decoded(text="", encoding="utf-8", bom=b"")


def to_crlf(text: str) -> str:
    """``text`` with every bare LF written as CRLF."""
    return _BARE_LF.sub("\r\n", text)


def _straight_quotes(text: str) -> str:
    return (
        text.replace(_LEFT_SINGLE, "'")
        .replace(_RIGHT_SINGLE, "'")
        .replace(_LEFT_DOUBLE, '"')
        .replace(_RIGHT_DOUBLE, '"')
    )


def _find_quote_normalized(content: str, needle: str) -> str | None:
    """The file's spelling of ``needle`` when they differ only in curly versus
    straight quotes (Claude Code's Edit fallback), else None."""
    index = _straight_quotes(content).find(_straight_quotes(needle))
    if index == -1:
        return None
    return content[index : index + len(needle)]


@dataclass(frozen=True)
class Match:
    """The text an edit replaces, as the file spells it, and its replacement."""

    old: str
    new: str


def find(content: str, old: str, new: str) -> Match | None:
    """Locate ``old`` in ``content`` and adapt ``new`` to the file's spelling.

    Tried in order: the exact text; the text with its line breaks written as
    CRLF (a model reads a CRLF file through Python's universal newlines and
    quotes it with LF); and each of those with curly and straight quotes
    treated alike. The replacement takes the same line breaks and, when the
    file used curly quotes, the same quote style (Claude Code's rules).
    """
    candidates = [(old, new)]
    if "\r\n" in content and _BARE_LF.search(old):
        candidates.append((to_crlf(old), to_crlf(new)))
    for candidate_old, candidate_new in candidates:
        if candidate_old in content:
            return Match(candidate_old, candidate_new)
    for candidate_old, candidate_new in candidates:
        actual = _find_quote_normalized(content, candidate_old)
        if actual is not None:
            return Match(actual, _apply_quote_style(candidate_old, actual, candidate_new))
    return None


def _opens_quote(chars: list[str], index: int) -> bool:
    if index == 0:
        return True
    return chars[index - 1] in (" ", "\t", "\n", "\r", "(", "[", "{", "—", "–")


def _curl_double(text: str) -> str:
    chars = list(text)
    return "".join(
        (_LEFT_DOUBLE if _opens_quote(chars, i) else _RIGHT_DOUBLE) if ch == '"' else ch
        for i, ch in enumerate(chars)
    )


def _curl_single(text: str) -> str:
    chars = list(text)
    out = []
    for i, ch in enumerate(chars):
        if ch != "'":
            out.append(ch)
            continue
        before = chars[i - 1] if i > 0 else ""
        after = chars[i + 1] if i + 1 < len(chars) else ""
        if before.isalpha() and after.isalpha():
            # An apostrophe inside a word.
            out.append(_RIGHT_SINGLE)
        else:
            out.append(_LEFT_SINGLE if _opens_quote(chars, i) else _RIGHT_SINGLE)
    return "".join(out)


def _apply_quote_style(old: str, actual: str, new: str) -> str:
    if old == actual:
        return new
    doubles = _LEFT_DOUBLE in actual or _RIGHT_DOUBLE in actual
    singles = _LEFT_SINGLE in actual or _RIGHT_SINGLE in actual
    if doubles:
        new = _curl_double(new)
    if singles:
        new = _curl_single(new)
    return new


def deletion_target(content: str, old: str) -> str:
    """Deleting a line also deletes its line break (Claude Code's rule): when
    the replacement is empty and ``old`` ends mid-line, take the break after it."""
    if old.endswith("\n"):
        return old
    for newline in ("\r\n", "\n"):
        if old + newline in content:
            return old + newline
    return old


def display(text: str) -> str:
    """Text for the diff display: CRLF shown as LF."""
    return text.replace("\r\n", "\n")
