#!/usr/bin/env python3
"""Offline cross-tool backfill generator for the Engineer Trajectory Index (ETI).

Walks every discoverable coding-session corpus on this machine (prime, Claude
Code, opencode), buckets records by UTC calendar DAY, and emits one
LearningDay-shaped file per (corpus, day) so the trajectory reader ingests it
with zero special-casing:

    <out>/<corpus>/<YYYY-MM-DD>.json   (file mode 0600, dir 0700)

Each emitted day carries only aggregate counts and STRUCTURAL error-class tokens
(class names, well-known failure strings). No message content is written. Every
datum is corpus/tool-tagged with a synthetic fingerprint that can never collide
with a runtime ledger id, so a cross-tool comparison is confounded by
construction (the ETI reader flags all three confounds on these windows).

Guarantees, matching the read-only sweep this is adapted from:
  * Nothing here calls a model provider or touches the network.
  * The corpora are only READ; nothing is written back into them.
  * Auth/secret files are never opened (see SKIP_NAMES).

The output is baseline/study data for `prime-agent learning trajectory
--include-backfill`. It is NEVER written into trajectory.json or the prompt.
"""
import argparse
import collections
import datetime
import glob
import hashlib
import json
import os
import re
import sys

# Never open these, even if encountered under a corpus root.
SKIP_NAMES = {"auth.json", "anthropic-auth-state.json"}

# ---- error / exception class extraction -------------------------------------
# STRUCTURAL error tokens only (class names, well-known failure strings), never
# surrounding message content.
_EXC_RE = re.compile(r"\b([A-Z][A-Za-z0-9_]*(?:Error|Exception|Warning))\b")
_RUST_RE = re.compile(r"\berror\[(E\d{2,4})\]")
_TS_RE = re.compile(r"\b(TS\d{3,5})\b")


def error_tokens(text):
    """Return a set of structural error-class tokens found in text. No content."""
    if not text:
        return ()
    out = set()
    if "Traceback (most recent call last)" in text:
        out.add("py:Traceback")
    for m in _EXC_RE.findall(text):
        out.add("exc:" + m)
    for m in _RUST_RE.findall(text):
        out.add("rustc:" + m)
    for m in _TS_RE.findall(text):
        out.add("tsc:" + m)
    low = text.lower()
    if "command not found" in low:
        out.add("sh:command-not-found")
    if "no such file or directory" in low:
        out.add("sh:no-such-file")
    if "permission denied" in low:
        out.add("sh:permission-denied")
    if text.startswith("fatal:") or "\nfatal:" in text:
        out.add("git:fatal")
    if "npm err!" in low:
        out.add("npm:err")
    # cap tokens per blob so one huge log cannot dominate
    return tuple(sorted(out))[:8]


def iso_week(dt):
    y, w, _ = dt.isocalendar()
    return f"{y}-W{w:02d}"


def parse_iso_ts(s):
    if not isinstance(s, str):
        return None
    try:
        s2 = s.replace("Z", "+00:00")
        return datetime.datetime.fromisoformat(s2)
    except Exception:
        return None


def ms_to_dt(ms):
    try:
        return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc)
    except Exception:
        return None


def day_key(dt):
    """UTC calendar day 'YYYY-MM-DD' for a datetime (naive datetimes read as UTC)."""
    if dt.tzinfo is not None:
        dt = dt.astimezone(datetime.timezone.utc)
    return dt.date().isoformat()


def norm_cwd(home, p):
    if not p or not isinstance(p, str):
        return None
    # collapse home to ~ so distinct-project counts are stable; no secrets here
    if p.startswith(home):
        return "~" + p[len(home):]
    return p


class DayAgg:
    """Per (corpus, day) accumulator: message counts and (tool, token) errors."""

    __slots__ = ("user_msgs", "assistant_msgs", "errors")

    def __init__(self):
        self.user_msgs = 0
        self.assistant_msgs = 0
        # (tool, token) -> count
        self.errors = collections.Counter()


def new_bucket():
    return collections.defaultdict(DayAgg)


def add_errors(agg, tool, blob):
    for tok in error_tokens(blob):
        agg.errors[(tool or "tool", tok)] += 1


# ---------------------------------------------------------------- PRIME -------
def walk_prime(data, prime_dir):
    files = glob.glob(os.path.join(prime_dir, "**", "*.jsonl"), recursive=True)
    bucket = data["prime"]
    for f in files:
        if os.path.basename(f) in SKIP_NAMES:
            continue
        try:
            fh = open(f, encoding="utf-8", errors="replace")
        except Exception:
            continue
        with fh:
            for ln in fh:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    d = json.loads(ln)
                except Exception:
                    continue
                if d.get("type") != "message":
                    continue
                dt = parse_iso_ts(d.get("timestamp"))
                if dt is None:
                    continue
                day = day_key(dt)
                agg = bucket[day]
                m = d.get("message") or {}
                role = m.get("role")
                content = m.get("content")
                if role == "user":
                    agg.user_msgs += 1
                elif role == "assistant":
                    agg.assistant_msgs += 1
                if isinstance(content, list):
                    for p in content:
                        if not isinstance(p, dict):
                            continue
                        if p.get("type") in ("toolResult", "text") and role == "toolResult":
                            txt = p.get("text")
                            if isinstance(txt, str):
                                add_errors(agg, p.get("name") or "tool", txt)


# --------------------------------------------------------------- CLAUDE -------
def walk_claude(data, claude_dir):
    files = glob.glob(os.path.join(claude_dir, "**", "*.jsonl"), recursive=True)
    bucket = data["claude"]
    for f in files:
        if os.path.basename(f) in SKIP_NAMES:
            continue
        try:
            fh = open(f, encoding="utf-8", errors="replace")
        except Exception:
            continue
        with fh:
            for ln in fh:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    d = json.loads(ln)
                except Exception:
                    continue
                dt = parse_iso_ts(d.get("timestamp"))
                if dt is None:
                    continue
                if d.get("type") not in ("user", "assistant"):
                    continue
                day = day_key(dt)
                agg = bucket[day]
                m = d.get("message") or {}
                role = m.get("role")
                content = m.get("content")
                # Honor isSidechain/isMeta so agent-to-agent turns do not inflate the denominator.
                is_side = bool(d.get("isSidechain"))
                is_meta = bool(d.get("isMeta"))
                has_text = False
                has_tool_result = False
                if isinstance(content, list):
                    for p in content:
                        if not isinstance(p, dict):
                            continue
                        pt = p.get("type")
                        if pt == "tool_result":
                            has_tool_result = True
                            if p.get("is_error"):
                                agg.errors[("tool", "tool_result:is_error")] += 1
                            c = p.get("content")
                            blob = None
                            if isinstance(c, str):
                                blob = c
                            elif isinstance(c, list):
                                parts = [cc["text"] for cc in c if isinstance(cc, dict) and isinstance(cc.get("text"), str)]
                                blob = "\n".join(parts) if parts else None
                            if blob:
                                add_errors(agg, "tool", blob)
                        elif pt == "text":
                            has_text = True
                elif isinstance(content, str):
                    has_text = True
                if role == "assistant" and not is_side and not is_meta:
                    agg.assistant_msgs += 1
                elif role == "user" and has_text and not has_tool_result and not is_meta and not is_side:
                    agg.user_msgs += 1


# ------------------------------------------------------------- OPENCODE -------
def walk_opencode(data, store):
    sess_root = os.path.join(store, "session", "global")
    msg_root = os.path.join(store, "message")
    part_root = os.path.join(store, "part")
    if not os.path.isdir(sess_root):
        return
    bucket = data["opencode"]

    # 1) sessions: id -> (created_dt, is_subagent)
    sess_meta = {}
    for f in glob.glob(os.path.join(sess_root, "*.json")):
        try:
            d = json.load(open(f, encoding="utf-8", errors="replace"))
        except Exception:
            continue
        sid = d.get("id")
        if not sid:
            continue
        created = (d.get("time") or {}).get("created")
        dt = ms_to_dt(created) if created else None
        sess_meta[sid] = (dt, bool(d.get("parentID")))

    # 2) messages: msg_id -> day ; count user/assistant msgs (skip subagent user turns)
    msg_day = {}
    if os.path.isdir(msg_root):
        for sd in os.scandir(msg_root):
            if not sd.is_dir():
                continue
            smeta = sess_meta.get(sd.name)
            is_sub = smeta[1] if smeta else False
            for mf in os.scandir(sd.path):
                if not mf.name.endswith(".json"):
                    continue
                try:
                    d = json.load(open(mf.path, encoding="utf-8", errors="replace"))
                except Exception:
                    continue
                created = (d.get("time") or {}).get("created")
                dt = ms_to_dt(created) if created else (smeta[0] if smeta else None)
                if dt is None:
                    continue
                day = day_key(dt)
                mid = d.get("id")
                if mid:
                    msg_day[mid] = day
                agg = bucket[day]
                role = d.get("role")
                if role == "user" and not is_sub:
                    agg.user_msgs += 1
                elif role == "assistant":
                    agg.assistant_msgs += 1

    # 3) parts: per-tool error tokens, joined to the message's day
    if os.path.isdir(part_root):
        for pd in os.scandir(part_root):
            if not pd.is_dir():
                continue
            day = msg_day.get(pd.name)
            for pf in os.scandir(pd.path):
                if not pf.name.endswith(".json"):
                    continue
                try:
                    d = json.load(open(pf.path, encoding="utf-8", errors="replace"))
                except Exception:
                    continue
                dd = day
                if dd is None:
                    pt_created = (d.get("time") or {}).get("start") if isinstance(d.get("time"), dict) else None
                    dt = ms_to_dt(pt_created) if pt_created else None
                    if dt:
                        dd = day_key(dt)
                if dd is None or d.get("type") != "tool":
                    continue
                agg = bucket[dd]
                tool = d.get("tool") or "?"
                st = d.get("state") or {}
                if isinstance(st, dict):
                    if st.get("status") == "error":
                        agg.errors[(tool, "tool:error")] += 1
                    out = st.get("output")
                    if isinstance(out, str):
                        add_errors(agg, tool, out)


def synthetic_fingerprint(corpus, tool, token):
    """Stable, corpus-scoped id that can never collide with a runtime ledger id."""
    raw = f"{corpus}\0{tool}\0{token}".encode("utf-8")
    return hashlib.sha1(raw).hexdigest()[:16]


def emit(out_dir, data, now_iso):
    written = 0
    for corpus, days in data.items():
        for day, agg in days.items():
            fingerprints = []
            for (tool, token), count in sorted(agg.errors.items()):
                fingerprints.append({
                    "fingerprint": synthetic_fingerprint(corpus, tool, token),
                    "name": token,
                    "status": "error",
                    "failure": True,
                    "count": count,
                    "message": token,
                })
            turns = agg.user_msgs + agg.assistant_msgs
            if turns == 0 and not fingerprints:
                continue
            record = {
                "schema": 1,
                "day": day,
                "sealedAt": now_iso,
                "turns": turns,
                "parseErrors": 0,
                "sourceFiles": [],
                "commits": [],
                "fingerprints": fingerprints,
            }
            corpus_dir = os.path.join(out_dir, corpus)
            os.makedirs(corpus_dir, mode=0o700, exist_ok=True)
            try:
                os.chmod(corpus_dir, 0o700)
            except OSError:
                pass
            path = os.path.join(corpus_dir, f"{day}.json")
            with open(path, "w", encoding="utf-8") as fh:
                json.dump(record, fh, indent=2)
                fh.write("\n")
            try:
                os.chmod(path, 0o600)
            except OSError:
                pass
            written += 1
    return written


def main(argv=None):
    home = os.path.expanduser("~")
    parser = argparse.ArgumentParser(description="Offline cross-tool backfill generator for the ETI (read-only).")
    parser.add_argument("--home", default=home, help="Home directory to collapse in paths and derive defaults from.")
    parser.add_argument("--out", default=None, help="Output dir (default: <home>/.prime/agent/learning/backfill).")
    parser.add_argument("--prime-dir", default=None, help="prime sessions dir (default: <home>/.prime/agent/sessions).")
    parser.add_argument("--claude-dir", default=None, help="Claude Code projects dir (default: <home>/.claude/projects).")
    parser.add_argument("--opencode-store", default=None, help="opencode storage dir (default: <home>/.local/share/opencode/storage).")
    parser.add_argument(
        "--corpus",
        action="append",
        choices=["prime", "claude", "opencode"],
        help="Limit to one or more corpora (default: all).",
    )
    args = parser.parse_args(argv)

    home = args.home
    out_dir = args.out or os.path.join(home, ".prime", "agent", "learning", "backfill")
    prime_dir = args.prime_dir or os.path.join(home, ".prime", "agent", "sessions")
    claude_dir = args.claude_dir or os.path.join(home, ".claude", "projects")
    opencode_store = args.opencode_store or os.path.join(home, ".local", "share", "opencode", "storage")
    corpora = set(args.corpus or ["prime", "claude", "opencode"])

    data = {"prime": new_bucket(), "claude": new_bucket(), "opencode": new_bucket()}
    if "prime" in corpora:
        print("walking prime...", file=sys.stderr)
        walk_prime(data, prime_dir)
    if "claude" in corpora:
        print("walking claude...", file=sys.stderr)
        walk_claude(data, claude_dir)
    if "opencode" in corpora:
        print("walking opencode (this can be large; be patient)...", file=sys.stderr)
        walk_opencode(data, opencode_store)

    now_iso = datetime.datetime.now(datetime.timezone.utc).isoformat()
    os.makedirs(out_dir, mode=0o700, exist_ok=True)
    written = emit(out_dir, data, now_iso)
    print(f"wrote {written} backfill day file(s) under {out_dir}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
