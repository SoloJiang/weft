#!/usr/bin/env python3
# One-off probe: does thread/start's `threadSource` land in sqlite
# threads.thread_source?
#
# Context: codex 0.145.0 has NO `--session-source` CLI flag (that exists only
# in newer codex-rs git source), so `threads.source` stays "vscode" for
# app-server threads. The v2 protocol's ThreadStartParams.threadSource is a
# client-supplied classification string; this probe checks where it persists.

import asyncio
import json
import sqlite3
import sys
import tempfile
from pathlib import Path

DB = Path.home() / ".codex" / "state_5.sqlite"


async def main() -> int:
    proc = await asyncio.create_subprocess_exec(
        "codex", "app-server",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    assert proc.stdin and proc.stdout

    async def request(rid: int, method: str, params: dict) -> dict:
        payload = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        proc.stdin.write(json.dumps(payload).encode() + b"\n")
        await proc.stdin.drain()
        while True:
            line = await proc.stdout.readline()
            msg = json.loads(line)
            if msg.get("id") == rid:
                return msg

    await request(1, "initialize", {
        "clientInfo": {"name": "probe", "version": "0.0.0"},
        "capabilities": {"experimentalApi": False},
    })
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "initialized"}).encode() + b"\n")
    await proc.stdin.drain()

    cwd = tempfile.mkdtemp(prefix="weft-tsprobe-")
    resp = await request(2, "thread/start", {
        "cwd": cwd,
        "approvalPolicy": "never",
        "sandbox": "read-only",
        "threadSource": "weft-codex",
    })
    thread = resp.get("result", {}).get("thread", {})
    tid = thread.get("id", "")
    print("thread.id:", tid)
    print("response thread.source:", thread.get("source"))
    print("response thread.threadSource:", thread.get("threadSource"))

    # The threads row only materializes once the thread actually does work
    # (thread/start alone persists nothing), so run one minimal turn.
    turn_resp = await request(3, "turn/start", {
        "threadId": tid,
        "input": [{"type": "text", "text": "Reply with exactly: ok"}],
    })
    print("turn/start error:", turn_resp.get("error"))

    row = None
    for attempt in range(30):
        await asyncio.sleep(1.0)
        con = sqlite3.connect(DB)
        row = con.execute(
            "select source, thread_source from threads where id = ?", (tid,)
        ).fetchone()
        con.close()
        if row:
            print(f"sqlite row (after {attempt + 1}s):", row)
            break
    if not row:
        print("sqlite row: None after 30s")

    proc.terminate()
    await proc.wait()
    ok = bool(row) and row[1] == "weft-codex"
    print("PROBE", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
