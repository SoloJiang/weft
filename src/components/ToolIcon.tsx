import { cn } from "../lib/cn";

const SRC: Record<string, string> = {
  claude: "/tools/claude.svg",
  codex: "/tools/codex.svg",
  opencode: "/tools/opencode.svg",
  omp: "/tools/omp.svg",
};

const FULL_NAME: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  omp: "Oh My Pi",
};

export function toolFullName(tool: string) {
  return FULL_NAME[tool] ?? tool;
}

/** The official mark for a coding tool (claude / codex / opencode / omp). */
export function ToolIcon({
  tool,
  size = 14,
  className,
}: {
  tool: string;
  size?: number;
  className?: string;
}) {
  const src = SRC[tool];
  if (!src) return null;
  return (
    <img
      src={src}
      alt={toolFullName(tool)}
      width={size}
      height={size}
      draggable={false}
      className={cn("shrink-0 rounded-[3px]", className)}
    />
  );
}
