#!/bin/sh
# Regenerate the icat goldens from a kitty checkout (KITTY=<path>, default
# ~/CLionProjects/forks/kitty; generated from 8ca3cc0d3, 2026-10-06).
#
# kitty's serializer (`tools/tui/graphics/command.go`) and the diacritics
# table (`tools/utils/images/rowcolumn_diacritics.go`) are copied into a
# scratch module verbatim, minus the tty/loop helpers they import, and the
# enum stringer is regenerated with kitty's own `gen/go_code.py`
# (`stringer.py`). `main.go.in` drives them the way `kittens/icat` does
# under tmux. Nothing from kitty is committed here, only its output.
set -eu
KITTY=${KITTY:-$HOME/CLionProjects/forks/kitty}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/graphics" "$WORK/images"
python3 - "$KITTY" "$WORK" <<'PY'
import re, sys
kitty, work = sys.argv[1], sys.argv[2]
src = open(kitty + "/tools/tui/graphics/command.go").read()
src = src.replace('"github.com/emmansun/base64"', '"encoding/base64"')
for dep in ("tools/tty", "tools/tui/loop", "tools/utils"):
    src = re.sub(r'\n\s*"github.com/kovidgoyal/kitty/' + dep + '"', "", src)
src = src.replace("var debugprintln = tty.DebugPrintln\nvar _ = debugprintln\n", "")
src = re.sub(r"type loop_io_writer struct \{.*?\n\}\n\nfunc \(self \*loop_io_writer\).*?\n\}\n\n"
             r"func \(self \*GraphicsCommand\) WriteWithPayloadToLoop.*?\n\}\n", "", src, flags=re.S)
src = src.replace("utils.UnsafeBytesToString(", "string(")
open(work + "/graphics/command.go", "w").write(src)
open(work + "/images/rowcolumn_diacritics.go", "w").write(
    open(kitty + "/tools/utils/images/rowcolumn_diacritics.go").read())
PY
cp "$HERE/main.go.in" "$WORK/main.go"
printf 'module icatgolden\n\ngo 1.24\n' > "$WORK/go.mod"
cd "$WORK"
KITTY="$KITTY" python3 "$HERE/stringer.py"
GOFLAGS=-mod=mod GOPROXY=off go run .
python3 -c "import zlib; open('transmit_big.golden.zz','wb').write(zlib.compress(open('transmit_big.golden','rb').read(), 9))"
cp transmit_small.golden transmit_big.golden.zz delete.golden delete_placements.golden \
   virtual_place.golden placeholder_3x2.golden diacritics.golden "$HERE/"
