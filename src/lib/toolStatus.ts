import type { ToolStatus } from "./types";

export function spawnableToolsOf(tools: readonly ToolStatus[]): ToolStatus[] {
  return tools.filter((tool) => tool.spawnable);
}
