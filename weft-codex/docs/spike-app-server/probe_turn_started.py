#!/usr/bin/env python3
# Probe: does turn/started arrive for OUR OWN turn (same app-server process)?
# Prints every server->client notification method seen during one small turn.

import asyncio
import json
import sys
import tempfile


async def main() -> int:
    proc = await asyncio.create_subprocess_exec(
        "codex", "app-server",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    assert proc.stdin and proc.stdout
    seen: list[str] = []
    done = asyncio.Event()

    async def read_loop() -> None:
        while True:
            line = await proc.stdout.readline()
            if not line:
                return
            msg = json.loads(line)
            method = msg.get("method")
            if method:
                if method not in ("item/agentMessage/delta",):
                    seen.append(method)
                if method == "turn/completed":
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
        "clientInfo": {"name": "ts-probe", "version": "0.0.0"},
        "capabilities": {"experimentalApi": False},
    })
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "initialized"}).encode() + b"\n")
    await proc.stdin.drain()
    cwd = tempfile.mkdtemp(prefix="weft-tstarted-")
    resp = await request(2, "thread/start", {
        "cwd": cwd, "approvalPolicy": "never", "sandbox": "read-only",
    })
    tid = resp["result"]["thread"]["id"]
    await request(3, "turn/start", {
        "threadId": tid,
        "input": [{"type": "text", "text": "Reply with exactly: ok"}],
    })
    await asyncio.wait_for(done.wait(), 120)
    proc.terminate()
    reader.cancel()
    await proc.wait()
    got = "turn/started" in seen
    print("notifications seen:", seen)
    print("TURN_STARTED_SAME_PROCESS", "YES" if got else "NO")
    return 0 if got else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
