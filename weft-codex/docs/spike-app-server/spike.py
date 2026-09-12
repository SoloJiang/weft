#!/usr/bin/env python3
"""Stage 0 spike (spec: docs/specs/2026-08-08-codex-desktop-migration-design.md §9).

Desktop-free verification of codex app-server:
  S2  thread/start persists to the shared ~/.codex store (Desktop-visible universe)
  S4  per-thread MCP config via thread/start `config` override
  S5  mid-turn input semantics: turn/start while busy vs turn/steer vs turn/interrupt

Safe: threads are created with approvalPolicy=never + sandbox=read-only in a temp cwd.
"""

import asyncio
import json
import os
import sqlite3
import sys
import tempfile
import time

LOG = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "spike.log"), "w")
RESULTS: list[tuple[str, str, str]] = []  # (item, PASS/FAIL, evidence)


def record(item: str, ok: bool, evidence: str) -> None:
    RESULTS.append((item, "PASS" if ok else "FAIL", evidence.replace("\n", " ")[:400]))


def log(msg: str) -> None:
    LOG.write(msg + "\n")
    LOG.flush()


class AppServer:
    def __init__(self) -> None:
        self.next_id = 1
        self.pending: dict[int, asyncio.Future] = {}
        self.notifs: list[dict] = []
        self.turn_started: list[str] = []
        self.cond = asyncio.Condition()

    async def start(self) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            "codex", "app-server",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            limit=64 * 1024 * 1024,
        )
        self.reader = asyncio.create_task(self._read_loop())
        self.err = asyncio.create_task(self._err_loop())

    async def _err_loop(self) -> None:
        while True:
            line = await self.proc.stderr.readline()
            if not line:
                return
            log("STDERR " + line.decode(errors="replace").rstrip())

    async def _read_loop(self) -> None:
        while True:
            raw = await self.proc.stdout.readline()
            if not raw:
                return
            line = raw.decode(errors="replace").strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                log("NONJSON " + line[:200])
                continue
            if "method" in msg and "id" in msg:
                # Server-initiated request (approval/elicitation): refuse so nothing hangs.
                self.proc.stdin.write(
                    (json.dumps({"id": msg["id"], "error": {"code": -32000,
                     "message": "spike: auto-refused"}}) + "\n").encode())
                await self.proc.stdin.drain()
                log(f"AUTO-REFUSE {msg['method']}")
            elif "method" in msg:
                async with self.cond:
                    self.notifs.append(msg)
                    self.cond.notify_all()
                if msg["method"] == "turn/started":
                    self.turn_started.append(msg["params"].get("turn", {}).get("id"))
                log(f"NOTIF {msg['method']} {json.dumps(msg.get('params', {}))[:200]}")
            elif "id" in msg:
                fut = self.pending.pop(msg["id"], None)
                if fut and not fut.done():
                    fut.set_result(msg)

    async def request(self, method: str, params: dict, timeout: float = 60) -> dict:
        i = self.next_id
        self.next_id += 1
        fut: asyncio.Future = asyncio.get_running_loop().create_future()
        self.pending[i] = fut
        self.proc.stdin.write((json.dumps({"id": i, "method": method, "params": params}) + "\n").encode())
        await self.proc.stdin.drain()
        return await asyncio.wait_for(fut, timeout)

    async def notify(self, method: str) -> None:
        self.proc.stdin.write((json.dumps({"method": method}) + "\n").encode())
        await self.proc.stdin.drain()

    async def wait_notif(self, method: str, timeout: float = 180,
                         pred=None) -> dict | None:
        deadline = time.monotonic() + timeout
        async with self.cond:
            while True:
                for n in self.notifs:
                    if n["method"] == method and (pred is None or pred(n.get("params", {}))):
                        return n
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
                try:
                    await asyncio.wait_for(self.cond.wait(), remaining)
                except asyncio.TimeoutError:
                    return None


def check_store(thread_id: str) -> str:
    """Look for the thread id in the shared ~/.codex store (what Desktop reads)."""
    codex_home = os.path.expanduser("~/.codex")
    idx = os.path.join(codex_home, "session_index.jsonl")
    if os.path.exists(idx):
        with open(idx, errors="replace") as f:
            for line in f:
                if thread_id in line:
                    return f"session_index.jsonl hit: {line.strip()[:200]}"
    for name in os.listdir(codex_home):
        if name.startswith("state_") and name.endswith(".sqlite"):
            try:
                db = sqlite3.connect(f"file:{os.path.join(codex_home, name)}?mode=ro", uri=True)
                tables = [r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")]
                for t in tables:
                    try:
                        cols = [c[1] for c in db.execute(f"PRAGMA table_info({t})")]
                        text_cols = [c for c in cols]
                        if not text_cols:
                            continue
                        where = " OR ".join(f"CAST({c} AS TEXT) LIKE ?" for c in text_cols)
                        rows = db.execute(
                            f"SELECT * FROM {t} WHERE {where} LIMIT 1",
                            [f"%{thread_id}%"] * len(text_cols)).fetchall()
                        if rows:
                            return f"{name}:{t} hit (cols={cols[:6]})"
                    except sqlite3.Error:
                        continue
                db.close()
            except sqlite3.Error:
                continue
    sessions = os.path.join(codex_home, "sessions")
    for root, _dirs, files in os.walk(sessions):
        for fn in files:
            if thread_id in fn:
                return f"sessions/ file: {os.path.join(root, fn)}"
    return "no hit in session_index.jsonl / state_*.sqlite / sessions/"


async def main() -> None:
    srv = AppServer()
    await srv.start()

    r = await srv.request("initialize", {
        "clientInfo": {"name": "weft-spike", "version": "0.1.0"},
        "capabilities": {"experimentalApi": False}})
    assert "result" in r, f"initialize failed: {r}"
    await srv.notify("initialized")
    log(f"initialize ok: {json.dumps(r['result'])[:200]}")

    tmp = tempfile.mkdtemp(prefix="weft-spike-")

    # ── S2: thread/start + persistence ──────────────────────────────────────
    r = await srv.request("thread/start", {
        "cwd": tmp, "approvalPolicy": "never", "sandbox": "read-only"})
    if "error" in r:
        record("S2 thread/start", False, json.dumps(r["error"])[:300])
        await finish(srv)
        return
    tid = r["result"]["thread"]["id"]
    record("S2 thread/start", True, f"thread id={tid}")
    await srv.request("thread/name/set", {"threadId": tid, "name": "weft-spike-20260808"})

    await asyncio.sleep(2.0)  # let the store writer/indexer settle
    r = await srv.request("thread/list", {"limit": 100})
    log(f"thread/list body: {json.dumps(r)[:800]}")
    ids = [t.get("id") for t in r.get("result", {}).get("data", [])]
    found_plain = tid in ids
    r2 = await srv.request("thread/list", {"limit": 100, "cwd": tmp})
    ids2 = [t.get("id") for t in r2.get("result", {}).get("data", [])]
    log(f"thread/list cwd-filtered: {json.dumps(r2)[:400]}")
    record("S2 thread/list contains", found_plain or tid in ids2,
           f"plain={found_plain} (n={len(ids)}) cwd-filtered={tid in ids2} (n={len(ids2)})")

    store_ev = check_store(tid)
    record("S2 shared store persistence", "hit" in store_ev or "file:" in store_ev, store_ev)

    r = await srv.request("thread/resume", {"threadId": tid})
    record("S2 thread/resume", "result" in r, json.dumps(r)[:200])

    # ── S5: mid-turn semantics ───────────────────────────────────────────────
    r = await srv.request("turn/start", {"threadId": tid, "input": [
        {"type": "text", "text": "Count from 1 to 400, one number per line, no other text."}]})
    turn_a = r.get("result", {}).get("turn", {}).get("id")
    record("S5 turn/start A", bool(turn_a), f"turnA={turn_a}")

    await asyncio.sleep(2.0)  # turn A is streaming now

    r = await srv.request("turn/start", {"threadId": tid, "input": [
        {"type": "text", "text": "SECOND-MESSAGE-WHILE-BUSY"}]})
    turn_b = None
    if "error" in r:
        record("S5 turn/start while busy", True, f"rejected (expected): {json.dumps(r['error'])[:200]}")
    else:
        turn_b = r.get("result", {}).get("turn", {}).get("id")
        record("S5 turn/start while busy", True,
               f"accepted -> queued or concurrent: {json.dumps(r['result'])[:200]}")

    r = await srv.request("turn/steer", {"threadId": tid, "expectedTurnId": turn_a,
        "input": [{"type": "text", "text": "Stop counting immediately and reply with exactly: STEER-OK"}]})
    record("S5 turn/steer", "result" in r, json.dumps(r)[:200])

    done = await srv.wait_notif("turn/completed", timeout=180)
    status = done.get("params", {}).get("turn", {}).get("status") if done else None
    record("S5 turn A completed", done is not None, f"status={status}")

    await asyncio.sleep(6.0)  # give a possibly-queued turn B a chance to start
    b_fate = ("started" if turn_b in srv.turn_started else "never started")
    record("S5 busy-turn fate", True, f"turnB={turn_b} {b_fate}; turn/started ids={srv.turn_started}")

    r = await srv.request("thread/read", {"threadId": tid})
    body = json.dumps(r.get("result", {}))
    log(f"thread/read body: {body[:3000]}")
    r_resume = await srv.request("thread/resume", {"threadId": tid})
    body2 = json.dumps(r_resume.get("result", {}))
    log(f"thread/resume body len: {len(body2)}")
    record("S5 steer visible in history",
           "Stop counting" in body or "STEER-OK" in body or "Stop counting" in body2 or "STEER-OK" in body2,
           f"read(len={len(body)}): steer-msg={'Stop counting' in body}, STEER-OK={'STEER-OK' in body}; "
           f"resume(len={len(body2)}): steer-msg={'Stop counting' in body2}, STEER-OK={'STEER-OK' in body2}")

    # interrupt on a fresh turn
    r = await srv.request("turn/start", {"threadId": tid, "input": [
        {"type": "text", "text": "Count from 1 to 900, one number per line."}]})
    turn_c = r.get("result", {}).get("turn", {}).get("id")
    await asyncio.sleep(2.0)
    r = await srv.request("turn/interrupt", {"threadId": tid, "turnId": turn_c})
    ok = "result" in r
    done = await srv.wait_notif("turn/completed", timeout=60) if ok else None
    record("S5 turn/interrupt", ok and done is not None,
           f"ack={ok} completed_status={done.get('params', {}).get('turn', {}).get('status') if done else None}")

    # ── S4: per-thread MCP config override ──────────────────────────────────
    r = await srv.request("thread/start", {
        "cwd": tmp, "approvalPolicy": "never", "sandbox": "read-only",
        "config": {"mcp_servers": {"spikebus": {"url": "http://127.0.0.1:9/mcp"}}}})
    if "error" in r:
        record("S4 per-thread mcp config", False, json.dumps(r["error"])[:300])
    else:
        tid2 = r["result"]["thread"]["id"]
        await srv.request("thread/name/set", {"threadId": tid2, "name": "weft-spike-mcp"})
        await asyncio.sleep(3.0)  # let MCP connectors attempt
        r = await srv.request("mcpServerStatus/list", {"limit": 100})
        body = json.dumps(r.get("result", {}))
        log(f"mcpServerStatus/list body: {body[:2000]}")
        record("S4 per-thread mcp config", "spikebus" in body,
               f"spikebus in mcpServerStatus/list: {'spikebus' in body}; body={body[:300]}")
        await srv.request("thread/archive", {"threadId": tid2})

    # NOTE: tid intentionally left unarchived and named weft-spike-20260808 so the
    # deferred Desktop visual check (spec §9 item 2) can find it in the thread list.
    await finish(srv)


async def finish(srv: AppServer) -> None:
    print("\n===== SPIKE RESULTS =====")
    for item, status, ev in RESULTS:
        print(f"[{status}] {item}\n       {ev}")
    srv.proc.terminate()
    try:
        await asyncio.wait_for(srv.proc.wait(), 5)
    except asyncio.TimeoutError:
        srv.proc.kill()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception as e:  # summary must print even on hard failure
        print(f"\nSPIKE CRASHED: {e!r}", file=sys.stderr)
        for item, status, ev in RESULTS:
            print(f"[{status}] {item}\n       {ev}")
        sys.exit(1)
