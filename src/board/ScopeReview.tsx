import { useEffect, useMemo, useRef, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, GitBranch, Sparkles, X } from "lucide-react";
import { useStore } from "../state/store";
import type { ResolvedDirection } from "../lib/types";
import { Button } from "../components/ui/Button";

// One sub-task lane: exactly one write repo per direction (scope rework). The
// old read/write/none taxonomy is gone from this gate — every lane here is a
// sub-task the lead will create.
type ScopeLane = {
  key: string;
  repoName: string;
  repoKnown: boolean;
  direction: ResolvedDirection;
  order: number;
  dirIndex: number;
  defaultBranch: string;
};

/** The repo's default branch, cleaned for display: strip a leading `origin/` and
 * trim; an empty or detached (`HEAD`) capture falls back to `main`. Shown as the
 * base-branch placeholder so the field says what the work actually forks off
 * instead of a vague "default branch". The stored value stays "" (= use the repo
 * default); the backend resolves that authoritatively at worktree-creation time. */
function defaultBranchLabel(baseRef: string): string {
  const b = baseRef.trim().replace(/^origin\//, "").trim();
  if (b === "" || b === "HEAD") return "main";
  return b;
}

export function ScopeReview({ onClose }: { onClose: () => void }) {
  const { proposal, confirmProposal, threads, activeThreadId, repos } = useStore();
  const { t } = useTranslation();
  const [confirming, setConfirming] = useState(false);
  const dirs = proposal?.directions ?? [];
  const thread = threads.find((th) => th.id === activeThreadId);

  const lanes = useMemo<ScopeLane[]>(
    () =>
      dirs.map((direction, dirIndex) => {
        // One write repo per direction (scope rework): repo + required reason.
        const entry = direction.repo;
        const repo = repos.find((r) => r.id === entry.repo_id);
        return {
          key: `${dirIndex}-${entry.repo_name}`,
          repoName: entry.repo_name,
          repoKnown: entry.known,
          direction,
          order: dirIndex + 1,
          dirIndex,
          defaultBranch: defaultBranchLabel(repo?.base_ref ?? ""),
        };
      }),
    [dirs, repos],
  );

  if (!proposal) return null;

  async function confirm() {
    setConfirming(true);
    try {
      await confirmProposal();
    } finally {
      setConfirming(false);
    }
  }

  // Hug-content column: shown only inside the review dialog, so it sizes to its
  // content (grow:0) and lets the dialog cap the height. When lanes overflow that
  // cap, the body (min-h-0 + scroll) scrolls and the footer (shrink-0) stays
  // pinned — no reliance on a definite-height ancestor a max-h dialog can't give.
  return (
    <div className="relative flex min-h-0 flex-col overflow-hidden bg-bg">
      {/* Standard ghost icon button (shared hover vocabulary): the hand-rolled
          `hover:bg-surface` was invisible on this dialog's bg-bg body. */}
      <Button
        type="button"
        size="icon"
        variant="ghost"
        onClick={onClose}
        aria-label={t("scope.close")}
        className="absolute right-3 top-3 z-20"
      >
        <X size={15} />
      </Button>

      <div className="min-h-0 overflow-y-auto px-5 py-5">
        <div className="mx-auto flex w-full max-w-[820px] flex-col gap-4">
          <div className="rounded-[var(--radius-lg)] border border-border bg-surface px-4 py-3 pr-12">
            <div className="flex items-center gap-3">
              <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-accent-ghost text-accent">
                <Sparkles size={16} />
              </span>
              <div className="min-w-0 flex-1">
                <div className="text-[11px] font-medium text-ink-faint">
                  {t("scope.inputTask")}
                </div>
                <div className="truncate text-[15px] font-semibold text-ink">
                  {thread?.title ?? t("palette.issue")}
                </div>
              </div>
              <span className="rounded-full border border-brand/35 bg-brand-ghost px-2 py-0.5 text-[11px] font-medium text-brand">
                {thread?.kind ? t(`kind.${thread.kind}`, thread.kind) : t("palette.issue")}
              </span>
            </div>
          </div>

          {proposal.rationale && (
            <div className="rounded-[var(--radius-lg)] border border-border bg-surface/60 px-4 py-3">
              <div className="mb-1 text-[10.5px] font-semibold uppercase tracking-wide text-ink-faint">
                {t("scope.rationale")}
              </div>
              <p className="text-[12.5px] leading-relaxed text-ink-muted">
                {proposal.rationale}
              </p>
            </div>
          )}

          <div className="relative py-1">
            <div className="absolute left-[29px] top-0 h-full w-px bg-border" />
            <div className="absolute left-[29px] top-3 h-[calc(100%-24px)] w-px bg-[var(--c-warp-line)] opacity-60" />
            <div className="mb-2 ml-12 text-[10.5px] font-semibold uppercase tracking-wide text-ink-faint">
              {t("scope.inferred")}
            </div>
            <div className="flex flex-col gap-2">
              {lanes.map((lane, index) => (
                <ScopeLaneRow key={lane.key} lane={lane} index={index} confirming={confirming} />
              ))}
            </div>
          </div>
        </div>
      </div>

      <div className="shrink-0 border-t border-border bg-bg/95 px-5 py-3 backdrop-blur">
        <div className="mx-auto flex w-full max-w-[820px] items-center gap-3">
          <Button
            className="ml-auto"
            variant="primary"
            onClick={() => void confirm()}
            disabled={confirming || dirs.length === 0}
          >
            <GitBranch size={14} />
            {confirming ? t("scope.confirming") : t("scope.confirm", { count: dirs.length })}
          </Button>
        </div>
      </div>
    </div>
  );
}

function ScopeLaneRow({ lane, index, confirming }: { lane: ScopeLane; index: number; confirming: boolean }) {
  const { t } = useTranslation();
  const { setProposalDirectionBase, proposal } = useStore();
  // Per-proposal version: changes on EVERY re-proposal (R50-2). Threaded into BaseBranchField so a
  // re-propose with the same name/repo/base still resets a dirty (unblurred) base edit.
  const proposalVersion = proposal?.created_at ?? "";
  return (
    <motion.div
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.18, delay: index * 0.025 }}
      className="relative grid grid-cols-[52px_minmax(120px,180px)_minmax(0,1fr)_auto] items-center gap-3 rounded-[var(--radius-lg)] border border-accent/35 bg-surface px-3 py-2.5"
    >
      <div className="relative flex items-center gap-3">
        <span className="z-10 grid h-7 w-7 shrink-0 place-items-center rounded-full border border-accent/50 bg-bg font-mono text-[11px] text-accent">
          {lane.order}
        </span>
        <span className="h-px flex-1 bg-accent/70" />
      </div>

      <div className="min-w-0 truncate font-mono text-[12.5px] text-ink">{lane.repoName}</div>

      <div className="min-w-0">
        <div className="truncate text-[12.5px] font-medium text-ink">{lane.direction.name}</div>
        {!lane.repoKnown && (
          <div className="mt-0.5 flex items-center gap-1 text-[10.5px] text-waiting">
            <AlertTriangle size={10} />
            {t("scope.unknownRepo")}
          </div>
        )}
      </div>

      <BaseBranchField
        index={lane.dirIndex}
        name={lane.direction.name}
        repo={lane.repoName}
        value={lane.direction.base_branch}
        version={proposalVersion}
        defaultBranch={lane.defaultBranch}
        disabled={confirming || !!lane.direction.decision}
        onSave={setProposalDirectionBase}
      />
    </motion.div>
  );
}

function BaseBranchField({
  index,
  name,
  repo,
  value,
  version,
  defaultBranch,
  disabled,
  onSave,
}: {
  index: number;
  name: string;
  repo: string;
  value: string;
  /** Per-proposal version (R50-2): changes on EVERY re-proposal, so a re-propose with the SAME
   * name/repo/value still discards an unblurred (dirty) edit. */
  version: string;
  /** The repo's real default branch (cleaned), shown as the placeholder when the field is blank. */
  defaultBranch: string;
  disabled: boolean;
  onSave: (index: number, name: string, repo: string, base: string, expectedOldBase: string, version: string) => Promise<void>;
}) {
  const { t } = useTranslation();
  const [val, setVal] = useState(value ?? "");
  const lastLoaded = useRef(value ?? "");
  // Always the LATEST persisted base. A rejected save's handler must revert to this, not
  // the stale `value` its render captured: a same-identity re-propose can change the base
  // while an older save is in flight, and reverting to the old closure value would put a
  // stale base back into the field (which a later Create/Approve blur would then save).
  const valueRef = useRef(value);
  valueRef.current = value;
  const lastIdentity = useRef(`${name}\0${repo}`);
  const lastVersion = useRef(version);
  useEffect(() => {
    const identity = `${name}\0${repo}`;
    // Reset the input when the persisted value changes, OR the lane IDENTITY (name/repo)
    // changes, OR the proposal VERSION changes. A re-propose can swap the lane in this slot
    // without changing `value`; and a re-propose of the SAME lane with the SAME base changes
    // neither identity nor value — only the version — so include it (R50-2) to discard a dirty
    // edit that would otherwise blur-save the stale value onto the fresh proposal.
    if (
      identity !== lastIdentity.current ||
      version !== lastVersion.current ||
      (value ?? "") !== lastLoaded.current
    ) {
      lastIdentity.current = identity;
      lastVersion.current = version;
      lastLoaded.current = value ?? "";
      setVal(value ?? "");
    }
  }, [name, repo, value, version]);
  const skipNextBlur = useRef(false);
  const save = () => {
    if (disabled) return;
    if (skipNextBlur.current) {
      skipNextBlur.current = false;
      return;
    }
    const next = val.trim();
    if (next === (value ?? "").trim()) return; // unchanged
    // The lane this save belongs to. A re-propose can swap a DIFFERENT lane into this
    // keyed slot while the save is in flight; the resolve/reject handlers below must
    // not touch the input or load-tracking once that's happened, or they'd clobber the
    // new lane's freshly-reset state with this (old) lane's value.
    const savedIdentity = `${name}\0${repo}`;
    // The persisted base this field was editing FROM. The backend rejects the save if a
    // same-identity (same name+repo) re-propose changed the lane's base meanwhile —
    // optimistic concurrency the name/repo + CAS guards can't catch on their own.
    const expectedOldBase = value ?? "";
    // The proposal version this edit was composed against. The backend rejects the save if a
    // re-propose bumped the version even with the lane's base UNCHANGED (R54-2) — a gap the
    // name/repo + expectedOldBase + CAS guards can't close on their own.
    const savedVersion = version;
    // #42-3: the resolve/reject handlers must ALSO version-gate, not just identity-gate. A
    // same-name/repo/base re-propose BUMPS the version while this save is in flight; the backend
    // rejects the stale save, but any NEW edit the user starts on the fresh proposal before that
    // rejection arrives would be CLOBBERED by the revert below because the identity still matches.
    // Touch lastLoaded / the input only when the proposal hasn't moved on since THIS save was
    // issued (savedVersion still current) — otherwise this save's result is stale; leave the
    // (possibly freshly-edited) input alone.
    void onSave(index, name, repo, next, expectedOldBase, version)
      .then(() => {
        if (lastIdentity.current === savedIdentity && savedVersion === lastVersion.current) {
          lastLoaded.current = next; // mark loaded only after the save actually lands
        }
      })
      .catch(() => {
        // Save failed — revert the field to the LATEST persisted value (valueRef, not the
        // stale closure `value`) so a same-identity re-propose's fresh base isn't replaced
        // by this old save's stale one. Only when this row still shows the same lane (see
        // savedIdentity) AND the proposal version hasn't moved on (#42-3) — else we'd
        // overwrite a different lane or a freshly-edited fresh proposal.
        if (lastIdentity.current === savedIdentity && savedVersion === lastVersion.current) {
          setVal(valueRef.current ?? "");
        }
      });
  };
  return (
    <span
      className="flex shrink-0 items-center gap-1"
      title={t("scope.baseBranchHint")}
    >
      <GitBranch size={11} className="text-ink-faint" />
      <input
        value={val}
        disabled={disabled}
        onChange={(e) => setVal(e.target.value)}
        onBlur={save}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            (e.target as HTMLInputElement).blur();
          } else if (e.key === "Escape") {
            skipNextBlur.current = true;
            setVal(value ?? "");
            (e.target as HTMLInputElement).blur();
          }
        }}
        placeholder={defaultBranch}
        spellCheck={false}
        aria-label={t("scope.baseBranch")}
        className="w-24 min-w-0 rounded-[var(--radius-sm)] border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink outline-none focus:border-brand disabled:cursor-not-allowed disabled:opacity-50"
      />
    </span>
  );
}
