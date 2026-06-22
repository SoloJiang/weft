import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { PanelRightClose } from "lucide-react";
import { useStore } from "../state/store";
import { LeadTab } from "../session/LeadTab";
import { RepoDetailContent } from "./RepoGraph";
import { ResizeEdge } from "./ResizeEdge";
import { cn } from "../lib/cn";

/**
 * The Repos view's single right drawer. Mutually-exclusive tabs — repo detail or
 * the dependency-curator chat — over a full-width graph. Rendered inside the
 * RepoMapView section (which is `relative`), absolutely positioned on the right;
 * it overlays the right of the canvas without dimming or blocking the rest, so
 * panning/zooming and clicking other nodes still work. Esc / the close button
 * dismiss it.
 */
export function RepoDrawer() {
  const { repoDrawerOpen, repoDrawerTab, setRepoDrawerTab, closeRepoDrawer, repoDrawerWidth, setRepoDrawerWidth } =
    useStore();
  const { t } = useTranslation();

  useEffect(() => {
    if (!repoDrawerOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeRepoDrawer();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [repoDrawerOpen, closeRepoDrawer]);

  if (!repoDrawerOpen) return null;

  const tabs: { key: "detail" | "curator"; label: string }[] = [
    { key: "detail", label: t("repomap.detailTab") },
    { key: "curator", label: t("repomap.curatorTitle") },
  ];

  return (
    <div
      role="dialog"
      aria-label={tabs.find((x) => x.key === repoDrawerTab)?.label}
      className="absolute right-4 top-4 bottom-4 z-30 flex flex-col overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface shadow-[0_8px_28px_-8px_rgba(0,0,0,0.6)]"
      style={{ width: repoDrawerWidth }}
    >
      <ResizeEdge width={repoDrawerWidth} onResize={setRepoDrawerWidth} />
      <header className="flex items-center gap-2 border-b border-border px-2 py-2">
        <div className="flex flex-1 items-center gap-0.5 rounded-[var(--radius-md)] bg-bg p-0.5">
          {tabs.map((tab) => (
            <button
              key={tab.key}
              onClick={() => setRepoDrawerTab(tab.key)}
              className={cn(
                "flex-1 rounded px-2 py-1 text-[12px] transition-colors",
                repoDrawerTab === tab.key
                  ? "bg-surface text-ink shadow-[0_1px_3px_-1px_rgba(0,0,0,0.3)]"
                  : "text-ink-faint hover:text-ink",
              )}
            >
              {tab.label}
            </button>
          ))}
        </div>
        <button
          onClick={closeRepoDrawer}
          aria-label={t("common.close")}
          title={t("common.close")}
          className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <PanelRightClose size={14} />
        </button>
      </header>
      <div className="min-h-0 flex-1">
        <DrawerBody />
      </div>
    </div>
  );
}

/** 3-way body kept as a helper (no nested ternaries, per CLAUDE.md). The curator
 *  thread is created lazily here so opening the curator tab is what triggers it. */
function DrawerBody() {
  const { repoDrawerTab, selectedRepoId, curatorThreadId, ensureCuratorThread } = useStore();
  const { t } = useTranslation();
  useEffect(() => {
    if (repoDrawerTab === "curator" && curatorThreadId == null) void ensureCuratorThread();
  }, [repoDrawerTab, curatorThreadId, ensureCuratorThread]);

  if (repoDrawerTab === "detail") return <RepoDetailContent repoId={selectedRepoId} />;
  if (curatorThreadId != null) return <LeadTab threadId={curatorThreadId} compact onReview={() => {}} />;
  return (
    <div className="flex h-full items-center justify-center text-[12px] text-ink-faint">
      {t("repomap.curatorLoading")}
    </div>
  );
}
