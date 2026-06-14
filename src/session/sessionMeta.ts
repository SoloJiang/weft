import type { McpServerInfo, SessionMeta } from "../lib/types";

/** server 名与 tool 前缀的非字母数字差异(如 `plugin:context7:context7` ↔
 *  `plugin_context7_context7`)规范化后比较。 */
const norm = (s: string) =>
  s.toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");

/** claude init:把扁平 tools 里 `mcp__<server>__<tool>` 按 server 归到对应条目。
 *  codex/opencode 不传 tools,这里只产出 server + 状态(tools 为空)。 */
export function groupMcpTools(
  servers: { name: string; status: string }[],
  tools: string[],
): McpServerInfo[] {
  const byPrefix = new Map<string, string[]>();
  for (const t of tools) {
    const m = /^mcp__(.+?)__(.+)$/.exec(t);
    if (!m) continue;
    const key = norm(m[1]);
    const list = byPrefix.get(key) ?? [];
    list.push(m[2]);
    byPrefix.set(key, list);
  }
  return servers.map((s) => ({
    name: s.name,
    status: s.status,
    tools: byPrefix.get(norm(s.name)) ?? [],
  }));
}

/** init push → SessionMeta(init 不带 usage,保留旧 contextTokens)。 */
export function metaFromInit(
  prev: SessionMeta | undefined,
  p: {
    mcp_servers: { name: string; status: string }[];
    tools: string[];
    model: string | null;
    window: number | null;
  },
): SessionMeta {
  return {
    contextTokens: prev?.contextTokens,
    window: p.window ?? prev?.window ?? undefined,
    model: p.model ?? prev?.model ?? undefined,
    mcpServers: groupMcpTools(p.mcp_servers, p.tools),
  };
}

/** usage push → SessionMeta(usage 不带 mcp,保留旧 mcpServers)。 */
export function metaFromUsage(
  prev: SessionMeta | undefined,
  p: { context_tokens: number; window: number | null; model: string | null },
): SessionMeta {
  return {
    mcpServers: prev?.mcpServers ?? [],
    model: p.model ?? prev?.model ?? undefined,
    window: p.window ?? prev?.window ?? undefined,
    contextTokens: p.context_tokens,
  };
}

/** lead_state / session_for 回包 → SessionMeta(快照里没有 tools,只回填 server+状态)。 */
export function metaFromSnapshot(snap: {
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[];
}): SessionMeta {
  return {
    contextTokens: snap.context_tokens ?? undefined,
    window: snap.window ?? undefined,
    model: snap.model ?? undefined,
    mcpServers: groupMcpTools(snap.mcp_servers, []),
  };
}
