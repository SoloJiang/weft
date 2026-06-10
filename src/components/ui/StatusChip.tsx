import { Circle, Square, X } from "lucide-react";
import { motion } from "motion/react";
import type { SessionStatus } from "../../lib/types";
import { cn } from "../../lib/cn";

const MAP: Record<
  SessionStatus,
  { label: string; color: string; ring: string }
> = {
  running: { label: "Running", color: "text-running", ring: "ring-running/30" },
  idle: { label: "Idle", color: "text-idle", ring: "ring-idle/25" },
  exited: { label: "Exited", color: "text-danger", ring: "ring-danger/30" },
};

function Glyph({ status }: { status: SessionStatus }) {
  if (status === "running")
    return <Circle size={9} className="weft-pulse fill-current" />;
  if (status === "exited") return <X size={11} />;
  return <Square size={9} className="fill-current" />;
}

export function StatusChip({
  status,
  className,
}: {
  status: SessionStatus;
  className?: string;
}) {
  const s = MAP[status];
  return (
    <motion.span
      layout
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full bg-raised px-2 py-0.5",
        "text-[11px] font-medium ring-1 ring-inset",
        s.color,
        s.ring,
        className,
      )}
    >
      <Glyph status={status} />
      <span>{s.label}</span>
    </motion.span>
  );
}

/** A bare status dot for dense rows (nav tree). */
export function StatusDot({ status }: { status: SessionStatus }) {
  const s = MAP[status];
  return (
    <span className={cn("inline-flex", s.color)} title={s.label}>
      <Glyph status={status} />
    </span>
  );
}
