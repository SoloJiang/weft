import type { McpServerInfo, SessionMeta } from "../lib/types";

/** server 名与 tool 前缀的非字母数字差异(如 `plugin:context7:context7` ↔
 *  `plugin_context7_context7`)规范化后比较。 */
const norm = (s: string) =>
  s.toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");

/** Weft 自己在 spawn 时注入的内部协调 MCP(weft_bus / weft_planner / …):是 Weft 的
 *  管道,不是用户配置的 MCP。面板只展示用户的 MCP,所以三家统一过滤掉。claude 的
 *  `system/init` 会带上它们(codex/opencode 的探测本就不含),在此统一隐藏以保持一致。 */
const isInternalMcp = (name: string) => /^weft[-_]/i.test(name);

/** claude init:把扁平 tools 里 `mcp__<server>__<tool>` 按 server 归到对应条目。
 *  codex/opencode 不传 tools,这里只产出 server + 状态(tools 为空)。weft_* 内部 server 过滤掉。 */
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
  return servers
    .filter((s) => !isInternalMcp(s.name))
    .map((s) => ({
      name: s.name,
      status: s.status,
      tools: byPrefix.get(norm(s.name)) ?? [],
    }));
}

/** init push → SessionMeta(init 不带 usage,保留旧 contextTokens)。
 *  codex/opencode 的 init push 不带 mcp_servers(只有 claude 的 `system/init` 带)——
 *  这种空 init 不能覆盖 session_meta 已填的 server,故空时保留 prev.mcpServers。 */
export function metaFromInit(
  prev: SessionMeta | undefined,
  p: {
    mcp_servers: { name: string; status: string }[];
    tools: string[];
    model: string | null;
    window: number | null;
  },
): SessionMeta {
  const grouped = groupMcpTools(p.mcp_servers, p.tools);
  return {
    contextTokens: prev?.contextTokens,
    window: p.window ?? prev?.window ?? undefined,
    model: p.model ?? prev?.model ?? undefined,
    mcpServers: grouped.length > 0 ? grouped : (prev?.mcpServers ?? []),
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

/** session_meta(M2 带外)→ 并入现有 meta。codex 的 contextTokens 走 live event,
 *  快照为 null 时保留旧值,别覆盖。servers 用快照(只 server+状态,tools 恒空)。 */
export function mergeSnapshot(
  prev: SessionMeta | undefined,
  s: {
    context_tokens: number | null;
    window: number | null;
    model: string | null;
    mcp_servers: { name: string; status: string }[];
  },
): SessionMeta {
  return {
    contextTokens: s.context_tokens ?? prev?.contextTokens,
    window: s.window ?? prev?.window ?? undefined,
    model: s.model ?? prev?.model ?? undefined,
    mcpServers: groupMcpTools(s.mcp_servers, []),
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
