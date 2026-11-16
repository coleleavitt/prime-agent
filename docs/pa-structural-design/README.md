# pa-structural — design

Native port of AFT's five structural tool concepts into prime-agent, as a removable feature crate exposed through
the kernel's `rlm.*` surface. No AFT dep, no Node shim, no duplicate daemon. Fits the kernel-is-tool-surface model.

Reference reading: `~/RustProjects/forks/aft/crates/aft/src/commands/{outline,zoom,edit_symbol,add_import,ast_search,ast_replace}.rs`,
`aft/docs/tools.md`, `docs/fork-feature-crates.md`, `CLAUDE.md` → "What this fork adds on top of upstream".

## 1. Scope

Five tools land. Python surface (all return `dict` or raise `RuntimeError`; no top-level `_private`):

```python
rlm.outline(path: str | list[str], *, depth: int = 2) -> OutlineResult
    # { "entries": [{ "kind": "struct|fn|class|...", "name", "range": [l0,l1], "members": [...] }],
    #   "complete": bool, "skipped": [{file, reason}] }

rlm.zoom(path: str, symbol: str | list[str], *, scope: str | None = None, callgraph: bool = False) -> ZoomResult
    # { "symbol", "range": [l0,l1], "signature", "doc", "body",
    #   "callers": [{"file","line","snippet"}], "callees": [...] }  # callers/callees only when callgraph=True;
                                                                    # delegates to codegraph MCP internally
    # On ambiguity: raises AmbiguousSymbolError with .candidates

rlm.edit_symbol(path: str, symbol: str, *, op: str, content: str | None = None, scope: str | None = None)
    # op in {"replace","delete","insert_before","insert_after"}. Routes writes through pa_core::file_history
    # so /rewind recovers them. Returns { "range", "new_range", "syntax_valid", "checkpoint_id" }.

rlm.organize_imports(path: str, *, op: str, module: str | None = None, names: list[str] | None = None)
    # op in {"organize","add","remove"}. Language-aware (Rust use lines, Python imports, TS import stmts).
    # Backup via file_history. Returns { "changes": [{op, module, names, line}], "formatted": bool }.

rlm.ast_grep(pattern: str, *, lang: str, rewrite: str | None = None,
             paths: list[str] | None = None, globs: list[str] | None = None, dry_run: bool = True)
    # Pattern uses $VAR metavars like ast-grep. rewrite=None = search; set = replace.
    # Returns { "matches": [{"file","range","matched","rewritten"?}], "complete": bool }.
```

**Non-goals.** No background index, no semantic search lane (fruit-fly RAG already handles memory search; AFT's
semantic/trigram lane is out of scope — ripgrep via `bash()` stays the code-grep path). No call-graph implementation
(codegraph MCP already serves it; `zoom(callgraph=True)` delegates). No `apply_patch`, no `aft_inspect`, no
`aft_conflicts`, no `aft_move`/`aft_delete` (bash + mv/rm already fine; `file_history` tracks edit undo). No
tool-provider role, no Subc, no AFT binary resolution.

## 2. Crate layout

`crates/pa-structural/` — feature crate per `docs/fork-feature-crates.md`.

```
crates/pa-structural/
  Cargo.toml                   # deps: pa-types, pa-core, pa-telemetry; tree-sitter-* per language; ast-grep-core 0.51
  README.md                    # scope, non-goals, public API, seams, telemetry events
  src/
    lib.rs                     # pub re-exports; register_host_handlers(&mut HostRequestHandlers)
    parser/
      mod.rs                   # TreeSitterCache (one parser per language per thread), LangId enum
      rust.rs  python.rs  typescript.rs  javascript.rs      # phase 1
      go.rs    java.rs                                      # phase 2
    outline.rs                 # OutlineRequest -> OutlineResult handler; walks files with file-size cap
    zoom.rs                    # symbol resolver, container menu on ambiguity, callgraph seam
    edit_symbol.rs             # resolve -> backup -> edit -> syntax-check -> file_history commit
    imports/
      mod.rs  rust.rs  python.rs  typescript.rs             # language-specific insertion ordering
    ast_grep.rs                # wraps ast-grep-core; dry_run default true
    symbols.rs                 # Range, Symbol, SymbolKind — the shared vocabulary
    telemetry.rs               # counters: structural_outline_calls, structural_edit_symbol_commits, ...
  tests/
    outline_rust.rs  outline_python.rs  outline_typescript.rs
    zoom_ambiguity.rs  edit_symbol_rollback.rs  imports_organize.rs  ast_grep_dryrun.rs
```

Dependencies (from `crates/aft/Cargo.toml` heritage; pin versions, respect 7-day release-age rule):

| crate | version | purpose |
|---|---|---|
| `tree-sitter` | 0.25 | core parser runtime |
| `tree-sitter-rust` | 0.24 | phase 1 |
| `tree-sitter-python` | 0.23 | phase 1 |
| `tree-sitter-typescript` | 0.23 | phase 1 (ships both TS and TSX grammars) |
| `tree-sitter-javascript` | 0.23 | phase 1 (TS grammar reuses) |
| `ast-grep-core` | 0.51.2 | ast_grep tool |
| `tree-sitter-go` | 0.23 | phase 2 |
| `tree-sitter-java` | 0.23 | phase 2 |

Estimated workspace size cost: ~8 MB compiled grammars phase 1, ~12 MB phase 1+2, ~25 MB phase 3 (ten languages).
Verified: AFT's build with the full grammar set is ~564 MB debug; our smaller surface is a small fraction of that.

**Composition** per rule 3: `pa-cli` gets `feature = "structural"` (default-on in the fork build);
`pa-cli --no-default-features` excludes it. No native crate depends on `pa-structural`. `pa-core` exposes one new
seam if needed (see §3) and stays unaware of the feature.

## 3. Host-request types (pa-types additions)

Two-sided contract between `rlm/structural.py` and the host handler. Add to `pa-types::host_request` (same module
family as `bash.*`, `computer_use.*`). Names follow AFT's shape but prefixed `structural.`:

```rust
// crates/pa-types/src/host_request/structural.rs  (new file; pub-use from host_request/mod.rs)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralOutlineRequest {
    pub targets: Vec<PathBuf>,
    #[serde(default)] pub depth: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralOutlineResponse {
    pub entries: Vec<OutlineEntry>,
    pub complete: bool,
    pub skipped: Vec<SkippedFile>,
}

pub struct StructuralZoomRequest  { path: PathBuf, symbols: Vec<String>, scope: Option<String>, callgraph: bool }
pub struct StructuralZoomResponse { /* body, signature, range, callers?, callees? */ }

pub struct StructuralEditSymbolRequest { path, symbol, op: EditSymbolOp, content: Option<String>, scope }
pub struct StructuralEditSymbolResponse { range, new_range, syntax_valid, checkpoint_id }

pub struct StructuralImportsRequest  { path, op: ImportOp, module?, names: Vec<String> }
pub struct StructuralImportsResponse { changes: Vec<ImportChange>, formatted: bool }

pub struct StructuralAstGrepRequest  { pattern, lang, rewrite?, paths, globs, dry_run }
pub struct StructuralAstGrepResponse { matches: Vec<AstMatch>, complete }
```

Request-type IDs registered in `pa-types::host_request::id` (`structural.outline`, `structural.zoom`, ...). Added to
the ready-probe surface in `kernel/bootstrap/venv/probe.rs` so every new `rlm` function is asserted present.

Not a protocol bump: these are new host requests at the existing protocol 6 surface, served through the generic
`HostRequestHandlers` seam (no kernel protocol change, no `REPL_PROTOCOL_VERSION` change).

## 4. Kernel-side surface

`prime-agent-runtime/src/rlm/structural.py` — one module, five public functions, pattern lifted from
`rlm/bash.py` and `rlm/web.py`:

```python
# prime-agent-runtime/src/rlm/structural.py
from . import _host, _telemetry
from ._host import repl

async def outline(targets, *, depth=2):
    data = {"id": _host.new_id(), "op": "structural.outline",
            "params": {"targets": _as_list(targets), "depth": depth}}
    return _unwrap_host(await repl.host_request(data))

async def zoom(path, symbols, *, scope=None, callgraph=False): ...
async def edit_symbol(path, symbol, *, op, content=None, scope=None): ...
async def organize_imports(path, *, op, module=None, names=None): ...
async def ast_grep(pattern, *, lang, rewrite=None, paths=None, globs=None, dry_run=True): ...
```

All five emit a `structural_<name>_called` telemetry event (schema 7 addition, follow
`bash_sleep_refused` pattern). Public symbols `outline`, `zoom`, `edit_symbol`, `organize_imports`, `ast_grep`
exported at module top for the prompt-guard admission (`prompt_guards.rs`); every helper stays `_private`.

`rlm/__init__.py` re-exports under the `rlm.*` namespace. `repl.md` adds the five frames alongside existing ones.
`kernel/bootstrap/venv/probe.rs` adds each to `RUNTIME_READY_CHECK`.

## 5. Language coverage phases

| phase | languages | why | grammar size |
|---|---|---|---|
| 1 | Rust, Python, TypeScript, JavaScript | the three files prime-agent itself edits | ~8 MB |
| 2 | Go, Java | the next most-common fork users' stacks | +4 MB |
| 3 | C, C++, Ruby, Swift, Kotlin, PHP | parity with AFT's 30+ claim | +13 MB |

Each language implements `LanguageProvider` in `parser/<lang>.rs`: `outline_queries()`, `symbol_resolver()`,
`import_spec()` (for organize_imports). Phase gating is per-language `#[cfg(feature = "lang-go")]` so phase 1 can
ship without pulling phase 2+3 grammars.

## 6. ast_grep integration

Use `ast-grep-core = "0.51.2"` from crates.io. **Do not roll our own.** Rationale: ast-grep's metavar grammar
(`$VAR`, `$$$ARGS`), its multi-language config format, and its rewrite application semantics are non-trivial and
battle-tested. The crate is MIT, 0.51 is a year old, release cadence stable. Adopt its `AstGrep::rewrite` directly;
wrap in our `StructuralAstGrepRequest` shape. Not a workspace-root dep; gated behind pa-structural only.

## 7. Overlap with existing — must not duplicate

- **codegraph MCP** (`~/.prime/agent/settings.json` → `mcpServers.codegraph`) is the authoritative call-graph /
  xref provider. `rlm.zoom(callgraph=True)` MUST delegate to it, not re-implement graph traversal. The pa-structural
  handler forwards callers/callees via MCP in-process (same way other MCP tools are reached); no second call graph.
- **file_history** (`pa_core::file_history`, `crates/pa-core/src/file_history/`) owns edit undo and the
  content-addressed snapshot layer. `edit_symbol` and `organize_imports` MUST call `file_history::before_write` /
  `after_write` the same way the `edit` skill does. `/rewind` then restores structural edits for free.
- **fruit-fly retrieval** (`pa-retrieval`) is the semantic lane for memories, not code; no overlap, we skip
  AFT's semantic code search entirely.
- **pa-lsp** already serves diagnostics; `edit_symbol`'s `syntax_valid` check uses tree-sitter's own error-node
  detection (fast, local), not an LSP round-trip. LSP diagnostics come later via the existing `<new-diagnostics>`
  reminder path if the edit triggers one.
- **ripgrep via bash** stays the text-search path; AFT's trigram-indexed grep is out of scope.
- **pa-bash's sleep guard** is orthogonal and must not be touched by this port.

## 8. Test plan

Unit (per crate): one per language per tool — e.g. `pa_structural::outline::tests::rust_struct_with_impls`,
`::python_class_with_methods`, `::typescript_interface_and_type_alias`. Deterministic fixtures in
`crates/pa-structural/tests/fixtures/<lang>/`.

Integration (kernel-backed): `crates/pa-core/tests/kernel_structural.rs` — one golden test per tool through the
real kernel, following the `kernel_bash_*` / `kernel_web_*` pattern. Needs the sandboxed kernel venv (see
`CLAUDE.md` → Validation).

`rlm/test/test_structural.py` — Python contract tests (shape, error classes, `AmbiguousSymbolError` surface).
Runs under `prime-agent-runtime`'s existing `unittest` discovery.

**Prompt-guard admission:** add `"rlm.outline"`, `"rlm.zoom"`, `"rlm.edit_symbol"`, `"rlm.organize_imports"`,
`"rlm.ast_grep"` to the registered surface the way the port adds any new rlm function; `prompt_guards.rs` admits
them. Document the five entry points in a new `SKILL.md`-style section in `rlm/repl.md`.

**Golden system prompt:** regenerate exactly once (`PA_UPDATE_GOLDEN=1 cargo test -p pa-core --test
golden_system_prompt`). Expected delta: five tool lines in the core prompt layer. Review the diff is exactly those
lines before commit.

Full gate per `CLAUDE.md`: fmt, clippy, windows-gnu clippy, workspace build + bootstrap, workspace tests, deny,
Python unittest + ruff.

## 9. Fan-out plan — 4 disjoint teams

Per [[feedback_fanout_teams]]: disjoint surfaces, per-team `CARGO_TARGET_DIR`, serial consolidation. `/tmp/pa-gate/wt/{a,b,c,d}`
as before.

| team | files owned (writes) | acceptance test |
|---|---|---|
| **Alpha — crate skeleton + outline** | `crates/pa-structural/{Cargo.toml,README.md,src/lib.rs,src/parser/*,src/outline.rs,src/symbols.rs,src/telemetry.rs}`; new workspace member in root `Cargo.toml`; `pa-cli` feature `structural`; `tests/outline_*.rs` | `cargo test -p pa-structural outline_rust::struct_with_impls` passes; `--no-default-features` still builds pa-cli |
| **Beta — zoom + edit_symbol** | `crates/pa-structural/src/{zoom.rs,edit_symbol.rs}`; `tests/zoom_ambiguity.rs`, `tests/edit_symbol_rollback.rs`; one small addition to `pa-core::file_history` iff a new entry point is needed (coordinate with Delta first) | `zoom_ambiguity` returns the container menu; `edit_symbol_rollback` leaves file byte-identical after a /rewind-equivalent restore |
| **Gamma — organize_imports + ast_grep** | `crates/pa-structural/src/imports/*`, `crates/pa-structural/src/ast_grep.rs`; `tests/imports_organize.rs`, `tests/ast_grep_dryrun.rs`; `ast-grep-core` in Cargo.toml | `imports_organize::rust_merges_use_groups` passes; `ast_grep_dryrun::no_writes_when_dry_run` passes |
| **Delta — kernel wiring + Python surface + prompt admission** | `crates/pa-types/src/host_request/structural.rs` + registry entry; `crates/pa-core/src/kernel/shared.rs` handler registration behind a `register_fn` passed from `pa-cli`; `prime-agent-runtime/src/rlm/structural.py`, `rlm/__init__.py` export, `rlm/repl.md` frames section; `kernel/bootstrap/venv/probe.rs` `RUNTIME_READY_CHECK`; `pa-core/tests/prompt_guards.rs` admission; golden system prompt regen | `cargo test -p pa-core --test prompt_guards` admits the five names; `cargo test -p pa-core --test golden_system_prompt` diff matches the five expected lines |

Disjointness: no file is written by two teams. Beta's optional pa-core touch is pre-agreed with Delta. Gamma does
not touch `pa-core` or `pa-types` at all. Alpha owns the crate skeleton; everyone else branches from Alpha's first
commit.

Serial consolidation: land Alpha → Delta → Beta → Gamma into `pa-structural-port`. Delta unblocks live calls; Beta
and Gamma then stack on top. Each team commits to its own worktree; the parent cherry-picks in that order.

## 10. Open questions for the user

1. **Toolforge vs new crate.** `pa-toolforge` already exists as a fork-only crate. Should `pa-structural` fold into
   it, or stay separate? Recommendation: **separate** (pa-toolforge's charter is different per `AGENTS.md` crate
   list; mixing concerns would violate rule 1 "one feature, one crate"). But confirm.
2. **ast-grep-core 0.51.2 pin.** The crate is a year+ old. Should we pin that version, or take the latest 0.x with
   a compatibility check? Recommendation: **pin 0.51.2** for the port; separate bump PR later.
3. **Phase 2 languages at ship time.** Ship with Rust + Python + TypeScript + JavaScript only, or include Go + Java
   from the start? Recommendation: **phase 1 only** — ship fast, add Go + Java in a follow-up once we see real
   kernel traffic patterns.
4. **AFT lib dep refactor (mid-turn user ask).** User asked whether we should send a huge PR to cortexkit/aft to
   make its lib surface `pub`, then depend on `agent-file-tools` as a Rust lib crate. This design assumes **no**.
   Rationale: AFT's lib is deeply `pub(crate)`, it ships its own daemon (subc), its own call-graph store, its own
   backup system; making it `pub` upstream is 3-6 months of upstream negotiation, duplicates infrastructure we
   already have (codegraph, file_history, pa-lsp), and couples us to CortexKit's release cadence. Writing a native
   port is faster and cleaner. **Flag for parent to answer the user.**
