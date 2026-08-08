import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { EvidenceRow } from "../lib/types";
import { api } from "../lib/api";
import { EvidenceBody, evidencePanelState } from "./EvidenceBody";

/**
 * Compact Evidence ledger listing for one Lane (issue #174 R1-04): kind,
 * source, a bounded summary, and a fresh|stale|unknown chip. Fetches lazily
 * — only while `active` (the owning menu/panel is open) — so a closed
 * dropdown never polls the backend. Presentation lives in `EvidenceBody`
 * (pure function of state), same split `ReadinessChip` uses.
 */
export function EvidencePanel({
  threadId,
  directionId,
  active,
  limit = 20,
}: {
  threadId: number;
  directionId: number;
  active: boolean;
  limit?: number;
}) {
  const { t } = useTranslation();
  const [fetchStatus, setFetchStatus] = useState<"idle" | "loading" | "resolved" | "rejected">(
    "idle",
  );
  const [rows, setRows] = useState<EvidenceRow[] | null>(null);

  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    setFetchStatus("loading");
    api
      .listEvidence(threadId, directionId, limit)
      .then((result) => {
        if (cancelled) return;
        setRows(result);
        setFetchStatus("resolved");
      })
      .catch(() => {
        if (cancelled) return;
        setFetchStatus("rejected");
      });
    return () => {
      cancelled = true;
    };
  }, [active, threadId, directionId, limit]);

  if (!active) return null;

  return (
    <div className="flex flex-col gap-1 px-2 py-1.5">
      <div className="text-[10px] font-semibold uppercase tracking-wide text-ink-faint">
        {t("evidence.title")}
      </div>
      <EvidenceBody state={evidencePanelState(fetchStatus, rows)} />
    </div>
  );
}
