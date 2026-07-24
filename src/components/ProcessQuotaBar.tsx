import { AlertTriangle, CircleAlert, type LucideIcon } from "lucide-react";
import type { TFunction } from "i18next";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useTranslation } from "react-i18next";
import type { ProcessQuotaLevel, ProcessQuotaStatus } from "../lib/types";
import { needsBarMotion } from "../lib/motion";
import { useStore } from "../state/store";

type QuotaBarView = {
  icon: LucideIcon;
  role: "status" | "alert";
  titleKey: string;
  bodyKey: string;
  containerClassName: string;
  iconClassName: string;
  titleClassName: string;
};

const QUOTA_BAR_VIEW: Record<ProcessQuotaLevel, QuotaBarView | null> = {
  normal: null,
  warning: {
    icon: AlertTriangle,
    role: "status",
    titleKey: "processQuota.warningTitle",
    bodyKey: "processQuota.warningBody",
    containerClassName: "border-waiting/30 bg-waiting/10",
    iconClassName: "text-waiting",
    titleClassName: "text-waiting",
  },
  degraded: {
    icon: CircleAlert,
    role: "alert",
    titleKey: "processQuota.degradedTitle",
    bodyKey: "processQuota.degradedBody",
    containerClassName: "border-danger/35 bg-danger/10",
    iconClassName: "text-danger",
    titleClassName: "text-danger",
  },
};

function quotaUsageText(status: ProcessQuotaStatus, t: TFunction): string {
  if (status.processLimit === null || status.usagePercent === null) {
    return t("processQuota.countOnly", { count: status.processCount });
  }
  return t("processQuota.usage", {
    count: status.processCount,
    limit: status.processLimit,
    percent: Math.round(status.usagePercent),
  });
}

export function ProcessQuotaBar({ inSettings = false }: { inSettings?: boolean }) {
  const { processQuota, openSettings } = useStore();
  const { t } = useTranslation();
  const reduce = useReducedMotion();
  const view = processQuota === null ? null : QUOTA_BAR_VIEW[processQuota.status];

  return (
    <AnimatePresence initial={false}>
      {processQuota !== null && view !== null && (
        <motion.div
          key="process-quota-bar"
          {...needsBarMotion(Boolean(reduce))}
          role={view.role}
          className={`shrink-0 overflow-hidden border-b ${view.containerClassName}`}
        >
          <div className="flex min-h-10 items-center gap-2 px-5 py-2 text-[12px]">
            <view.icon size={14} className={`shrink-0 ${view.iconClassName}`} />
            <span className={`shrink-0 font-semibold ${view.titleClassName}`}>
              {t(view.titleKey)}
            </span>
            <span className="min-w-0 flex-1 truncate text-ink-muted">
              {t(view.bodyKey, {
                degraded: processQuota.degradedPercent,
                recovery: processQuota.recoveryPercent,
              })}
            </span>
            <span className="hidden shrink-0 font-mono text-[11px] tabular-nums text-ink-faint sm:inline">
              {quotaUsageText(processQuota, t)}
            </span>
            {!inSettings && (
              <button
                type="button"
                onClick={openSettings}
                className="shrink-0 rounded-[var(--radius-sm)] px-2 py-0.5 font-medium text-ink transition-colors hover:bg-bg/60"
              >
                {t("processQuota.openDetails")}
              </button>
            )}
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
