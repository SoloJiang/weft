import { useTranslation } from "react-i18next";
import type { EvidenceFreshness, EvidenceKind, EvidenceRow } from "../lib/types";

/** One discriminated read state (CLAUDE.md: derive ONE discriminated value,
 *  map it exhaustively) — no re-derived loading/error booleans at the call
 *  site. The actual fetch lives in `EvidencePanel`; this file stays a pure
 *  function of props, same split as `ReadinessChip` vs. its callers' fetch
 *  orchestration. */
export type EvidencePanelState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "error" }
  | { kind: "ready"; rows: EvidenceRow[] };

/** Collapse a raw fetch lifecycle + its last resolved rows into ONE
 *  discriminated `EvidencePanelState`. */
export function evidencePanelState(
  fetchStatus: "idle" | "loading" | "resolved" | "rejected",
  rows: EvidenceRow[] | null,
): EvidencePanelState {
  if (fetchStatus === "idle") return { kind: "idle" };
  if (fetchStatus === "loading") return { kind: "loading" };
  if (fetchStatus === "rejected") return { kind: "error" };
  return { kind: "ready", rows: rows ?? [] };
}

const FRESHNESS_STYLE: Record<EvidenceFreshness, { dot: string; labelKey: string }> = {
  fresh: { dot: "bg-running", labelKey: "evidence.freshness.fresh" },
  stale: { dot: "bg-waiting", labelKey: "evidence.freshness.stale" },
  unknown: { dot: "bg-ink-faint", labelKey: "evidence.freshness.unknown" },
};

const KIND_LABEL_KEYS: Record<EvidenceKind, string> = {
  code: "evidence.kind.code",
  verification: "evidence.kind.verification",
  interface: "evidence.kind.interface",
  host: "evidence.kind.host",
  execution: "evidence.kind.execution",
  decision: "evidence.kind.decision",
  handoff: "evidence.kind.handoff",
};

function FreshnessTag({ freshness }: { freshness: EvidenceFreshness }) {
  const { t } = useTranslation();
  const style = FRESHNESS_STYLE[freshness];
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1 text-[10px] font-medium text-ink-faint"
      title={t(style.labelKey)}
    >
      <span className={["h-1.5 w-1.5 shrink-0 rounded-full", style.dot].join(" ")} />
      {t(style.labelKey)}
    </span>
  );
}

function EvidenceRowItem({ row }: { row: EvidenceRow }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-0.5 border-b border-border/60 py-1.5 last:border-b-0">
      <div className="flex items-center justify-between gap-2">
        <span className="truncate text-[11px] font-medium text-ink">
          {t(KIND_LABEL_KEYS[row.kind])}
        </span>
        <FreshnessTag freshness={row.freshness} />
      </div>
      <div
        className="truncate text-[10.5px] text-ink-faint"
        title={row.source_ref ? `${row.source} · ${row.source_ref}` : row.source}
      >
        {row.source}
        {row.source_ref ? ` · ${row.source_ref}` : ""}
      </div>
      {row.summary && (
        <div className="truncate text-[10.5px] text-ink-muted" title={row.summary}>
          {row.summary}
        </div>
      )}
      {row.superseded_by !== 0 && (
        <span className="w-fit rounded-full bg-raised px-1.5 py-0.5 text-[9.5px] text-ink-faint">
          {t("evidence.supersededBadge")}
        </span>
      )}
    </div>
  );
}

/** Renders one `EvidencePanelState` exhaustively: idle/loading share a
 *  spinner-free "loading" copy, error and empty/ready are each their own
 *  branch. */
export function EvidenceBody({ state }: { state: EvidencePanelState }) {
  const { t } = useTranslation();
  switch (state.kind) {
    case "idle":
    case "loading":
      return <div className="text-[10.5px] text-ink-faint">{t("evidence.loading")}</div>;
    case "error":
      return <div className="text-[10.5px] text-danger">{t("evidence.loadFailed")}</div>;
    case "ready":
      if (state.rows.length === 0) {
        return <div className="text-[10.5px] text-ink-faint">{t("evidence.empty")}</div>;
      }
      return (
        <div className="flex flex-col">
          {state.rows.map((row) => (
            <EvidenceRowItem key={row.id} row={row} />
          ))}
        </div>
      );
  }
}
