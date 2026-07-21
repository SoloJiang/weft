import { Circle, Square, X } from "lucide-react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import type { SessionStatus } from "../../lib/types";
import { cn } from "../../lib/cn";

const STYLE_MAP: Record<SessionStatus, { color: string; ring: string }> = {
  running: { color: "text-running", ring: "ring-running/30" },
  stalled: { color: "text-waiting", ring: "ring-waiting/30" },
  idle: { color: "text-idle", ring: "ring-idle/25" },
  exited: { color: "text-danger", ring: "ring-danger/30" },
};

const LABEL_KEYS: Record<SessionStatus, string> = {
  running: "status.running",
  stalled: "status.stalled",
  idle: "status.idle",
  exited: "status.exited",
};

function Glyph({ status }: { status: SessionStatus }) {
  if (status === "running")
    return <Circle size={9} className="weft-pulse fill-current" />;
  // Stalled: same disc as running but STILL (no pulse). Motion = healthy stream,
  // stillness = gone quiet. Amber comes from the parent's text-waiting.
  if (status === "stalled") return <Circle size={9} className="fill-current" />;
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
  const { t } = useTranslation();
  const s = STYLE_MAP[status];
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
      <span>{t(LABEL_KEYS[status])}</span>
    </motion.span>
  );
}

/** A bare status dot for dense rows (nav tree). */
export function StatusDot({ status }: { status: SessionStatus }) {
  const { t } = useTranslation();
  const s = STYLE_MAP[status];
  return (
    <span className={cn("inline-flex", s.color)} title={t(LABEL_KEYS[status])}>
      <Glyph status={status} />
    </span>
  );
}
