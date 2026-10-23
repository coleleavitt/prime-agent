"""Edit, write and notebook-edit files through the session's permission rules,
/rewind file history and diff display (Claude Code's Edit, Write and
NotebookEdit tools)."""

from __future__ import annotations

from . import _io, _notebook, _text


async def run(path: str, old_str: str, new_str: str, replace_all: bool = False) -> str:
    """Replace an exact string in a file.

    ``old_str`` must appear exactly once in the file at ``path`` (or any number
    of times with ``replace_all=True``); each match is replaced with
    ``new_str`` and the file is written back in place. The file keeps its
    encoding, byte-order mark and line endings: text read from a CRLF file
    through Python (which shows LF) still matches, and the replacement is
    written with CRLF. Straight quotes in ``old_str`` match the file's curly
    quotes, and the replacement then takes the file's quote style. An empty
    ``new_str`` that deletes whole lines also deletes their line break. An
    empty ``old_str`` creates the file with ``new_str`` (or fills an empty
    one). Prefer this over rewriting a whole file for targeted edits.

    Args:
        path: File to edit, relative to the working directory, absolute, or
            `~`-prefixed (the leading `~`/`~user` is expanded to the home dir).
        old_str: Exact text to find; empty to create the file.
        new_str: Replacement text (must differ from ``old_str``).
        replace_all: Replace every occurrence instead of requiring one.

    Returns:
        A short confirmation message.

    Raises:
        FileNotFoundError: If ``path`` does not exist (and ``old_str`` is not empty).
        ValueError: If ``old_str`` is absent, matches more than once without
            ``replace_all``, equals ``new_str``, or the file is not UTF-8/UTF-16
            text, a notebook, or was modified since it was read.
        PermissionError: If the session's permission rules refuse the edit.
    """
    filepath, absolute = _io._target(path)
    if old_str == new_str:
        raise ValueError("No changes to make: old_str and new_str are exactly the same.")
    if old_str and filepath.suffix == ".ipynb":
        raise ValueError("File is a Jupyter Notebook. Use edit.notebook_edit to edit this file.")
    exists = filepath.exists()
    if not exists and old_str:
        raise FileNotFoundError(f"File does not exist: {path}")
    # Before this function reads the file itself (which counts as a read).
    _check_read_state(absolute, exists)
    resolved = str(filepath.resolve())
    if exists:
        decoded = _text._decode(filepath.read_bytes())
    else:
        decoded = _text._new_file()
    content = decoded.text
    if not old_str:
        if content.strip():
            raise ValueError("Cannot create new file - file already exists.")
        await _io._check_permission(resolved)
        await _io._write_bytes(filepath, resolved, decoded.encode(new_str), create_parents=not exists)
        _io._note_written(absolute)
        verb = "Created" if not exists else "Wrote"
        _io._emit_diff(resolved, "", _text._display(new_str), 1, f"{verb} {resolved}")
        return f"{verb} {resolved}"
    match = _text._find(content, old_str, new_str)
    if match is None:
        raise ValueError(f"String to replace not found in {path}.\nString: {old_str}")
    count = content.count(match.old)
    if count > 1 and not replace_all:
        raise ValueError(
            f"Found {count} matches of the string to replace, but replace_all is False. "
            "To replace all occurrences, set replace_all=True. To replace only one "
            "occurrence, please provide more context to uniquely identify the instance.\n"
            f"String: {old_str}"
        )
    old = _text._deletion_target(content, match.old) if match.new == "" else match.old
    await _io._check_permission(resolved)
    start_line = content.count("\n", 0, content.index(old)) + 1
    updated = content.replace(old, match.new) if replace_all else content.replace(old, match.new, 1)
    await _io._write_bytes(filepath, resolved, decoded.encode(updated))
    _io._note_written(absolute)
    _io._emit_diff(resolved, _text._display(old), _text._display(match.new), start_line, f"Edited {resolved}")
    if replace_all and count > 1:
        return f"Edited {resolved} ({count} replacements)"
    return f"Edited {resolved}"


async def write(path: str, content: str) -> str:
    """Create or overwrite a whole file (Claude Code's Write tool).

    Goes through the same permission rules, /rewind checkpoint and diff
    display as ``edit.run``; an existing file keeps its encoding and
    byte-order mark, and ``content`` is written as given. Missing parent
    directories are created. Prefer ``edit.run`` for changes to part of a file.

    Raises:
        ValueError: If the existing file was modified since it was read (or,
            under ``kernel.editReadCheck: "read"``, never read), or is not text.
        PermissionError: If the session's permission rules refuse the write.
    """
    filepath, absolute = _io._target(path)
    exists = filepath.exists()
    if exists and not filepath.is_file():
        raise IsADirectoryError(f"{path} is not a regular file")
    _check_read_state(absolute, exists)
    decoded = _text._new_file()
    if exists:
        try:
            decoded = _text._decode(filepath.read_bytes())
        except ValueError:
            pass  # not text: replaced whole, as UTF-8
    resolved = str(filepath.resolve())
    await _io._check_permission(resolved)
    await _io._write_bytes(filepath, resolved, decoded.encode(content), create_parents=not exists)
    _io._note_written(absolute)
    if exists:
        summary = f"The file {resolved} has been updated successfully."
    else:
        summary = f"File created successfully at: {resolved}"
    _io._emit_diff(resolved, _text._display(decoded.text), _text._display(content), 1, summary)
    return summary


async def notebook_read(path: str, cell_id: str | None = None) -> str:
    """Render a Jupyter notebook's cells (or one cell) with their outputs.

    Each cell reads ``<cell id="ID">SOURCE</cell id="ID">``; the ID is what
    ``edit.notebook_edit`` takes. Image outputs show as a placeholder line.
    """
    filepath, _absolute = _io._target(path)
    with open(filepath, "rb") as handle:
        notebook = _notebook._parse(_text._decode(handle.read()).text)
    return _notebook._render(notebook, cell_id)


async def notebook_edit(
    path: str,
    new_source: str,
    cell_id: str | None = None,
    *,
    cell_type: str | None = None,
    edit_mode: str = "replace",
) -> str:
    """Replace, insert or delete one cell of a Jupyter notebook (Claude Code's NotebookEdit).

    Args:
        path: The ``.ipynb`` file.
        new_source: The cell's new source (ignored for ``delete``).
        cell_id: The cell to edit, as ``edit.notebook_read`` shows it (its
            ``id``, or ``cell-N``). With ``insert`` the new cell goes after it,
            or first when it is None.
        cell_type: ``"code"`` or ``"markdown"``; required for ``insert``, else
            it keeps the cell's type. A replaced code cell loses its outputs.
        edit_mode: ``"replace"`` (default), ``"insert"`` or ``"delete"``.

    Raises:
        ValueError: An invalid notebook, an unknown cell, a missing cell type
            for an insert, or a notebook modified since it was read.
        PermissionError: If the session's permission rules refuse the edit.
    """
    filepath, absolute = _io._target(path)
    if filepath.suffix != ".ipynb":
        raise ValueError(
            "File must be a Jupyter notebook (.ipynb file). For editing other file types, use edit.run."
        )
    if not filepath.exists():
        raise FileNotFoundError("Notebook file does not exist.")
    _check_read_state(absolute, exists=True)
    decoded = _text._decode(filepath.read_bytes())
    notebook = _notebook._parse(decoded.text)
    mode, edited_id, previous, kind = _notebook._apply(notebook, new_source, cell_id, cell_type, edit_mode)
    updated = _notebook._dump(notebook, decoded.text.endswith("\n"))
    if "\r\n" in decoded.text:
        updated = _text._to_crlf(updated)
    resolved = str(filepath.resolve())
    await _io._check_permission(resolved)
    await _io._write_bytes(filepath, resolved, decoded.encode(updated))
    _io._note_written(absolute)
    if mode == "delete":
        summary = f"Deleted cell {edited_id}"
        _io._emit_diff(resolved, previous or "", "", 1, f"{summary} in {resolved}")
    elif mode == "insert":
        summary = f"Inserted cell {edited_id} ({kind})"
        _io._emit_diff(resolved, "", new_source, 1, f"{summary} in {resolved}")
    else:
        summary = f"Updated cell {edited_id}"
        _io._emit_diff(resolved, previous or "", new_source, 1, f"{summary} in {resolved}")
    return f"{summary} in {resolved}"


def _check_read_state(absolute: str, exists: bool) -> None:
    """Refuse a write to a file changed since the model read it (or never
    read, under `kernel.editReadCheck: "read"`). Runs before the permission
    check, so a stale write is refused without asking the user."""
    if exists:
        refusal = _io._read_refusal(absolute)
        if refusal is not None:
            raise ValueError(refusal)
