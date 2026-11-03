#!/usr/bin/env python3
"""Measure session index scans on synthetic, append-only Claude JSONL transcripts.

Usage: python3 scripts/bench-session-index.py [path/to/mdkb]
Each case uses an isolated HOME and store. Report wall-clock medians, not a CI gate.
"""
import json
import os
import sqlite3
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time


def run(binary, root, home, *args):
    env = dict(os.environ, HOME=str(home), USERPROFILE=str(home), MDKB_NO_DAEMON="1", MDKB_NO_SPAWN="1")
    start = time.perf_counter()
    out = subprocess.run([str(binary), *args], cwd=root, env=env, capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    if out.returncode:
        raise RuntimeError(f"mdkb {' '.join(args)}: {out.stderr} {out.stdout}")
    return elapsed, out.stdout


def append_turns(path, first, end):
    with path.open("a", encoding="utf-8") as file:
        for n in range(first, end):
            for role in ("user", "assistant"):
                record = {"type": role, "sessionId": "bench-session", "message": {"role": role,
                          "content": f"Turn {n}: describe the distinct architecture decision and its consequences."}}
                file.write(json.dumps(record) + "\n")


def measure(binary, turns):
    with tempfile.TemporaryDirectory(prefix="mdkb-session-bench-") as name:
        base = Path(name)
        root = base / "repo"
        root.mkdir()
        home = base / "home"
        home.mkdir()
        sessions_base = home / ".claude" / "projects"
        session_dir = sessions_base / str(root).replace("/", "-").replace(".", "-")
        session_dir.mkdir(parents=True)
        source = session_dir / "bench-session.jsonl"
        append_turns(source, 0, turns)
        run(binary, root, home, "init")
        args = ("--format", "json", "session", "index", "--sessions-path", str(sessions_base),
                "--project-root", str(root))
        cold, cold_out = run(binary, root, home, *args)
        if json.loads(cold_out)["added"] < 1:
            raise RuntimeError(f"cold scan did not index the transcript: {cold_out}")
        warm_runs = [run(binary, root, home, *args) for _ in range(3)]
        if any(json.loads(out)["unchanged"] < 1 for _, out in warm_runs):
            raise RuntimeError("warm scan did not find indexed transcript chunks")
        append_turns(source, turns, turns + 2)
        appended, appended_out = run(binary, root, home, *args)
        if json.loads(appended_out)["added"] + json.loads(appended_out)["updated"] < 1:
            raise RuntimeError(f"append did not update indexed chunks: {appended_out}")
        with sqlite3.connect(root / ".mdkb" / "index.sqlite") as conn:
            rows = conn.execute("SELECT COUNT(*) FROM documents WHERE collection='claude_sessions'").fetchone()[0]
        if rows < 1:
            raise RuntimeError("no session documents persisted after index")
        return {"turns": turns, "jsonl_bytes": source.stat().st_size, "indexed_docs": rows,
                "cold_s": round(cold, 4),
                "warm_median_s": round(statistics.median(t for t, _ in warm_runs), 4),
                "append_two_turns_s": round(appended, 4)}


if __name__ == "__main__":
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/mdkb").resolve()
    if not binary.is_file():
        sys.exit(f"build the binary first: {binary}")
    for turns in (100, 1000, 5000):
        print(json.dumps(measure(binary, turns)), flush=True)
