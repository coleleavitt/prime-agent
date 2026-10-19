---
name: edit
description: Replace exact strings in files, write whole files, and read or edit Jupyter notebook cells through the session's permission rules, /rewind checkpoints and diff display. Use for targeted edits from the Python kernel instead of rewriting files with open().
---

# Edit

Every function here asks the session's permission rules first (Claude Code's
`Edit` rules), checkpoints the file for /rewind, and shows the change as a diff.
Plain `open(path, "w")` does none of that.

## Replace a string: `edit.run`

    await edit(path="pkg/file.py", old_str=old, new_str=new)
    await edit.run("pkg/file.py", "x = 1", "x = 2", replace_all=True)

- `old_str` must appear exactly once, or pass `replace_all=True`.
- An empty `old_str` creates the file (or fills an empty one) with `new_str`.
- The file keeps its encoding, byte-order mark and line endings. Text you
  read from a CRLF file through Python shows LF; it still matches, and the
  replacement is written with CRLF.
- Straight quotes in `old_str` match curly quotes in the file, and the
  replacement then uses the file's curly style.
- An empty `new_str` that removes whole lines also removes their line break.
- An edit to a file that changed on disk since you last read it in the kernel
  is refused ("File has been modified since read"): read it again. With the
  setting `kernel.editReadCheck: "read"`, an existing file you never read is
  refused too ("File has not been read yet"); `"off"` disables both checks.

If the text contains triple double quotes, use triple single-quoted variables
(`old = '''...'''`) or build `old`/`new` from inspected file slices.

## Write a whole file: `edit.write`

    await edit.write("notes/plan.md", text)

Creates the file (and its parent directories) or overwrites it. An existing
file keeps its encoding and byte-order mark; `content` is written as given.

## Notebooks: `edit.notebook_read`, `edit.notebook_edit`

    print(await edit.notebook_read("analysis.ipynb"))
    await edit.notebook_edit("analysis.ipynb", "df.describe()", "cell-3")
    await edit.notebook_edit("analysis.ipynb", "# Notes", "intro", cell_type="markdown", edit_mode="insert")
    await edit.notebook_edit("analysis.ipynb", "", "a1b2c3d4", edit_mode="delete")

`notebook_read` renders each cell as `<cell id="ID">source</cell id="ID">`
followed by its text outputs. The ID is the cell's `id`, or `cell-N` for its
index. `notebook_edit` replaces a cell's source (a code cell loses its
outputs), inserts a new cell after `cell_id` (first when `cell_id` is None;
`cell_type` is required), or deletes one.

## From a shell cell

    !edit --path pkg/file.py --old-str "..." --new-str "..."
