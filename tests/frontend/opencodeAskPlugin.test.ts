import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

/**
 * Runs the REAL opencode Ask Bridge plugin source that
 * `src-tauri/src/bus/inject.rs::inject_opencode_ask_plugin` writes into a
 * worktree (it `include_str!`s the same file), with `fetch` stubbed to stand in
 * for weft. The plugin is the last line of defense for opencode sessions: weft's
 * Needs-you card is the only thing that shows a tool call to a human, so
 * anything short of an explicit "allow" from weft must throw (opencode's
 * `tool.execute.before` denies by throwing).
 */
const PLUGIN_SRC = new URL(
  "../../src-tauri/src/bus/weft-ask-plugin.js",
  import.meta.url,
);
const ASK_URL = "http://127.0.0.1:65535/ask/1/10?tool=opencode";

type FetchStub = (url: string, init: RequestInit) => Promise<Response>;

let loads = 0;

/** Load the plugin with `__URL__` substituted and `fetch` stubbed, then return
 *  its `tool.execute.before` handler. Imported via a data: URL so no temp file
 *  is involved, with a per-load comment so the module cache hands back a fresh
 *  instance each time. */
async function loadHandler(fetchStub: FetchStub) {
  loads += 1;
  const src = readFileSync(PLUGIN_SRC, "utf8").replace("__URL__", ASK_URL);
  assert.ok(!src.includes("__URL__"), "plugin must have exactly one URL slot");
  (globalThis as unknown as { fetch: FetchStub }).fetch = fetchStub;
  const mod = await import(
    "data:text/javascript," + encodeURIComponent(`${src}\n// load ${loads}\n`)
  );
  const hooks = await mod.WeftAsk({});
  return hooks["tool.execute.before"] as (
    input: { tool: string },
    output: { args: unknown },
  ) => Promise<void>;
}

const CALL = [{ tool: "bash" }, { args: { command: "rm -rf /" } }] as const;

/** A weft response carrying `decision`, in the Ask Bridge's wire shape. */
function decision(value: string, status = 200): Response {
  return new Response(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: value,
        permissionDecisionReason: "test",
      },
    }),
    { status, headers: { "Content-Type": "application/json" } },
  );
}

test("weft unreachable denies instead of falling through", async () => {
  let posted = 0;
  const handler = await loadHandler(async () => {
    posted += 1;
    throw new TypeError("fetch failed");
  });
  await assert.rejects(
    () => handler(...CALL),
    /unreachable/,
    "a down weft must DENY (throw), not silently allow the tool call",
  );
  assert.equal(posted, 1, "the plugin must have actually tried to ask weft");
});

test("explicit deny from weft throws", async () => {
  const handler = await loadHandler(async () => decision("deny"));
  await assert.rejects(() => handler(...CALL), /Denied in weft/);
});

test("explicit allow from weft proceeds", async () => {
  const seen: Array<[string, RequestInit]> = [];
  const handler = await loadHandler(async (url, init) => {
    seen.push([url, init]);
    return decision("allow");
  });
  await handler(...CALL);
  assert.equal(seen.length, 1);
  assert.equal(seen[0][0], ASK_URL);
  assert.deepEqual(JSON.parse(String(seen[0][1].body)), {
    tool_name: "bash",
    tool_input: { command: "rm -rf /" },
  });
});

test("a non-decision response denies", async () => {
  // weft answered, but not with a decision (version skew, wrong route, a proxy
  // in between): nobody reviewed the call, so it is still denied.
  for (const [label, body] of [
    ["empty object", new Response("{}", { status: 200 })],
    ["unknown verdict", decision("maybe")],
    ["non-JSON body", new Response("<html>nope</html>", { status: 200 })],
    ["server error", decision("allow", 500)],
  ] as const) {
    const handler = await loadHandler(async () => body.clone());
    await assert.rejects(
      () => handler(...CALL),
      /undecided|no decision|without a decision/,
      `${label} must deny`,
    );
  }
});
