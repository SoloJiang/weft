import type { McpServerInfo, SessionMeta } from "../lib/types";

/** server 名与 tool 前缀的非字母数字差异(如 `plugin:context7:context7` ↔
 *  `plugin_context7_context7`)规范化后比较。 */
const norm = (s: string) =>
  s.toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");

/** Weft 自己在 spawn 时注入的内部协调 MCP(见 `bus/inject.rs`)。精确名单,不是前缀
 *  匹配。面板展示时 status 标为 `weft`,与用户配置的 server 区分。 */
const WEFT_INTERNAL_MCP = new Set(["weft_bus", "weft_planner", "weft_global", "weft_curator"]);
const isInternalMcp = (name: string) => WEFT_INTERNAL_MCP.has(name.toLowerCase());

/** claude init:把扁平 tools 里 `mcp__<server>__<tool>` 按 server 归到对应条目。
 *  codex/opencode 不传 tools,这里只产出 server + 状态(tools 为空)。
 *  Weft 内部 server status 标为 `weft`(见 WEFT_INTERNAL_MCP)。 */
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
  // Show every server on the session, including Weft pipes (weft_bus /
  // weft_planner / weft_global). Hiding them made ACP/omp look MCP-less even
  // though those pipes are what the agent actually has. Tag Weft pipes so the
  // row still reads as infrastructure.
  return servers.map((s) => ({
    name: s.name,
    status: isInternalMcp(s.name) && (s.status === "connected" || !s.status)
      ? "weft"
      : s.status,
    tools: byPrefix.get(norm(s.name)) ?? [],
  }));
}

/** init push → SessionMeta(init 不带 usage,保留旧 contextTokens)。
 *  只有 claude 的 `system/init` 带权威 mcp_servers/tools/model;codex/opencode 的
 *  "裸" init(native-id 抽取 / 命令刷新回包)带 `model:null` + 空 server,只是占位,
 *  绝不能覆盖 session_meta 带外填好的 server。用 `model != null` 区分两者:
 *  claude 权威 → 即使空也替换(会话真的没 MCP 时清掉陈旧行);裸 init → 空时保留旧的。 */
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
  // MCP authority only from MCP payload (raw servers present).
  const authoritative = p.mcp_servers.length > 0;
  const keepPrevServers = !authoritative && grouped.length === 0;
  return {
    contextTokens: prev?.contextTokens,
    window: p.window ?? prev?.window ?? undefined,
    model: p.model ?? prev?.model ?? undefined,
    mcpServers: keepPrevServers ? (prev?.mcpServers ?? []) : grouped,
    mcpAuthoritative: authoritative ? true : prev?.mcpAuthoritative,
    engineSkills: prev?.engineSkills, // 走带外 session_meta,保留已并入的
    reasoningEffort: prev?.reasoningEffort,
  };
}

/** usage push → SessionMeta(usage 不带 mcp,保留旧 mcpServers)。 */
export function metaFromUsage(
  prev: SessionMeta | undefined,
  p: { context_tokens: number; window: number | null; model: string | null },
): SessionMeta {
  return {
    mcpServers: prev?.mcpServers ?? [],
    mcpAuthoritative: prev?.mcpAuthoritative,
    model: p.model ?? prev?.model ?? undefined,
    window: p.window ?? prev?.window ?? undefined,
    contextTokens: p.context_tokens,
    engineSkills: prev?.engineSkills,
    reasoningEffort: prev?.reasoningEffort,
  };
}

/** session_meta(M2 带外)→ 并入现有 meta。不变量:部分/空的来源**绝不覆盖**已有的更
 *  富 meta —— contextTokens(codex 走 live event)、window、model 空时保留旧值。
 *  mcp_servers 是 Option:`null` = 探测失败(`codex mcp list`/`/mcp` 瞬时错),保留旧行;
 *  非 null = 权威结果(即使空数组也替换——会话此刻确实没 MCP server,该清掉陈旧行)。 */
export function mergeSnapshot(
  prev: SessionMeta | undefined,
  s: {
    context_tokens: number | null;
    window: number | null;
    model: string | null;
    mcp_servers: { name: string; status: string }[] | null;
    skills?: { name: string; description: string }[] | null;
    reasoning_effort?: string | null;
  },
): SessionMeta {
  return {
    contextTokens: s.context_tokens ?? prev?.contextTokens,
    window: s.window ?? prev?.window ?? undefined,
    model: s.model ?? prev?.model ?? undefined,
    mcpServers: s.mcp_servers == null ? (prev?.mcpServers ?? []) : groupMcpTools(s.mcp_servers, []),
    // null = 没探到(保留旧),非 null = 权威列表(空也算——此刻确实没有 MCP)
    mcpAuthoritative: s.mcp_servers == null ? prev?.mcpAuthoritative : true,
    engineSkills: s.skills == null ? prev?.engineSkills : s.skills,
    reasoningEffort: s.reasoning_effort ?? prev?.reasoningEffort,
  };
}

/** lead_state / session_for 回包 → SessionMeta(引擎缓存的回填,重挂常驻面板不空白)。
 *  claude 的回包带 tools(引擎 `last_tools`),据此把 `mcp__server__tool` 归到 server 下;
 *  codex/opencode 不给 tools(空数组),只回填 server + 状态。 */
export function metaFromSnapshot(snap: {
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[];
  tools: string[];
  mcp_known?: boolean;
}): SessionMeta {
  const mcpServers = groupMcpTools(snap.mcp_servers, snap.tools);
  // Authority only from MCP reading (mcp_known or non-empty raw list).
  const mcpAuthoritative =
    snap.mcp_known === true || snap.mcp_servers.length > 0 ? true : undefined;
  return {
    contextTokens: snap.context_tokens ?? undefined,
    window: snap.window ?? undefined,
    model: snap.model ?? undefined,
    mcpServers,
    mcpAuthoritative,
  };
}

/** 引擎/DB 快照回填到已有 meta 的**补洞**合并:带外 session_meta / live push 可能已
 *  先写入(更新的)值,快照只填 undefined 的洞、mcpServers 只在旧值为空**且不权威**时
 *  使用 —— 权威的空列表(探测明确说"没有 MCP")绝不被陈旧的持久化快照复活。解决
 *  "带外 meta 先落地导致 first-paint-only 回填被跳过、持久化快照丢失"的竞态,同时保持
 *  原不变量:快照绝不覆盖更富/更权威的 live meta。 */
export function fillMetaHoles(prev: SessionMeta | undefined, snap: SessionMeta): SessionMeta {
  if (!prev) return snap;
  const fillServers = prev.mcpServers.length === 0 && !prev.mcpAuthoritative;
  return {
    contextTokens: prev.contextTokens ?? snap.contextTokens,
    window: prev.window ?? snap.window,
    model: prev.model ?? snap.model,
    mcpServers: fillServers ? snap.mcpServers : prev.mcpServers,
    // Promote authority when the snapshot is a real engine reading (even if
    // user-visible servers are empty after filtering Weft-internal pipes).
    mcpAuthoritative: prev.mcpAuthoritative || snap.mcpAuthoritative,
    engineSkills: prev.engineSkills ?? snap.engineSkills,
    reasoningEffort: prev.reasoningEffort ?? snap.reasoningEffort,
  };
}
