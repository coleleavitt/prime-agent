set -e
printf 'Hello World\nFoo  Bar\nA B C\n' > names.txt
D="$PRIME_AGENT_CODING_AGENT_DIR/skills/evalslug"
mkdir -p "$D/src/evalslug"
cat > "$D/SKILL.md" <<'MD'
# evalslug
Slugify a string: lowercase, non-alphanumerics to single hyphens, trimmed.
Call: `evalslug.run("A B") -> "a-b"`
MD
cat > "$D/pyproject.toml" <<'TOML'
[project]
name = "evalslug"
version = "0.1.0"
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
TOML
cat > "$D/src/evalslug/__init__.py" <<'PY'
import re
def run(s: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")
PY
