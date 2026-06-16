import { useEffect, useMemo, useRef, useState } from "react";
import * as RD from "@radix-ui/react-dialog";
import { useTranslation } from "react-i18next";
import { Check, FolderOpen, Loader2, Network, X } from "lucide-react";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { Input, Field, Textarea } from "../components/ui/Input";
import { Select } from "../components/ui/Select";
import { toast } from "../components/Toast";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { Worktree } from "../lib/types";
import { parseCloneSources, repoNameFromUrl } from "../lib/gitUrl";
import { cn } from "../lib/cn";

export function CreateWorkspaceDialog({ open, onOpenChange }: DProps) {
  const { createWorkspace } = useStore();
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const { t } = useTranslation();

  useEffect(() => {
    if (!open) {
      setValue("");
      setBusy(false);
      setErr(null);
    }
  }, [open]);

  async function submit() {
    if (!value.trim() || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await createWorkspace(value.trim());
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <RD.Root open={open} onOpenChange={onOpenChange}>
      <RD.Portal>
        <RD.Overlay className="weft-overlay fixed inset-0 z-50 bg-black/55 backdrop-blur-[1px]" />
        <RD.Content
          className={cn(
            "weft-pop fixed left-1/2 top-1/2 z-50 w-[min(500px,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2 overflow-hidden",
            "rounded-[var(--radius-lg)] border border-border bg-surface shadow-[0_20px_58px_-28px_rgba(0,0,0,0.9)]",
          )}
        >
          <div className="flex items-center gap-3 border-b border-border px-5 py-4">
            <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
              <Network size={15} />
            </span>
            <div className="min-w-0 flex-1">
              <RD.Title className="text-[14px] font-semibold text-ink">
                {t("dialog.newWorkspaceTitle")}
              </RD.Title>
            </div>
            <RD.Close
              aria-label={t("common.close")}
              className="-mr-1 grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink focus-visible:outline-2 focus-visible:outline-brand focus-visible:outline-offset-1"
            >
              <X size={15} />
            </RD.Close>
          </div>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col"
        >
          <div className="flex flex-col gap-4 px-5 py-5">
            <Input
              autoFocus
              placeholder={t("dialog.workspaceNamePlaceholder")}
              value={value}
              onChange={(e) => setValue(e.currentTarget.value)}
              className="h-9"
            />
            {err && <p className="text-[12px] text-danger">{err}</p>}
          </div>

          <div className="flex items-center justify-end gap-3 border-t border-border bg-bg/70 px-5 py-3">
            <div className="ml-auto flex items-center gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              type="submit"
              variant="primary"
              className="h-9 px-4"
              disabled={!value.trim() || busy}
            >
              {busy ? t("dialog.creating") : t("dialog.createWorkspace")}
            </Button>
            </div>
          </div>
        </form>
        </RD.Content>
      </RD.Portal>
    </RD.Root>
  );
}

type RepoMode = "local" | "clone" | "new";

const basename = (p: string) => p.trim().replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? "";

type RowStatus = "queued" | "cloning" | "ok" | "error";

/** Per-repo status icon in the batch-import list. */
function StatusDot({ status }: { status: RowStatus }) {
  if (status === "cloning")
    return <Loader2 size={13} className="shrink-0 animate-spin text-brand" />;
  if (status === "ok") return <Check size={13} className="shrink-0 text-running" />;
  if (status === "error") return <X size={13} className="shrink-0 text-danger" />;
  return <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-ink-faint/50" />;
}

export function AddRepoDialog({ open, onOpenChange }: DProps) {
  const { addRepos, importRepos, createRepo, projectsDir, activeWorkspaceId } = useStore();
  const { t } = useTranslation();
  const [mode, setMode] = useState<RepoMode>("local");
  const [path, setPath] = useState(""); // local — a single typed path
  const [localPaths, setLocalPaths] = useState<string[]>([]); // local — multi-picked folders
  const [url, setUrl] = useState(""); // clone — one or many pasted URLs
  const [dest, setDest] = useState(""); // clone/new parent
  const [name, setName] = useState(""); // local name / single-clone override / new name
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [progress, setProgress] = useState<Record<number, { status: RowStatus; error?: string }>>(
    {},
  );
  // Aborts the in-flight batch when the dialog is closed/cancelled mid-import.
  const abortRef = useRef<AbortController | null>(null);
  // Live mirror of the active workspace so an in-flight batch can tell it has
  // been switched out from under itself (clones target the submit-time one).
  const wsRef = useRef(activeWorkspaceId);
  wsRef.current = activeWorkspaceId;

  // What we actually clone, parsed live from the paste box (see parseCloneSources:
  // newline/space/comma separated, spaced local paths kept whole, unmodeled
  // sources passed verbatim to git for per-row status, deduped).
  const cloneTargets = useMemo(() => parseCloneSources(url), [url]);

  // Local repos to add: the multi-picked folders plus any single typed path,
  // deduped in pick-then-type order. Duplicates already in the workspace are
  // deduped again by the backend, so re-picking is harmless.
  const localTargets = useMemo(() => {
    const out: string[] = [];
    const seen = new Set<string>();
    for (const p of [...localPaths, path]) {
      const v = p.trim();
      if (!v || seen.has(v)) continue;
      seen.add(v);
      out.push(v);
    }
    return out;
  }, [localPaths, path]);

  // Reset on close; default the destination to the configured projects dir.
  useEffect(() => {
    if (!open) {
      setMode("local");
      setPath("");
      setLocalPaths([]);
      setUrl("");
      setDest("");
      setName("");
      setErr(null);
      setBusy(false);
      setProgress({});
      abortRef.current = null;
    } else {
      setDest(projectsDir);
    }
  }, [open, projectsDir]);

  // Clear stale per-row status when the URLs, the destination, OR the active
  // workspace change. A new dest means the prior successes aren't there; a new
  // workspace means the prior successes landed elsewhere — in both cases the
  // "ok" rows must re-clone, or a retry would silently skip them and toast
  // success for repos that never reached the current workspace.
  useEffect(() => {
    setProgress((p) => (Object.keys(p).length ? {} : p));
    // `mode` is a dep too: switching clone↔local must clear stale "ok" rows, or
    // runBatch would skip those indices and close without adding anything.
  }, [url, dest, activeWorkspaceId, path, localPaths, mode]);

  // Switching workspace mid-batch aborts the in-flight import: its remaining
  // clones (and any late callbacks) target the submit-time workspace, so they
  // must not keep writing into a dialog that now reflects a different one.
  useEffect(() => {
    abortRef.current?.abort();
  }, [activeWorkspaceId]);

  // Unmounting mid-batch (e.g. a resize collapses the nav, dropping this dialog
  // without a Radix onOpenChange) must still abort, or importRepos keeps queuing
  // clones after the dialog is gone.
  useEffect(() => () => abortRef.current?.abort(), []);

  // Closing/cancelling mid-batch aborts the loop so it stops queuing more clones.
  function handleOpenChange(o: boolean) {
    if (!o) abortRef.current?.abort();
    onOpenChange(o);
  }

  const canSubmit =
    !busy &&
    (mode === "local"
      ? localTargets.length >= 1
      : mode === "clone"
        ? cloneTargets.length >= 1 && !!dest.trim()
        : !!name.trim() && !!dest.trim());

  // Shared sequential-batch harness for clone & local-add. `targets` is the full
  // ordered set (for indexing + count); `run` does the work for the not-yet-ok
  // rows and reports status by ORIGINAL index. Skips already-ok rows so a retry
  // re-runs only failures. Honors mid-batch abort + workspace switch (rows land
  // in the submit-time workspace), then toasts / sets the final summary via the
  // mode-specific `report` strings.
  async function runBatch(
    targets: string[],
    report: {
      toast: (count: number) => string;
      failOne: string;
      summary: (ok: number, failed: number) => string;
    },
    run: (
      pendingIdx: number[],
      onProgress: (originalIdx: number, status: RowStatus, error?: string) => void,
      signal: AbortSignal,
    ) => Promise<void>,
  ) {
    const pendingIdx = targets.map((_, i) => i).filter((i) => progress[i]?.status !== "ok");
    if (pendingIdx.length === 0) {
      onOpenChange(false);
      return;
    }
    setProgress((p) => {
      const next = { ...p };
      for (const i of pendingIdx) next[i] = { status: "queued" };
      return next;
    });

    const controller = new AbortController();
    abortRef.current = controller;
    const submitWs = activeWorkspaceId; // these rows land in this workspace
    const errors: Record<number, string> = {};
    try {
      await run(
        pendingIdx,
        (originalIdx, status, error) => {
          // Drop callbacks from a superseded batch (close+reopen) or one whose
          // workspace was switched away — the AbortController stays "current"
          // across a workspace change, so guard on the workspace too.
          if (abortRef.current !== controller || wsRef.current !== submitWs) return;
          setProgress((p) => ({ ...p, [originalIdx]: { status, error } }));
          if (status === "error" && error) errors[originalIdx] = error;
        },
        controller.signal,
      );
    } catch (e) {
      if (abortRef.current === controller) {
        abortRef.current = null;
        setErr(String(e));
        setBusy(false);
      }
      return;
    }
    // Ignore a stale completion: after close+reopen, a newer batch owns the ref.
    if (abortRef.current !== controller) return;
    abortRef.current = null;
    setBusy(false);
    if (controller.signal.aborted) return; // dialog was closed mid-batch
    if (wsRef.current !== submitWs) return; // workspace switched — don't toast for the old one
    const failed = Object.keys(errors).length;
    if (failed === 0) {
      if (targets.length > 1) toast(report.toast(targets.length));
      onOpenChange(false);
    } else if (targets.length === 1) {
      setErr(Object.values(errors)[0] ?? report.failOne);
    } else {
      setErr(report.summary(targets.length - failed, failed));
    }
  }

  async function submit() {
    if (!canSubmit) return;
    setBusy(true);
    setErr(null);

    if (mode === "new") {
      try {
        await createRepo(name.trim() || "repo", dest.trim());
        onOpenChange(false);
      } catch (e) {
        setErr(String(e));
      } finally {
        setBusy(false);
      }
      return;
    }

    if (mode === "local") {
      // Name override only with a single target (mirrors clone); multi-picks
      // derive each name from its folder basename.
      const nameFor = (p: string) =>
        localTargets.length === 1 ? name.trim() || basename(p) || "repo" : basename(p) || "repo";
      await runBatch(
        localTargets,
        {
          toast: (count) => t("dialog.addedToast", { count }),
          failOne: t("dialog.addFailed"),
          summary: (ok, failed) => t("dialog.addSummary", { ok, failed }),
        },
        (pendingIdx, onProgress, signal) =>
          addRepos(
            pendingIdx.map((i) => ({ name: nameFor(localTargets[i]), path: localTargets[i] })),
            (j, status, error) => onProgress(pendingIdx[j], status, error),
            signal,
          ),
      );
      return;
    }

    // clone — one or many recognized URLs, each into <dest>/<name>.
    const nameFor = (u: string) =>
      cloneTargets.length === 1 ? name.trim() || repoNameFromUrl(u) || "repo" : repoNameFromUrl(u) || "repo";
    await runBatch(
      cloneTargets,
      {
        toast: (count) => t("dialog.importedToast", { count }),
        failOne: t("dialog.importFailed"),
        summary: (ok, failed) => t("dialog.importSummary", { ok, failed }),
      },
      (pendingIdx, onProgress, signal) =>
        importRepos(
          pendingIdx.map((i) => ({ url: cloneTargets[i], name: nameFor(cloneTargets[i]) })),
          dest.trim(),
          (j, status, error) => onProgress(pendingIdx[j], status, error),
          signal,
        ),
    );
  }

  async function pickInto(setter: (v: string) => void, derive?: (v: string) => void) {
    const d = await api.pickFolder(t("dialog.addRepoTitle"));
    if (!d) return;
    setter(d);
    if (derive) derive(d);
  }

  // Local multi-pick: append the chosen folders to the picked list (deduped).
  async function pickLocalFolders() {
    const dirs = await api.pickFolders(t("dialog.addRepoTitle"));
    if (dirs.length === 0) return;
    setLocalPaths((prev) => {
      const out = [...prev];
      for (const d of dirs) if (!out.includes(d)) out.push(d);
      return out;
    });
  }

  const cta =
    mode === "local"
      ? busy
        ? t("dialog.adding")
        : localTargets.length > 1
          ? t("dialog.addReposCta", { count: localTargets.length })
          : t("dialog.addRepo")
      : mode === "clone"
        ? busy
          ? t("dialog.cloning")
          : cloneTargets.length > 1
            ? t("dialog.cloneReposCta", { count: cloneTargets.length })
            : t("dialog.cloneRepo")
        : busy
          ? t("dialog.creating")
          : t("dialog.createRepoCta");

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent title={t("dialog.addRepoTitle")}>
        <div className="mb-4 flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
          {(["local", "clone", "new"] as RepoMode[]).map((m) => (
            <button
              key={m}
              type="button"
              onClick={() => {
                setMode(m);
                setErr(null);
              }}
              className={cn(
                "flex-1 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12.5px] transition-colors",
                mode === m
                  ? "bg-raised text-ink shadow-[0_1px_2px_rgba(0,0,0,0.2)]"
                  : "text-ink-faint hover:text-ink-muted",
              )}
            >
              {t(`dialog.repoMode_${m}`)}
            </button>
          ))}
        </div>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          {mode === "local" && (
            <>
              <Field label={t("dialog.repoPath")} hint={t("dialog.localPickHint")}>
                <PathInput
                  value={path}
                  placeholder="/Users/you/code/web-app"
                  onChange={setPath}
                  onPick={pickLocalFolders}
                  disabled={busy}
                />
              </Field>

              {localTargets.length >= 2 && (
                <div className="flex max-h-48 flex-col gap-0.5 overflow-y-auto rounded-[var(--radius-md)] border border-border bg-bg/50 p-2">
                  <div className="px-1 pb-1 text-[11px] text-ink-faint">
                    {t("dialog.localRecognized", { count: localTargets.length })}
                  </div>
                  {/* Iterate the actual add set so every counted row is shown; the
                      map index IS the batch's progress index (see runBatch). */}
                  {localTargets.map((p, i) => {
                    const row = progress[i];
                    const status: RowStatus = row?.status ?? "queued";
                    return (
                      <div key={p} className="flex items-center gap-2 px-1 py-0.5 text-[12px]">
                        <StatusDot status={status} />
                        <span className="shrink-0 font-medium text-ink">{basename(p) || "repo"}</span>
                        <span
                          className={cn(
                            "min-w-0 flex-1 truncate font-mono text-[11px]",
                            status === "error" ? "text-danger" : "text-ink-faint",
                          )}
                          title={row?.error || p}
                        >
                          {row?.error || p}
                        </span>
                        {!busy && (
                          <button
                            type="button"
                            aria-label={t("dialog.removeRow")}
                            onClick={() => {
                              // The typed path lives in `path`; picked folders in `localPaths`.
                              if (p === path.trim()) setPath("");
                              else setLocalPaths((prev) => prev.filter((x) => x !== p));
                            }}
                            className="shrink-0 text-ink-faint transition-colors hover:text-ink"
                          >
                            <X size={13} />
                          </button>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}
            </>
          )}

          {mode === "clone" && (
            <>
              <Field label={t("dialog.repoUrl")} hint={t("dialog.clonePasteHint")}>
                <Textarea
                  autoFocus
                  rows={3}
                  placeholder={"https://github.com/acme/web-app.git\ngit@github.com:acme/api.git"}
                  value={url}
                  onChange={(e) => setUrl(e.currentTarget.value)}
                  disabled={busy}
                />
              </Field>

              {cloneTargets.length >= 2 && (
                <div className="flex max-h-48 flex-col gap-0.5 overflow-y-auto rounded-[var(--radius-md)] border border-border bg-bg/50 p-2">
                  <div className="px-1 pb-1 text-[11px] text-ink-faint">
                    {t("dialog.cloneRecognized", { count: cloneTargets.length })}
                  </div>
                  {cloneTargets.map((u, i) => {
                    const row = progress[i];
                    const status: RowStatus = row?.status ?? "queued";
                    return (
                      <div key={u} className="flex items-center gap-2 px-1 py-0.5 text-[12px]">
                        <StatusDot status={status} />
                        <span className="shrink-0 font-medium text-ink">
                          {repoNameFromUrl(u) || "repo"}
                        </span>
                        <span
                          className={cn(
                            "min-w-0 flex-1 truncate font-mono text-[11px]",
                            status === "error" ? "text-danger" : "text-ink-faint",
                          )}
                          title={row?.error || u}
                        >
                          {row?.error || u}
                        </span>
                      </div>
                    );
                  })}
                </div>
              )}

              <Field label={t("dialog.repoLocation")}>
                <PathInput
                  value={dest}
                  placeholder="/Users/you/code"
                  onChange={setDest}
                  onPick={() => pickInto(setDest)}
                  disabled={busy}
                />
              </Field>
            </>
          )}

          {mode === "new" && (
            <Field label={t("dialog.repoLocation")}>
              <PathInput
                value={dest}
                placeholder="/Users/you/code"
                onChange={setDest}
                onPick={() => pickInto(setDest)}
                disabled={busy}
              />
            </Field>
          )}

          {(mode === "new" ||
            (mode === "local" && localTargets.length === 1) ||
            (mode === "clone" && cloneTargets.length === 1)) && (
            <Field label={t("dialog.repoName")}>
              <Input
                autoFocus={mode === "new"}
                placeholder={
                  mode === "local" ? basename(localTargets[0] ?? "") || "web-app" : "web-app"
                }
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
                disabled={busy}
              />
            </Field>
          )}

          {err && <p className="text-[12px] leading-relaxed text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => handleOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button type="submit" variant="primary" disabled={!canSubmit}>
              {cta}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A path input with a trailing native folder-picker button. */
function PathInput({
  value,
  placeholder,
  onChange,
  onPick,
  disabled,
}: {
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
  onPick: () => void;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center gap-2">
      <Input
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.currentTarget.value)}
        disabled={disabled}
      />
      <button
        type="button"
        onClick={onPick}
        disabled={disabled}
        title={t("settings.choose")}
        className="grid h-9 w-9 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink disabled:opacity-50"
      >
        <FolderOpen size={15} />
      </button>
    </div>
  );
}

export function CreateThreadDialog({ open, onOpenChange }: DProps) {
  const { createThread } = useStore();
  const { t } = useTranslation();
  const [title, setTitle] = useState("");
  const [kind, setKind] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!open) {
      setTitle("");
      setKind("");
      setBusy(false);
      setErr(null);
    }
  }, [open]);

  async function submit() {
    if (!title.trim() || !kind || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await createThread(title.trim(), kind);
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t("dialog.newThreadTitle")}
        description={t("dialog.newThreadDesc")}
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          <Field label={t("dialog.threadTitle")}>
            <Input
              autoFocus
              placeholder={t("dialog.threadTitlePlaceholder")}
              value={title}
              onChange={(e) => setTitle(e.currentTarget.value)}
            />
          </Field>
          <Field label={t("dialog.threadType")}>
            <Select
              value={kind}
              onValueChange={setKind}
              ariaLabel={t("dialog.threadType")}
              placeholder={t("dialog.threadTypePlaceholder")}
              options={[
                { value: "feature", label: t("kind.feature") },
                { value: "bugfix", label: t("kind.bugfix") },
                { value: "refactor", label: t("kind.refactor") },
                { value: "spike", label: t("kind.spike") },
              ]}
            />
          </Field>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button type="submit" variant="primary" disabled={!title.trim() || !kind || busy}>
              {busy ? t("dialog.creating") : t("dialog.createThread")}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function RenameDialog({
  open,
  onOpenChange,
  title,
  label,
  initial,
  onSubmit,
}: DProps & {
  title: string;
  label: string;
  initial: string;
  onSubmit: (value: string) => Promise<void>;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initial);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // Seed `value` only on the false→true edge so an external refresh that
  // changes `initial` while the dialog is open doesn't clobber what the user
  // is typing. We read the latest `initial` via a ref to avoid stale closures.
  const initialRef = useRef(initial);
  initialRef.current = initial;
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      setValue(initialRef.current);
      setBusy(false);
      setErr(null);
    }
    wasOpen.current = open;
  }, [open]);

  async function submit() {
    const v = value.trim();
    if (!v || busy) return;
    if (v === initial.trim()) {
      onOpenChange(false);
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await onSubmit(v);
      onOpenChange(false);
    } catch (e) {
      const raw = String(e);
      // Backend uses anyhow::bail!("…cannot be empty") / "…already" for the
      // two known rejections — translate them; fall back to a generic message
      // (the raw Rust string is logged for debugging, not shown).
      if (/empty/i.test(raw)) setErr(t("error.renameEmpty"));
      else if (/already/i.test(raw)) setErr(t("error.renameDuplicate"));
      else setErr(t("error.renameFailed"));
      if (import.meta.env.DEV) console.error("rename failed:", raw);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={title}>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          <Field label={label}>
            <Input
              autoFocus
              value={value}
              onChange={(e) => setValue(e.currentTarget.value)}
              onFocus={(e) => e.currentTarget.select()}
            />
          </Field>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button type="submit" variant="primary" disabled={!value.trim() || busy}>
              {busy ? t("dialog.renaming") : t("common.rename")}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** Confirm-only destructive dialog for deleting a finished task's worktree. Open
 *  state is driven by `worktree` (null = closed); the last shown worktree is kept
 *  in a ref so the text stays put through the close transition. */
export function DeleteWorktreeDialog({
  worktree,
  onOpenChange,
  onConfirm,
}: {
  worktree: Worktree | null;
  onOpenChange: (o: boolean) => void;
  onConfirm: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const shownRef = useRef(worktree);
  if (worktree) shownRef.current = worktree;
  const wasOpen = useRef(false);
  const open = worktree != null;
  useEffect(() => {
    if (open && !wasOpen.current) {
      setBusy(false);
      setErr(null);
    }
    wasOpen.current = open;
  }, [open]);
  const wt = shownRef.current;

  async function confirm() {
    if (busy) return;
    setBusy(true);
    setErr(null);
    try {
      await onConfirm();
      onOpenChange(false);
    } catch (e) {
      setErr(t("error.deleteWorktreeFailed"));
      if (import.meta.env.DEV) console.error("delete worktree failed:", String(e));
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t("thread.deleteWorktreeTitle")}>
        <div className="flex flex-col gap-4">
          <p className="text-[13px] leading-relaxed text-ink-muted">
            {wt ? t("thread.deleteWorktreeBody", { path: wt.path, branch: wt.branch }) : ""}
          </p>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="ghost"
              disabled={busy}
              onClick={() => onOpenChange(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button type="button" variant="danger" disabled={busy} onClick={() => void confirm()}>
              {t("thread.deleteWorktreeConfirm")}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

interface DProps {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}
