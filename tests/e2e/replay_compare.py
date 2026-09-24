#!/usr/bin/env python3 -B
"""Replay recorded Claude Code hook traffic through the CLI edition and diff
what it would say against a baseline would-say.jsonl.

A behaviour check for `heard daemon`: given the same hook traffic, the daemon
must say exactly what a reference run (for example an earlier build, or the
Python reference implementation) said.

Inputs (bring your own; none is included in this repo):

    --traffic DIR   dir with payloads.jsonl ({"i","start","upto","payload"} per
                    hook event) and source-transcript.jsonl (the transcript the
                    events were fired against)            [env HXE_TRAFFIC]
    --baseline F    the baseline would-say.jsonl          [env HXE_BASELINE]
    --bin DIR       dir with the built `heard` and `heard-hook` (target/debug)
                                                          [env HRC_BIN]
    --out DIR       scratch output dir (default: $TMPDIR/hrc-replay); its
                    path must be short (the socket lives in it)  [env OUT]

Procedure: the transcript copy starts with the lines before the window;
before each event it grows to the line that event was fired at; then the
REAL `heard-hook claude-code` (CLI edition, HEARD_CLI_HOME = a sandbox root)
gets the payload on stdin, with `transcript_path` pointed at the copy. IDLE_S
before the first event, GAP_S after each event (STOP_GAP_S after a Stop),
DRAIN_S at the end. The daemon is
`heard daemon --speech log --tts null --notifier log`: it records, never
speaks, never posts a notification. Provider keys are removed.

Output: counts only (matched / different / missing / extra) and, with -v,
the differing lines' kind/tag/via and length — never their text, so the
output is safe to share even when the traffic is not.

Run:  python3 -B tests/e2e/replay_compare.py --traffic DIR --baseline FILE --bin DIR [-v]
"""
from __future__ import annotations

import argparse
import difflib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

# A sandbox config (hand-written, secret-free): the settings the baseline is
# expected to have run with. Everything that could reach a human sense or a
# device is off.
SANDBOX_CONFIG = """\
greeted: true
onboarded: true
muted: false
hotkey_enabled: false
auto_silence_on_mic: false
push_to_talk: false
voice_mode: "off"
voice_prompt_announce: false
"""

SCRUB = ["ANTHROPIC_API_KEY", "ELEVENLABS_API_KEY"]


def scrub(env: dict) -> None:
    """Remove provider credentials: the explicit names plus every *_API_KEY / *_TOKEN."""
    for k in list(env):
        if k in SCRUB or k.endswith("_API_KEY") or k.endswith("_TOKEN"):
            del env[k]
KEY = ("text", "kind", "tag", "via")


def arg_path(value: str | None, flag: str, name: str, default: str | None = None) -> Path:
    v = value or os.environ.get(name, default)
    if not v:
        raise SystemExit(f"pass {flag} or set {name}")
    return Path(v)


def load(path: Path) -> list[dict]:
    return [json.loads(l) for l in path.read_text(encoding="utf-8").splitlines() if l.strip()]


def replay(traffic: Path, bindir: Path, out: Path) -> Path:
    if out.exists():
        shutil.rmtree(out)
    root, home, tmp = out / "st", out / "home", out / "tmp"
    for d in (root, home, tmp):
        d.mkdir(parents=True)
    (root / "config.yaml").write_text(SANDBOX_CONFIG)
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "LANG": "en_US.UTF-8",
        "HOME": str(home),
        "TMPDIR": str(tmp) + "/",
        "TERM_PROGRAM": "e2e-harness",
        "HEARD_CLI_HOME": str(root),
        "HEARD_DAEMON_SPEECH": "log",
        "HEARD_DAEMON_TTS": "null",
        "HEARD_DAEMON_NOTIFIER": "log",
        "NO_COLOR": "1",
    }
    scrub(env)
    heard, hook = str(bindir / "heard"), str(bindir / "heard-hook")
    gap = float(os.environ.get("GAP_S", "0.6"))
    stop_gap = float(os.environ.get("STOP_GAP_S", "3.0"))
    drain = float(os.environ.get("DRAIN_S", "30"))
    limit = int(os.environ.get("LIMIT", "0"))

    payloads = load(traffic / "payloads.jsonl")
    if limit:
        payloads = payloads[:limit]
    src = (traffic / "source-transcript.jsonl").read_text(encoding="utf-8").splitlines(True)
    live = out / "transcript.jsonl"
    written = payloads[0]["start"]
    live.write_text("".join(src[:written]), encoding="utf-8")

    r = subprocess.run([heard, "start"], env=env, capture_output=True, text=True, timeout=30)
    if r.returncode != 0:
        raise SystemExit(f"heard start failed: {r.stdout}{r.stderr}")
    try:
        time.sleep(float(os.environ.get("IDLE_S", "0")))
        t0 = time.time()
        for n, rec in enumerate(payloads):
            upto = rec["upto"]
            if upto > written:
                with open(live, "a", encoding="utf-8") as f:
                    f.write("".join(src[written:upto]))
                written = upto
            pl = dict(rec["payload"], transcript_path=str(live))
            subprocess.run([hook, "claude-code"], input=json.dumps(pl).encode(), env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
            time.sleep(stop_gap if pl.get("hook_event_name") == "Stop" else gap)
            if n % 50 == 0:
                print(f"fed {n}/{len(payloads)} t={time.time() - t0:.0f}s", flush=True)
        time.sleep(drain)
    finally:
        subprocess.run([heard, "stop"], env=env, capture_output=True, timeout=30)
    return root / "would-say.jsonl"


def compare(baseline: list[dict], ours: list[dict], verbose: bool) -> dict:
    a = [tuple(l.get(k, "") for k in KEY) for l in baseline]
    b = [tuple(l.get(k, "") for k in KEY) for l in ours]
    sm = difflib.SequenceMatcher(a=a, b=b, autojunk=False)
    res = {"baseline": len(a), "ours": len(b), "matched": 0, "different": 0, "missing": 0, "extra": 0}
    for op, i1, i2, j1, j2 in sm.get_opcodes():
        if op == "equal":
            res["matched"] += i2 - i1
            continue
        if op == "replace":
            k = min(i2 - i1, j2 - j1)
            res["different"] += k
            res["missing"] += (i2 - i1) - k
            res["extra"] += (j2 - j1) - k
        elif op == "delete":
            res["missing"] += i2 - i1
        elif op == "insert":
            res["extra"] += j2 - j1
        if verbose:
            for x in a[i1:i2]:
                print(f"  - baseline #{a.index(x)} kind={x[1]} tag={x[2]} via={x[3]} chars={len(x[0])}")
            for y in b[j1:j2]:
                print(f"  + ours     kind={y[1]} tag={y[2]} via={y[3]} chars={len(y[0])}")
    return res


def main() -> None:
    ap = argparse.ArgumentParser(description="Replay hook traffic through heard daemon and diff the would-say lines.")
    ap.add_argument("--traffic")
    ap.add_argument("--baseline")
    ap.add_argument("--bin")
    ap.add_argument("--out")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    verbose = args.verbose
    traffic = arg_path(args.traffic, "--traffic", "HXE_TRAFFIC")
    baseline = arg_path(args.baseline, "--baseline", "HXE_BASELINE")
    bindir = arg_path(args.bin, "--bin", "HRC_BIN")
    out = arg_path(args.out, "--out", "OUT", os.path.join(os.environ.get("TMPDIR", "/tmp"), "hrc-replay"))
    if os.environ.get("REPLAY_ONLY_COMPARE") != "1":
        ours_path = replay(traffic, bindir, out)
    else:
        ours_path = out / "st" / "would-say.jsonl"
    ours = load(ours_path) if ours_path.exists() else []
    res = compare(load(baseline), ours, verbose)
    notes = out / "st" / "notifications.jsonl"
    res["notifications_logged"] = len(load(notes)) if notes.exists() else 0
    print(json.dumps(res))
    sys.exit(0 if res["matched"] == res["baseline"] == res["ours"] else 1)


if __name__ == "__main__":
    main()
