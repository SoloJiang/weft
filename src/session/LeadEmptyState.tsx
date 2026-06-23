import { useTranslation } from "react-i18next";
import { ListChecks } from "lucide-react";
import { OnboardingCue, SuggestionChips } from "../components/ai-elements";
import { ActionCardBlock, type ActionCardAction } from "./blocks/ActionCardBlock";
import type { useRepoActions } from "./useRepoActions";

type RunAction = ReturnType<typeof useRepoActions>["run"];
export type EmptyStateMode = "default" | "lead-task" | "lead-repo-guide";

/**
 * The issue console's empty state, built by the lead host (which owns runAction /
 * promptText). ChatTimeline itself is empty-state-agnostic — it renders whatever
 * node the host injects; this is what LeadTab and the worker hosts inject. Other
 * surfaces (e.g. the dependency curator panel) inject their own node instead.
 */
export function LeadEmptyState({
  mode,
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
}: {
  mode: EmptyStateMode;
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId: number | null;
  workspaceId: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
}) {
  const { t } = useTranslation();

  if (mode === "lead-repo-guide" && runAction && promptText) {
    const actions: ActionCardAction[] = [
      { id: "empty-add-repo", kind: "add", label: t("actionCard.addRepoLabel") },
      { id: "empty-new-repo", kind: "new", label: t("actionCard.newRepoLabel") },
      { id: "empty-clone-repo", kind: "clone", label: t("actionCard.cloneRepoLabel") },
    ];
    const steps = [
      t("lead.repoGuideStepChoose"),
      t("lead.repoGuideStepMap"),
      t("lead.repoGuideStepReturn"),
    ];

    return (
      <div className="flex flex-1 items-center justify-center px-4 py-6">
        <div className="w-full max-w-[620px]">
          <ActionCardBlock
            title={t("lead.repoGuideTitle")}
            body={t("lead.repoGuideBody")}
            steps={steps}
            actions={actions}
            readOnly={false}
            busy={actionsBusy ?? {}}
            onAction={(action) =>
              void runAction({
                actionId: action.id,
                kind: action.kind,
                ctx: {
                  threadId: threadId ?? undefined,
                  preferredWorkspaceId: workspaceId,
                },
                promptText,
              })
            }
          />
          <SuggestionChips
            label={t("lead.suggestionLabel")}
            suggestions={[
              t("lead.suggestionImportRepo"),
              t("lead.suggestionCloneRepo"),
              t("lead.suggestionCreateRepo"),
            ]}
          />
        </div>
      </div>
    );
  }

  if (mode === "lead-task") {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <div className="max-w-[460px]">
          <OnboardingCue
            eyebrow={t("lead.onboardingCueEyebrow")}
            title={t("lead.taskEmptyTitle")}
            body={t("lead.onboardingCueBody")}
            icon={<ListChecks size={15} />}
          />
          <SuggestionChips
            label={t("lead.suggestionLabel")}
            suggestions={[
              t("lead.suggestionPlan"),
              t("lead.suggestionQueue"),
              t("lead.suggestionTask"),
            ]}
          />
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 items-center justify-center px-6 text-center">
      <div className="max-w-[420px]">
        <p className="text-[12px] leading-relaxed text-ink-faint">{t("lead.transcriptEmpty")}</p>
        <SuggestionChips
          label={t("lead.suggestionLabel")}
          suggestions={[
            t("lead.suggestionDescribe"),
            t("lead.suggestionAttach"),
            t("lead.suggestionSlash"),
          ]}
        />
      </div>
    </div>
  );
}
