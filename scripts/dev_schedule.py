#!/usr/bin/env python3
"""Local Tervo development reminder timer.

This is intentionally not an agent scheduler: it can only log and send a
desktop notification. It cannot wake an LLM session or run coding tasks.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCHEDULE = ROOT / "scripts" / "dev_schedule.json"
STATE = ROOT / ".logs" / "dev-schedule-state.json"
LOG = ROOT / ".logs" / "dev-schedule.log"


def load_state() -> set[str]:
    try:
        data = json.loads(STATE.read_text())
        return set(data.get("done", []))
    except (OSError, ValueError):
        return set()


def save_state(done: set[str]) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps({"done": sorted(done)}, indent=2) + "\n")


def log(line: str) -> None:
    LOG.parent.mkdir(parents=True, exist_ok=True)
    with LOG.open("a", encoding="utf-8") as f:
        f.write(f"{datetime.now().isoformat(timespec='seconds')} {line}\n")


def notify(title: str, body: str) -> None:
    log(f"NOTIFY {title} | {body}")
    print(f"[dev-schedule] {title}: {body}", flush=True)
    try:
        subprocess.run(
            ["notify-send", "--app-name=Tervo Dev", "--urgency=normal", title, body],
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        log(f"notify-send unavailable: {error}")


def due(entries: list[dict], done: set[str]) -> list[dict]:
    now = datetime.now().astimezone()
    out = []
    for entry in entries:
        if entry["id"] in done:
            continue
        try:
            at = datetime.fromisoformat(entry["at"])
        except ValueError:
            log(f"bad schedule entry: {entry!r}")
            continue
        if at.tzinfo is None:
            at = at.astimezone()
        if now >= at:
            out.append(entry)
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", help="check due entries once (default)")
    mode.add_argument("--daemon", action="store_true", help="poll until stopped")
    parser.add_argument("--interval", type=int, default=20, help="poll seconds")
    args = parser.parse_args()

    try:
        entries = json.loads(SCHEDULE.read_text())
    except (OSError, ValueError) as error:
        print(f"cannot read {SCHEDULE}: {error}", file=sys.stderr)
        return 1

    while True:
        done = load_state()
        for entry in due(entries, done):
            notify(entry["title"], entry["body"])
            done.add(entry["id"])
            save_state(done)
        if not args.daemon:
            return 0
        time.sleep(max(5, args.interval))


if __name__ == "__main__":
    raise SystemExit(main())
