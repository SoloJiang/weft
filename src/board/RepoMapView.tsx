import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Network } from "lucide-react";
import { useStore } from "../state/store";
import { RepoGraph } from "./RepoGraph";
import { RepoDrawer } from "./RepoDrawer";
import { useRepoActions } from "../session/useRepoActions";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Input } from "../components/ui/Input";
import { Button } from "../components/ui/Button";

/**
 * The Repo map (ARCHITECTURE §4.9): what each repo is, and how they depend on
 * one another — the curator's map that powers cross-repo scope decomposition.
 * Rendered as a dependency graph (mind-map); each node carries the repo's role,
 * stack, and one-line summary, with dependencies as edges.
 */
export function RepoMapView({ embedded = false }: { embedded?: boolean }) {
  const { repoProfiles, refreshRepoMap, selectedRepoId, repoDrawerTab, clearSelectedRepo, closeRepoDrawer } =
    useStore();

  useEffect(() => {
    void refreshRepoMap();
  }, [refreshRepoMap]);

  // Repair drawer state when the selected repo is deleted or vanishes after a map
  // refresh (the old local seeding effect did this): drop the dangling selection,
  // and close the drawer if it's showing that now-empty detail. Lives here (always
  // mounted, unlike RepoGraph) so it also fires when the last repo is removed.
  useEffect(() => {
    if (selectedRepoId != null && !repoProfiles.some((p) => p.repo_id === selectedRepoId)) {
      clearSelectedRepo();
      if (repoDrawerTab === "detail") closeRepoDrawer();
    }
  }, [repoProfiles, selectedRepoId, repoDrawerTab, clearSelectedRepo, closeRepoDrawer]);

  const body =
    repoProfiles.length === 0 ? (
      <div className="min-h-0 flex-1">
        <EmptyMap />
      </div>
    ) : (
      <div className="relative min-h-0 flex-1">
        <RepoGraph />
        <RepoDrawer />
      </div>
    );

  if (embedded) return body;

  return <section className="flex min-w-0 flex-1 flex-col bg-bg">{body}</section>;
}

type PromptState = {
  title: string;
  placeholder?: string;
  value: string;
  resolve: (v: string | null) => void;
};

function EmptyMap() {
  const { t } = useTranslation();
  const { run, busy } = useRepoActions();
  const [promptState, setPromptState] = useState<PromptState | null>(null);

  const promptText = (title: string, placeholder?: string) =>
    new Promise<string | null>((resolve) =>
      setPromptState({ title, placeholder, value: "", resolve }),
    );

  const click = (kind: "add" | "new" | "clone") =>
    void run({
      actionId: `emptymap-${kind}`,
      kind,
      ctx: {},
      promptText,
    });

  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Network size={22} className="text-ink-faint" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">{t("repomap.emptyTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">
        {t("repomap.emptyBody")}
      </p>
      <div className="mt-4 flex flex-wrap items-center justify-center gap-2">
        <Button
          variant="default"
          size="sm"
          disabled={!!busy["emptymap-add"]}
          onClick={() => click("add")}
        >
          {t("emptyMap.addRepoBtn")}
        </Button>
        <Button
          variant="default"
          size="sm"
          disabled={!!busy["emptymap-new"]}
          onClick={() => click("new")}
        >
          {t("emptyMap.newRepoBtn")}
        </Button>
        <Button
          variant="default"
          size="sm"
          disabled={!!busy["emptymap-clone"]}
          onClick={() => click("clone")}
        >
          {t("emptyMap.cloneRepoBtn")}
        </Button>
      </div>

      <Dialog
        open={promptState != null}
        onOpenChange={(open) => {
          if (!open && promptState) {
            promptState.resolve(null);
            setPromptState(null);
          }
        }}
      >
        {promptState && (
          <DialogContent title={promptState.title}>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const v = promptState.value.trim();
                promptState.resolve(v || null);
                setPromptState(null);
              }}
              className="flex flex-col gap-3"
            >
              <Input
                autoFocus
                placeholder={promptState.placeholder}
                value={promptState.value}
                onChange={(e) =>
                  setPromptState((s) => (s ? { ...s, value: e.currentTarget.value } : s))
                }
              />
              <div className="flex justify-end gap-2">
                <Button
                  type="button"
                  variant="ghost"
                  onClick={() => {
                    promptState.resolve(null);
                    setPromptState(null);
                  }}
                >
                  {t("session.promptCancel")}
                </Button>
                <Button type="submit" variant="primary">
                  {t("session.promptOk")}
                </Button>
              </div>
            </form>
          </DialogContent>
        )}
      </Dialog>
    </div>
  );
}
