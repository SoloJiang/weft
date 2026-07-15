import { useEffect, useState, type Dispatch, type SetStateAction } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowRight,
  Check,
  GitBranch,
  Plus,
  Sparkles,
} from "lucide-react";
import { Button } from "./ui/Button";
import { Select } from "./ui/Select";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import {
  addPendingRepo,
  removePendingRepo,
  renamePendingRepo,
  type PendingRepo,
} from "./firstRunOnboardingRepos";
import { submitOnboarding, InvalidReposError } from "./firstRunOnboardingSubmit";

const STORAGE_KEY = "weft-first-run-onboarding-v2-dismissed";

const NODES: Record<string, [number, number]> = {
  api: [150, 40],
  web: [40, 120],
  mobile: [150, 130],
  tokens: [255, 110],
};

const EDGES = [
  ["web", "api"],
  ["mobile", "api"],
  ["api", "tokens"],
  ["web", "tokens"],
];

export function FirstRunOnboarding() {
  const { workspaces, refreshWorkspaces, selectWorkspace } = useStore();
  const { t } = useTranslation();
  const [ready, setReady] = useState(false);
  const [dismissed, setDismissed] = useState(() => localStorage.getItem(STORAGE_KEY) === "1");
  const [step, setStep] = useState(0);
  const [workspaceName, setWorkspaceName] = useState("");
  const [task, setTask] = useState("");
  const [issueKind, setIssueKind] = useState("");
  const [busy, setBusy] = useState(false);
  const [repos, setRepos] = useState<PendingRepo[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const steps = t("onboarding.steps", { returnObjects: true }) as string[];
  const open = ready && workspaces.length === 0 && !dismissed;
  const last = step === steps.length - 1;
  const canEnter = !!workspaceName.trim() && !!task.trim() && !!issueKind;

  useEffect(() => {
    const timer = window.setTimeout(() => setReady(true), 300);
    return () => window.clearTimeout(timer);
  }, []);

  function dismiss() {
    localStorage.setItem(STORAGE_KEY, "1");
    setDismissed(true);
  }

  async function enter() {
    if (busy || !canEnter) return;
    setBusy(true);
    setErr(null);
    try {
      const ws = await submitOnboarding(api, {
        name: workspaceName.trim(),
        title: task.trim(),
        issueKind,
        repos,
      });
      await refreshWorkspaces();
      await selectWorkspace(ws.id);
      dismiss();
    } catch (e) {
      if (e instanceof InvalidReposError) {
        setErr(t("onboarding.repoInvalid", { paths: e.invalidRepos.join(", ") }));
      } else {
        setErr(String(e));
      }
    } finally {
      setBusy(false);
    }
  }

  async function pickRepo() {
    const picked = await api.pickFolders(t("dialog.addRepoTitle"));
    if (picked.length === 0) return;
    setRepos((current) => {
      let next = current;
      let anyDuplicate = false;
      for (const p of picked) {
        const result = addPendingRepo(next, p);
        if (!result.added) anyDuplicate = true;
        next = result.repos;
      }
      setErr(anyDuplicate ? t("onboarding.repoDuplicate") : null);
      return next;
    });
  }

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-bg text-ink">
      <div className="flex h-[54px] shrink-0 items-center gap-3 border-b border-border px-5">
        <button
          className="flex items-center gap-2 rounded-[var(--radius-md)] px-1 py-1 text-[13px] font-semibold text-ink"
          onClick={() => setStep(0)}
        >
          <img src="/weft-mark.svg" alt="" className="h-[18px] w-[18px]" draggable={false} />
          Weft
          <span className="text-[12px] font-normal text-ink-faint">{t("onboarding.kicker")}</span>
        </button>
        <div className="mx-auto hidden min-w-0 items-center gap-2 lg:flex">
          {steps.map((label, i) => (
            <button
              key={label}
              onClick={() => setStep(i)}
              className={cn(
                "flex min-w-0 items-center gap-2 rounded-[var(--radius-sm)] px-1.5 py-1 text-[12px] transition-colors",
                i === step ? "text-ink" : i < step ? "text-ink-muted" : "text-ink-faint",
              )}
            >
              <span
                className={cn(
                  "grid h-5 w-5 shrink-0 place-items-center rounded-full border font-mono text-[10px]",
                  i === step
                    ? "border-brand bg-brand text-brand-ink"
                    : i < step
                      ? "border-brand/60 bg-brand-ghost text-brand"
                      : "border-border bg-surface text-ink-faint",
                )}
              >
                {i < step ? <Check size={11} /> : i + 1}
              </span>
              <span className="truncate">{label}</span>
            </button>
          ))}
        </div>
        <button
          onClick={dismiss}
          className="ml-auto rounded-[var(--radius-md)] px-3 py-1.5 text-[12.5px] font-medium text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink lg:ml-0"
        >
          {t("onboarding.skip")}
        </button>
      </div>

      <main className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex min-h-full w-full max-w-[900px] items-center justify-center px-6 py-10">
          <OnboardingStage
            step={step}
            task={task}
            setTask={setTask}
            issueKind={issueKind}
            setIssueKind={setIssueKind}
            workspaceName={workspaceName}
            setWorkspaceName={setWorkspaceName}
            repos={repos}
            setRepos={setRepos}
            pickRepo={pickRepo}
          />
        </div>
      </main>

      <div className="flex h-[58px] shrink-0 items-center gap-3 border-t border-border bg-surface px-5">
        <Button variant="default" onClick={() => setStep((s) => Math.max(s - 1, 0))} disabled={step === 0}>
          {t("onboarding.back")}
        </Button>
        {err && <span className="max-w-[360px] truncate text-[12px] text-danger">{err}</span>}
        <span className="ml-auto text-[12px] tabular-nums text-ink-faint">
          {step + 1} / {steps.length}
        </span>
        {last ? (
          <button
            onClick={() => void enter()}
            disabled={busy || !canEnter}
            className="inline-flex h-8 items-center gap-1.5 rounded-[var(--radius-md)] border border-accent bg-accent px-3 text-[13px] font-medium text-[oklch(0.18_0.02_40)] transition-[filter,opacity] hover:brightness-105 disabled:pointer-events-none disabled:opacity-45"
          >
            <ArrowRight size={14} />
            {busy ? t("dialog.creating") : t("onboarding.enterWorkspace")}
          </button>
        ) : (
          <Button variant="primary" onClick={() => setStep((s) => Math.min(s + 1, steps.length - 1))}>
            {step === 0 ? t("onboarding.start") : t("onboarding.next")}
            <ArrowRight size={14} />
          </Button>
        )}
      </div>
    </div>
  );
}

function OnboardingStage({
  step,
  workspaceName,
  setWorkspaceName,
  task,
  setTask,
  issueKind,
  setIssueKind,
  repos,
  setRepos,
  pickRepo,
}: {
  step: number;
  workspaceName: string;
  setWorkspaceName: (v: string) => void;
  task: string;
  setTask: (v: string) => void;
  issueKind: string;
  setIssueKind: (v: string) => void;
  repos: PendingRepo[];
  setRepos: Dispatch<SetStateAction<PendingRepo[]>>;
  pickRepo: () => void;
}) {
  const { t } = useTranslation();

  if (step === 0) {
    return (
      <section className="flex max-w-[440px] flex-col items-center text-center">
        <img src="/weft-mark.svg" alt="" className="h-14 w-14" draggable={false} />
        <h1 className="mt-5 text-[42px] font-semibold leading-none text-ink">Weft</h1>
        <p className="mt-5 text-[15px] text-ink-muted">{t("onboarding.heroSubtitle")}</p>
        <p className="mt-3 max-w-[42ch] text-[13px] leading-relaxed text-ink-faint">
          {t("onboarding.heroBody")}
        </p>
      </section>
    );
  }

  if (step === 1) {
    return (
      <section className="w-full max-w-[560px]">
        <h2 className="text-[17px] font-semibold text-ink">{t("onboarding.workspaceTitle")}</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">
          {t("onboarding.workspaceBody")}
        </p>
        <label className="mt-5 flex flex-col gap-2">
          <span className="text-[10.5px] font-semibold uppercase tracking-wider text-ink-faint">
            {t("dialog.workspaceName")}
          </span>
          <input
            value={workspaceName}
            onChange={(e) => setWorkspaceName(e.currentTarget.value)}
            className="h-11 rounded-[var(--radius-md)] border border-border bg-bg px-3 text-[15px] text-ink outline-none transition-colors focus:border-brand focus:ring-2 focus:ring-brand/25"
          />
        </label>
      </section>
    );
  }

  if (step === 2) {
    return (
      <section className="w-full max-w-[560px]">
        <h2 className="text-[17px] font-semibold text-ink">{t("onboarding.addReposTitle")}</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("onboarding.addReposBody")}</p>
        <div className="mt-5 flex flex-col gap-3">
          {repos.length === 0 ? (
            <div className="rounded-[var(--radius-md)] border border-dashed border-border bg-bg/60 px-3 py-4 text-[12px] leading-relaxed text-ink-faint">
              {t("onboarding.repoEmpty")}
            </div>
          ) : (
            repos.map((repo) => (
              <div key={repo.id} className="rounded-[var(--radius-md)] border border-border bg-bg/70 p-3">
                <div className="flex items-center gap-2">
                  <GitBranch size={15} className="shrink-0 text-brand" />
                  <input
                    value={repo.name}
                    aria-label={t("onboarding.repoNameLabel")}
                    onChange={(e) =>
                      setRepos((current) => renamePendingRepo(current, repo.id, e.currentTarget.value))
                    }
                    className="min-w-0 flex-1 rounded-[var(--radius-sm)] border border-border bg-surface px-2 py-1 text-[12px] font-semibold text-ink outline-none focus:border-brand focus:ring-2 focus:ring-brand/25"
                  />
                  <button
                    type="button"
                    onClick={() => setRepos((current) => removePendingRepo(current, repo.id))}
                    className="rounded-[var(--radius-sm)] px-2 py-1 text-[11px] text-ink-faint hover:bg-brand-ghost hover:text-ink"
                  >
                    {t("onboarding.removeRepo")}
                  </button>
                </div>
                <div className="mt-2 truncate font-mono text-[11px] text-ink-faint" title={repo.path}>
                  {repo.path}
                </div>
              </div>
            ))
          )}
          <button
            type="button"
            onClick={() => void pickRepo()}
            className="inline-flex h-9 items-center justify-center gap-2 rounded-[var(--radius-md)] border border-border bg-surface px-3 text-[12.5px] font-medium text-ink transition-colors hover:border-brand hover:bg-brand-ghost"
          >
            <Plus size={14} />
            {t("onboarding.addLocalRepo")}
          </button>
        </div>
      </section>
    );
  }

  if (step === 3) {
    return (
      <section className="w-full max-w-[560px]">
        <h2 className="text-[17px] font-semibold text-ink">{t("onboarding.graphTitle")}</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("onboarding.graphBody")}</p>
        <div className="mt-5 rounded-[var(--radius-md)] border border-border bg-bg p-6">
          <OnboardingGraph />
        </div>
      </section>
    );
  }

  if (step === 4) {
    return (
      <section className="w-full max-w-[560px]">
        <h2 className="text-[17px] font-semibold text-ink">{t("onboarding.firstIssueTitle")}</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("onboarding.firstIssueBody")}</p>
        <textarea
          value={task}
          onChange={(e) => setTask(e.currentTarget.value)}
          placeholder={t("dialog.threadTitlePlaceholder")}
          rows={4}
          className="mt-5 w-full resize-none rounded-[var(--radius-md)] border border-border bg-bg px-3 py-2.5 text-[13px] leading-relaxed text-ink outline-none transition-colors focus:border-brand focus:ring-2 focus:ring-brand/25"
        />
        <label className="mt-4 flex flex-col gap-2">
          <span className="text-[10.5px] font-semibold uppercase tracking-wider text-ink-faint">
            {t("dialog.threadType")}
          </span>
          <Select
            value={issueKind}
            onValueChange={setIssueKind}
            ariaLabel={t("dialog.threadType")}
            placeholder={t("dialog.threadTypePlaceholder")}
            options={[
              { value: "feature", label: t("kind.feature") },
              { value: "bugfix", label: t("kind.bugfix") },
              { value: "refactor", label: t("kind.refactor") },
              { value: "spike", label: t("kind.spike") },
            ]}
          />
        </label>
      </section>
    );
  }

  return (
    <section className="w-full max-w-[620px]">
      <div className="flex items-center gap-2">
        <Sparkles size={16} className="text-brand" />
        <h2 className="text-[17px] font-semibold text-ink">{t("onboarding.scopeTitle")}</h2>
      </div>
      <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("onboarding.scopeBody")}</p>
      <div className="mt-5 flex flex-col gap-2">
        <ScopeLane order={1} repo="api" reason={t("onboarding.scopeApi")} />
        <ScopeLane order={2} repo="web-app" reason={t("onboarding.scopeWeb")} />
        <ScopeLane order={2} repo="mobile" reason={t("onboarding.scopeMobile")} />
      </div>
      <p className="mt-3 text-[11.5px] text-ink-faint">{t("onboarding.scopeOrder")}</p>
    </section>
  );
}

function OnboardingGraph() {
  const curve = (a: string, b: string) => {
    const A = NODES[a];
    const B = NODES[b];
    return `M${A[0]} ${A[1]} Q ${(A[0] + B[0]) / 2} ${A[1]} ${(A[0] + B[0]) / 2} ${(A[1] + B[1]) / 2} T ${B[0]} ${B[1]}`;
  };
  return (
    <svg viewBox="0 0 300 170" className="mx-auto block h-[260px] w-full max-w-[460px]">
      {EDGES.map(([a, b], i) => (
        <path key={`${a}-${b}-${i}`} d={curve(a, b)} stroke="currentColor" className="fill-none text-brand/45" strokeWidth="1.25" />
      ))}
      {Object.entries(NODES).map(([id, [x, y]]) => (
        <g key={id} transform={`translate(${x},${y})`}>
          <rect
            x="-38"
            y="-14"
            width="76"
            height="28"
            rx="8"
            className={cn(id === "api" ? "stroke-brand" : "stroke-border", "fill-surface")}
            strokeWidth={id === "api" ? "1.5" : "1"}
          />
          <text x="0" y="4" textAnchor="middle" className="fill-ink font-mono text-[11px] font-semibold">
            {id === "web" ? "web-app" : id}
          </text>
        </g>
      ))}
    </svg>
  );
}

function ScopeLane({ order, repo, reason }: { order: number; repo: string; reason: string }) {
  return (
    <div className="flex items-center gap-3 rounded-[var(--radius-md)] border border-accent/45 bg-bg px-3 py-2.5 text-[12px]">
      <span className="grid h-6 w-6 shrink-0 place-items-center rounded-full border border-accent/50 bg-bg font-mono text-[11px] text-accent">
        {order}
      </span>
      <span className="min-w-[112px] font-mono font-semibold text-ink">{repo}</span>
      <span className="min-w-0 flex-1 truncate text-[11.5px] text-ink-faint">{reason}</span>
      <GitBranch size={13} className="text-ink-faint" />
    </div>
  );
}
