"""Jupyter notebooks as Claude Code's Read and NotebookEdit tools see them.

Read renders each cell as ``<cell id="ID">SOURCE</cell id="ID">`` (a non-code
cell also names its ``<cell_type>``), followed by its outputs as text. The ID
is the cell's ``id`` (nbformat 4.5+), else ``cell-N`` for its index. Edits
replace, insert or delete one cell; the notebook is written back with
Jupyter's own layout (one-space indent, non-ASCII kept).
"""

from __future__ import annotations

import json
import re
import uuid
from typing import Any

INVALID_NOTEBOOK = (
    'Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of '
    'cell objects, each with a string or string-array "source").'
)
# Claude Code's bound on one cell's rendered outputs before they are replaced
# by a pointer to read them another way.
_OUTPUTS_LIMIT = 10_000
_CELL_INDEX_ID = re.compile(r"^cell-(\d+)$")


def _parse(text: str) -> dict[str, Any]:
    try:
        notebook = json.loads(text)
    except json.JSONDecodeError as error:
        raise ValueError(
            "Notebook file is not valid JSON (it may be truncated, corrupted, or still being "
            f"written): {error}"
        ) from error
    cells = notebook.get("cells") if isinstance(notebook, dict) else None
    if not isinstance(cells, list) or not all(
        isinstance(cell, dict) and isinstance(cell.get("source"), (str, list)) for cell in cells
    ):
        raise ValueError(INVALID_NOTEBOOK)
    return notebook


def _dump(notebook: dict[str, Any], trailing_newline: bool) -> str:
    text = json.dumps(notebook, indent=1, ensure_ascii=False)
    return text + "\n" if trailing_newline else text


def _language(notebook: dict[str, Any]) -> str:
    info = (notebook.get("metadata") or {}).get("language_info") or {}
    return str(info.get("name") or "python")


def _source(cell: dict[str, Any]) -> str:
    source = cell.get("source", "")
    return "".join(source) if isinstance(source, list) else str(source)


def _joined(value: Any) -> str:
    if value is None:
        return ""
    return "".join(value) if isinstance(value, list) else str(value)


def _cell_id(cell: dict[str, Any], index: int) -> str:
    return str(cell.get("id") or f"cell-{index}")


def _output_text(output: dict[str, Any]) -> str:
    kind = output.get("output_type")
    if kind == "stream":
        return _joined(output.get("text"))
    if kind in ("execute_result", "display_data"):
        data = output.get("data") or {}
        text = _joined(data.get("text/plain"))
        images = [mime for mime in ("image/png", "image/jpeg") if isinstance(data.get(mime), str)]
        if images:
            text = (text + "\n" if text else "") + f"[{images[0]} output]"
        return text
    if kind == "error":
        traceback = "\n".join(output.get("traceback") or [])
        return f"{output.get('ename')}: {output.get('evalue')}\n{traceback}"
    return ""


def _render(notebook: dict[str, Any], only: str | None = None) -> str:
    """The notebook (or the one cell ``only`` names) as Read renders it."""
    lang = _language(notebook)
    cells = notebook["cells"]
    indices = range(len(cells))
    if only is not None:
        index = _find_cell(notebook, only)
        if index is None:
            raise ValueError(f'Cell with ID "{only}" not found in notebook')
        indices = range(index, index + 1)
    parts = []
    for index in indices:
        cell = cells[index]
        ident = _cell_id(cell, index)
        kind = cell.get("cell_type", "code")
        tags = ""
        if kind != "code":
            tags += f"<cell_type>{kind}</cell_type>"
        elif lang != "python":
            tags += f"<_language>{lang}</_language>"
        parts.append(f'<cell id="{ident}">{tags}{_source(cell)}</cell id="{ident}">')
        if kind == "code" and cell.get("outputs"):
            texts = [_output_text(output) for output in cell["outputs"] if isinstance(output, dict)]
            if sum(len(text) for text in texts) > _OUTPUTS_LIMIT and only is None:
                texts = [
                    "Outputs are too large to include. Read them with "
                    f'edit.notebook_read(path, _cell_id="{ident}")'
                ]
            parts.extend(text for text in texts if text)
    return "\n".join(parts)


def _find_cell(notebook: dict[str, Any], ident: str) -> int | None:
    """The index of the cell ``ident`` names: its ``id``, else ``cell-N``."""
    cells = notebook["cells"]
    for index, cell in enumerate(cells):
        if cell.get("id") == ident:
            return index
    match = _CELL_INDEX_ID.match(ident)
    if match and int(match.group(1)) < len(cells):
        return int(match.group(1))
    return None


def _apply(
    notebook: dict[str, Any],
    new_source: str,
    ident: str | None,
    cell_type: str | None,
    edit_mode: str,
) -> tuple[str, str, str | None, str]:
    """Apply one NotebookEdit in place.

    Returns:
        (mode applied, the edited cell's id, its previous source, its type).
        Replacing just past the last cell inserts there, as Claude Code does.
    """
    if edit_mode not in ("replace", "insert", "delete"):
        raise ValueError("edit_mode must be 'replace', 'insert' or 'delete'")
    if cell_type is not None and cell_type not in ("code", "markdown"):
        raise ValueError("cell_type must be 'code' or 'markdown'")
    if edit_mode == "insert" and cell_type is None:
        raise ValueError("Cell type is required when using edit_mode=insert.")
    cells = notebook["cells"]
    if ident is None:
        if edit_mode != "insert":
            raise ValueError("Cell ID must be specified when not inserting a new cell.")
        index = 0
    else:
        found = _find_cell(notebook, ident)
        if found is None:
            raise ValueError(f'Cell with ID "{ident}" not found in notebook.')
        index = found + 1 if edit_mode == "insert" else found
    mode = edit_mode
    if mode == "replace" and index == len(cells):
        mode = "insert"
        cell_type = cell_type or "code"
    nbformat = int(notebook.get("nbformat") or 0)
    minor = int(notebook.get("nbformat_minor") or 0)
    has_ids = nbformat > 4 or (nbformat == 4 and minor >= 5)
    if mode == "delete":
        removed = cells.pop(index)
        return mode, _cell_id(removed, index), _source(removed), str(removed.get("cell_type", "code"))
    if mode == "insert":
        kind = cell_type or "code"
        cell: dict[str, Any] = {"cell_type": kind}
        if has_ids:
            cell["id"] = uuid.uuid4().hex[:8]
        cell["metadata"] = {}
        cell["source"] = new_source
        if kind == "code":
            cell["execution_count"] = None
            cell["outputs"] = []
        cells.insert(index, cell)
        return mode, _cell_id(cell, index), None, kind
    cell = cells[index]
    previous = _source(cell)
    cell["source"] = new_source
    if cell_type is not None and cell_type != cell.get("cell_type"):
        cell["cell_type"] = cell_type
        if cell_type == "markdown":
            cell.pop("outputs", None)
            cell.pop("execution_count", None)
    if cell.get("cell_type") == "code":
        cell["execution_count"] = None
        cell["outputs"] = []
    return mode, _cell_id(cell, index), previous, str(cell.get("cell_type", "code"))
