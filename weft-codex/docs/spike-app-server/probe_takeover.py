#!/usr/bin/env python3
# Human-takeover simulation (spec: migration design §6 injection boundary).
#
# A SECOND app-server process plays the role of Desktop: it resumes an
# orchestrated thread and starts its own turn on it. While that foreign
# turn runs, weftd's message API is called — the expected behavior is:
#   1. weftd's watcher sees turn/started with an unknown id → flags the
#      thread foreign (SSE: thread.human-active)
#   2. the message parks instead of injecting (SSE: bus.parked)
#   3. when the foreign turn ends, weftd flushes the backlog (direction
#      status flips to working as the parked message starts OUR turn)
#
# Usage: probe_takeover.py <thread_id> <weftd_base_url> <direction_id>

import asyncio
import json
import sys
import urllib.request


async def main() -> int:
    thread_id, base, direction_id = sys.argv[1], sys.argv[2], sys.argv[3]
    proc = await asyncio.create_subprocess_exec(
        "codex", "app-server",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    assert proc.stdin and proc.stdout
    done = asyncio.Event()

    async def read_loop() -> None:
        while True:
            line = await proc.stdout.readline()
            if not line:
                return
            msg = json.loads(line)
            method = msg.get("method", "")
            if method == "turn/completed":
                print("foreign turn completed", flush=True)
                done.set()
            elif "id" in msg and "result" in msg:
                fut = pending.get(msg["id"])
                if fut and not fut.done():
                    fut.set_result(msg)

    pending: dict[int, asyncio.Future] = {}
    reader = asyncio.create_task(read_loop())

    async def request(rid: int, method: str, params: dict) -> dict:
        fut: asyncio.Future = asyncio.get_event_loop().create_future()
        pending[rid] = fut
        payload = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        proc.stdin.write(json.dumps(payload).encode() + b"\n")
        await proc.stdin.drain()
        return await asyncio.wait_for(fut, 30)

    await request(1, "initialize", {
        "clientInfo": {"name": "takeover-probe", "version": "0.0.0"},
        "capabilities": {"experimentalApi": False},
    })
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "initialized"}).encode() + b"\n")
    await proc.stdin.drain()
    await request(2, "thread/resume", {"threadId": thread_id})
    resp = await request(3, "turn/start", {
        "threadId": thread_id,
        "input": [{"type": "text", "text": "Reply with exactly: foreign-ok"}],
    })
    turn = resp.get("result", {}).get("turn", {})
    print("foreign turn started:", turn.get("id"), flush=True)

    # Give weftd's watcher a moment to route the foreign turn/started, then
    # act like the human ALSO typing in weftd's own UI mid-takeover.
    await asyncio.sleep(2.0)
    body = json.dumps({"text": "parked while you drive"}).encode()
    req = urllib.request.Request(
        f"{base}/api/directions/{direction_id}/message",
        data=body,
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        print("weftd message api:", r.status, r.read().decode(), flush=True)

    await asyncio.wait_for(done.wait(), 120)
    await asyncio.sleep(2.0)  # let weftd's TurnEnd fold + flush run
    proc.terminate()
    reader.cancel()
    await proc.wait()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
