import test from "node:test";
import assert from "node:assert/strict";
import type { ToolStatus } from "../../src/lib/types.ts";
import { spawnableToolsOf } from "../../src/lib/toolStatus.ts";

function tool(name: string, spawnable: boolean): ToolStatus {
  return {
    tool: name,
    installed: true,
    spawnable,
    version: null,
    path: null,
    meets_min: true,
    diagnostics: [],
  };
}

test("spawnable tool options exclude installed diagnostic-only tools", () => {
  const tools = spawnableToolsOf([tool("codex", false), tool("claude", true)]);
  assert.deepEqual(tools.map((candidate) => candidate.tool), ["claude"]);
});
