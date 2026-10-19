"""PDF pages as images and text, through poppler's command-line tools.

Claude Code's Read renders PDF pages with `pdftoppm -jpeg -r 100` and sends
them as images (at most 20 pages per read; a PDF of more than 10 pages must
name its pages). The same tools serve here, so the kernel needs no PDF
library: `pdfinfo` counts the pages, `pdftoppm` renders them, `pdftotext`
extracts the text layer. They ship together in poppler-utils.
"""

from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path

MAX_PAGES_PER_READ = 20
WHOLE_READ_PAGE_LIMIT = 10
# The text layer returned by one `pdf_text` call, in characters.
MAX_TEXT_CHARS = 200_000
_RENDER_TIMEOUT_SECONDS = 120
_PAGES = re.compile(r"^\s*(\d+)\s*(?:-\s*(\d+)\s*)?$")

_INSTALL_HINT = (
    "Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) "
    "to enable PDF page rendering."
)


def is_pdf(path: Path) -> bool:
    try:
        with path.open("rb") as handle:
            return handle.read(5) == b"%PDF-"
    except OSError:
        return False


def parse_pages(pages: str) -> tuple[int, int]:
    """``"3"`` or ``"2-5"`` as (first, last), 1-based and inclusive."""
    match = _PAGES.match(str(pages))
    if match is None:
        raise ValueError(f'pages must be a page or a range such as "3" or "1-5", got {pages!r}')
    first = int(match.group(1))
    last = int(match.group(2) or first)
    if first < 1 or last < first:
        raise ValueError(f"pages {pages!r} is not a valid range (pages start at 1)")
    return first, last


def _tool(name: str) -> str:
    found = shutil.which(name)
    if found is None:
        raise RuntimeError(f"{name} is not installed. {_INSTALL_HINT}")
    return found


def _failure(name: str, completed: subprocess.CompletedProcess[bytes], path: Path) -> ValueError:
    stderr = completed.stderr.decode("utf-8", "replace").strip()
    if re.search("password", stderr, re.IGNORECASE):
        return ValueError("PDF is password-protected. Please provide an unprotected version.")
    if re.search("damaged|corrupt|invalid|Couldn't (find trailer|read xref)", stderr, re.IGNORECASE):
        return ValueError(f"{path}: PDF file is corrupted or invalid.")
    return ValueError(f"{name} failed on {path}: {stderr or f'exit code {completed.returncode}'}")


def page_count(path: Path) -> int | None:
    """The document's page count, or None when pdfinfo cannot tell."""
    completed = subprocess.run(
        [_tool("pdfinfo"), str(path)],
        capture_output=True,
        timeout=_RENDER_TIMEOUT_SECONDS,
        check=False,
    )
    if completed.returncode != 0:
        raise _failure("pdfinfo", completed, path)
    match = re.search(rb"^Pages:\s+(\d+)", completed.stdout, re.MULTILINE)
    return int(match.group(1)) if match else None


def page_range(path: Path, pages: str | None) -> tuple[int, int]:
    """The pages one read covers, checked against the document and the
    per-read bounds (Claude Code's wording)."""
    count = page_count(path)
    if count == 0:
        raise ValueError("PDF reports 0 pages (empty page tree). The PDF may be invalid.")
    if pages is None:
        if count is None:
            raise ValueError(
                "This PDF's page count is unknown. Use the pages parameter to read specific "
                f'page ranges (e.g., pages="1-5"). Maximum {MAX_PAGES_PER_READ} pages per request.'
            )
        if count > WHOLE_READ_PAGE_LIMIT:
            raise ValueError(
                f"This PDF has {count} pages, which is too many to read at once. Use the pages "
                f'parameter to read specific page ranges (e.g., pages="1-5"). Maximum '
                f"{MAX_PAGES_PER_READ} pages per request."
            )
        return 1, count
    first, last = parse_pages(pages)
    if last - first + 1 > MAX_PAGES_PER_READ:
        raise ValueError(f"pages {pages!r} names more than {MAX_PAGES_PER_READ} pages; read at most that many per call")
    if count is not None and last > count:
        page_word = "page" if count == 1 else "pages"
        raise ValueError(
            f"Requested pages {pages} is outside the document (PDF has {count} {page_word}). "
            f"Use a range within 1-{count}, maximum {MAX_PAGES_PER_READ} pages per request "
            f'(e.g. pages="1-{min(count, MAX_PAGES_PER_READ)}").'
        )
    return first, last


def render(path: Path, first: int, last: int, out_dir: Path) -> list[Path]:
    """Render pages ``first..last`` as JPEGs at 100 dpi; one file per page, in order."""
    prefix = out_dir / "page"
    completed = subprocess.run(
        [_tool("pdftoppm"), "-jpeg", "-r", "100", "-f", str(first), "-l", str(last), str(path), str(prefix)],
        capture_output=True,
        timeout=_RENDER_TIMEOUT_SECONDS,
        check=False,
    )
    if completed.returncode != 0:
        raise _failure("pdftoppm", completed, path)
    # pdftoppm zero-pads the page number to the document's digit count.
    rendered = sorted(out_dir.glob("page-*.jpg"), key=lambda page: int(page.stem.rsplit("-", 1)[1]))
    if not rendered:
        raise ValueError(f"pdftoppm rendered no pages from {path}")
    return rendered


def text(path: Path, pages: str | None) -> str:
    """The text layer of ``pages`` (the whole document when None), laid out."""
    command = [_tool("pdftotext"), "-layout"]
    if pages is not None:
        first, last = parse_pages(pages)
        command += ["-f", str(first), "-l", str(last)]
    completed = subprocess.run(
        [*command, str(path), "-"],
        capture_output=True,
        timeout=_RENDER_TIMEOUT_SECONDS,
        check=False,
    )
    if completed.returncode != 0:
        raise _failure("pdftotext", completed, path)
    extracted = completed.stdout.decode("utf-8", "replace")
    if len(extracted) > MAX_TEXT_CHARS:
        extracted = (
            extracted[:MAX_TEXT_CHARS]
            + f"\n[... text truncated at {MAX_TEXT_CHARS} characters; pass pages= to read the rest]"
        )
    return extracted
