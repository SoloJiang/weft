import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Network } from "lucide-react";
import { useStore } from "../state/store";
import { RepoGraph } from "./RepoGraph";

/**
 * The Repo map (ARCHITECTURE §4.9): what each repo is, and how they depend on
 * one another — the curator's map that powers cross-repo scope decomposition.
 * Rendered as a dependency graph (mind-map); each node carries the repo's role,
 * stack, and one-line summary, with dependencies as edges.
 */
export function RepoMapView({ embedded = false }: { embedded?: boolean }) {
  const { repoProfiles, refreshRepoMap } = useStore();
  const { t } = useTranslation();

  useEffect(() => {
    void refreshRepoMap();
  }, [refreshRepoMap]);

  const body = (
    <div className="min-h-0 flex-1">
      {repoProfiles.length === 0 ? <EmptyMap /> : <RepoGraph />}
    </div>
  );

  if (embedded) return body;

  return (
    <section className="flex min-w-0 flex-1 flex-col bg-bg">
      <header className="flex items-center gap-2.5 border-b border-border px-5 py-2.5">
        <span className="grid h-6 w-6 place-items-center rounded-[var(--radius-sm)] bg-brand-ghost">
          <Network size={14} className="text-brand" />
        </span>
        <h1 className="text-[16px] font-semibold tracking-tight text-ink">{t("nav.repoMap")}</h1>
        <span className="ml-auto text-[12px] text-ink-faint">{t("repomap.subtitle")}</span>
      </header>
      {body}
    </section>
  );
}

function EmptyMap() {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Network size={22} className="text-ink-faint" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">{t("repomap.emptyTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">
        {t("repomap.emptyBody")}
      </p>
    </div>
  );
}
