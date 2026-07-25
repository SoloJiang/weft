import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { listen } from "@tauri-apps/api/event";
import { useReducedMotion } from "motion/react";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import type {
  ProcessQuotaLevel,
  ProcessQuotaStatus,
  ResourceDashboardSnapshot,
  ResourceOwnerCount,
} from "../lib/types";

// Backend samples process quota every 3s (process_quota.rs's own monitor
// cadence) — polling faster would just re-read the same value.
const POLL_MS = 3000;

// One missed tick is normal IPC noise and stays silent (see the `.catch` below).
// Past this many *consecutive* misses (~9s at POLL_MS=3000) the snapshot is old
// enough that staying silent would be dishonest for a panel whose whole point is
// "right now" — surface a restrained staleness hint instead.
const STALE_AFTER_CONSECUTIVE_FAILURES = 3;

/** Settings → Resources: read-only local-runtime dashboard (issue #112). Polls
 *  the combined snapshot while mounted and layers the existing
 *  `process-quota://changed` push event on top so a warn/degrade transition
 *  reflects instantly instead of waiting for the next tick. No manual actions
 *  here (no reap / no degrade-now) — display only, matching the read-only scope
 *  the safety-net write paths (`proc_registry`, `process_quota`, `session_gate`)
 *  keep for themselves. */
export function ResourcesSettings() {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<ResourceDashboardSnapshot | null>(null);
  const [stale, setStale] = useState(false);

  useEffect(() => {
    let alive = true;
    let seq = 0;
    let consecutiveFailures = 0;
    const poll = () => {
      const mySeq = ++seq;
      void api
        .resourceDashboardSnapshot()
        .then((next) => {
          // A newer poll (or the unmount cleanup) may have already landed;
          // never let a slow, out-of-order response regress the display.
          if (!alive || mySeq !== seq) return;
          consecutiveFailures = 0;
          setSnapshot(next);
          setStale(false);
        })
        .catch(() => {
          // Transient IPC hiccup: keep showing the last good snapshot rather
          // than flashing an error state for one missed tick. Only past
          // STALE_AFTER_CONSECUTIVE_FAILURES misses in a row does the panel
          // admit the data might be old — self-clears the instant a poll
          // succeeds again, so it never lingers past the problem.
          if (!alive || mySeq !== seq) return;
          consecutiveFailures += 1;
          if (consecutiveFailures >= STALE_AFTER_CONSECUTIVE_FAILURES) {
            setStale(true);
          }
        });
    };
    poll();
    const id = setInterval(poll, POLL_MS);
    const unlistenPromise = listen<ProcessQuotaStatus>("process-quota://changed", (event) => {
      if (!alive) return;
      setSnapshot((prev) => (prev ? { ...prev, quota: event.payload } : prev));
    });
    return () => {
      alive = false;
      clearInterval(id);
      void unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

  return (
    <div className="flex flex-col gap-10">
      <div className="flex flex-col gap-1">
        <p className="text-[12px] leading-relaxed text-ink-faint">{t("settings.resourcesHint")}</p>
        {stale && (
          <p className="text-[11.5px] leading-relaxed text-ink-faint">
            {t("settings.resourcesStale")}
          </p>
        )}
      </div>

      <SettingsGroup title={t("settings.resourcesQuotaGroup")}>
        <div className="px-3 py-3.5">
          <QuotaGauge quota={snapshot?.quota ?? null} t={t} />
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.resourcesTreeGroup")}>
        <div className="px-3 py-3.5">
          <ProcessTree
            processCount={snapshot?.instanceProcessCount ?? null}
            byOwner={snapshot?.byOwner ?? null}
            t={t}
          />
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.resourcesMemoryGroup")}>
        <div className="px-3 py-3.5">
          <MemoryStat
            bytes={snapshot ? snapshot.instanceMemoryBytes : undefined}
            t={t}
          />
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.resourcesSessionsGroup")}>
        <div className="px-3 py-3.5">
          <SessionSlots
            active={snapshot?.activeSessions ?? null}
            max={snapshot?.maxSessions ?? null}
            t={t}
          />
        </div>
      </SettingsGroup>
    </div>
  );
}

// ── 进程配额:唯一带危险分级配色的仪表(阈值来自后端 governor,不自造) ──────────

const QUOTA_TONE: Record<
  ProcessQuotaLevel,
  { bar: string; text: string; pillBg: string; labelKey: string }
> = {
  normal: {
    bar: "bg-success",
    text: "text-success",
    pillBg: "border-success/30 bg-success/15",
    labelKey: "settings.resourcesQuotaNormal",
  },
  warning: {
    bar: "bg-waiting",
    text: "text-waiting",
    pillBg: "border-waiting/30 bg-waiting/15",
    labelKey: "settings.resourcesQuotaWarning",
  },
  degraded: {
    bar: "bg-danger",
    text: "text-danger",
    pillBg: "border-danger/30 bg-danger/15",
    labelKey: "settings.resourcesQuotaDegraded",
  },
};

function QuotaGauge({ quota, t }: { quota: ProcessQuotaStatus | null; t: TFunction }) {
  const reduce = useReducedMotion();
  if (!quota) return <GaugeSkeleton />;
  const tone = QUOTA_TONE[quota.status];
  const hasLimit = quota.processLimit !== null && quota.usagePercent !== null;
  const pct = hasLimit ? Math.max(0, Math.min(100, quota.usagePercent as number)) : null;
  const detail = hasLimit
    ? t("processQuota.usage", {
        count: quota.processCount,
        limit: quota.processLimit,
        percent: Math.round(quota.usagePercent as number),
      })
    : t("processQuota.countOnly", { count: quota.processCount });

  return (
    <div className="flex flex-col gap-2.5">
      <div className="flex items-center justify-between gap-3">
        <span
          className={cn(
            "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11px] font-medium",
            tone.pillBg,
            tone.text,
          )}
        >
          {t(tone.labelKey)}
        </span>
        <span className="font-mono text-[12px] tabular-nums text-ink-muted">{detail}</span>
      </div>
      {pct !== null ? (
        <div className="relative h-2.5 w-full">
          <div className="h-full w-full overflow-hidden rounded-full bg-border">
            <div
              className={cn(
                "h-full rounded-full",
                tone.bar,
                !reduce && "transition-[width] duration-700 ease-out",
              )}
              style={{ width: `${pct}%` }}
            />
          </div>
          {/* Threshold ticks straight from the governor's own warn/degrade
           *  percents — never invented, so the danger grading always matches
           *  what actually triggers a state change. */}
          <ThresholdTick percent={quota.warningPercent} />
          <ThresholdTick percent={quota.degradedPercent} />
        </div>
      ) : (
        <p className="text-[11.5px] text-ink-faint">{t("settings.resourcesQuotaNoLimit")}</p>
      )}
    </div>
  );
}

function ThresholdTick({ percent }: { percent: number }) {
  return (
    <div
      aria-hidden
      className="absolute inset-y-0 w-px bg-ink/25"
      style={{ left: `${Math.max(0, Math.min(100, percent))}%` }}
    />
  );
}

// ── 进程树:Weft owned 子树的构成(纯信息展示,无危险配色) ─────────────────────

const OWNER_LABEL_KEY: Record<string, string> = {
  global_app_server: "settings.resourcesOwnerGlobalAppServer",
  session: "settings.resourcesOwnerSession",
  lead_thread: "settings.resourcesOwnerLeadThread",
  curator: "settings.resourcesOwnerCurator",
  opencode: "settings.resourcesOwnerOpencode",
  preview: "settings.resourcesOwnerPreview",
  probe: "settings.resourcesOwnerProbe",
  other: "settings.resourcesOwnerOther",
};

function ownerLabel(kind: string, t: TFunction): string {
  const key = OWNER_LABEL_KEY[kind];
  return key ? t(key) : kind;
}

function ProcessTree({
  processCount,
  byOwner,
  t,
}: {
  processCount: number | null;
  byOwner: ResourceOwnerCount[] | null;
  t: TFunction;
}) {
  if (processCount === null || byOwner === null) return <GaugeSkeleton />;
  return (
    <div className="flex flex-col gap-2.5">
      <span className="font-mono text-[13px] tabular-nums text-ink">
        {t("settings.resourcesTreeTotal", { count: processCount })}
      </span>
      {byOwner.length > 0 ? (
        <div className="flex flex-wrap gap-1.5">
          {byOwner.map((entry) => (
            <span
              key={entry.kind}
              className="inline-flex items-center gap-1.5 rounded-full border border-border bg-bg px-2 py-0.5 text-[11px] text-ink-muted"
            >
              {ownerLabel(entry.kind, t)}
              <span className="font-mono tabular-nums text-ink-faint">{entry.count}</span>
            </span>
          ))}
        </div>
      ) : (
        <p className="text-[11.5px] text-ink-faint">{t("settings.resourcesTreeEmpty")}</p>
      )}
    </div>
  );
}

// ── 内存:Weft owned 子树的常驻内存合计(无危险配色——watchdog 未对内存分级) ────

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function MemoryStat({ bytes, t }: { bytes: number | null | undefined; t: TFunction }) {
  if (bytes === undefined) return <GaugeSkeleton />;
  return (
    <div className="flex flex-col gap-1">
      <span className="font-mono text-[15px] tabular-nums text-ink">
        {bytes === null ? t("settings.resourcesMemoryUnavailable") : fmtBytes(bytes)}
      </span>
      <p className="text-[11.5px] leading-relaxed text-ink-faint">
        {t("settings.resourcesMemoryHint")}
      </p>
    </div>
  );
}

// ── 并发会话额度:session_gate 的槽位占用(满员是真实状态,不是发明的阈值) ──────

function SessionSlots({
  active,
  max,
  t,
}: {
  active: number | null;
  max: number | null;
  t: TFunction;
}) {
  const reduce = useReducedMotion();
  if (active === null || max === null) return <GaugeSkeleton />;
  const atCapacity = max > 0 && active >= max;
  const pct = max > 0 ? Math.max(0, Math.min(100, (active / max) * 100)) : 0;
  return (
    <div className="flex flex-col gap-2.5">
      <div className="flex items-center justify-between gap-3">
        <span className="font-mono text-[13px] tabular-nums text-ink">
          {t("settings.resourcesSessionsLabel", { active, max })}
        </span>
        {atCapacity && (
          <span className="inline-flex items-center gap-1.5 rounded-full border border-waiting/30 bg-waiting/15 px-2 py-0.5 text-[11px] font-medium text-waiting">
            {t("settings.resourcesSessionsFull")}
          </span>
        )}
      </div>
      <div className="h-2 w-full overflow-hidden rounded-full bg-border">
        <div
          className={cn(
            "h-full rounded-full bg-brand",
            !reduce && "transition-[width] duration-700 ease-out",
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
      <p className="text-[11.5px] leading-relaxed text-ink-faint">
        {t("settings.resourcesSessionsHint")}
      </p>
    </div>
  );
}

// ── shared ──────────────────────────────────────────────────────────────────

function GaugeSkeleton() {
  return (
    <div aria-hidden className="flex flex-col gap-2.5">
      <div className="h-4 w-40 animate-pulse rounded bg-border-strong/40" />
      <div className="h-2.5 w-full animate-pulse rounded-full bg-border-strong/25" />
    </div>
  );
}

// Local copies of SettingsDialog's layout primitive (not exported from there;
// mirrors src/settings/Backup.tsx's same choice to avoid a cross-cutting
// refactor for one more panel).
function SettingsGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <h2 className="text-[13px] font-semibold text-ink">{title}</h2>
      <div className="flex flex-col rounded-[var(--radius-lg)] border border-border bg-surface">
        {children}
      </div>
    </section>
  );
}
