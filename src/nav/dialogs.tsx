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
import { parseGitUrls, repoNameFromUrl } from "../lib/gitUrl";
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
  const { addRepo, importRepos, createRepo, projectsDir } = useStore();
  const { t } = useTranslation();
  const [mode, setMode] = useState<RepoMode>("local");
  const [path, setPath] = useState(""); // local
  const [url, setUrl] = useState(""); // clone — one or many pasted URLs
  const [dest, setDest] = useState(""); // clone/new parent
  const [name, setName] = useState(""); // local name / single-clone override / new name
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [progress, setProgress] = useState<Record<number, { status: RowStatus; error?: string }>>(
    {},
  );

  // Recognized git URLs parsed live from the paste box (deduped, first-seen order).
  const recognized = useMemo(() => parseGitUrls(url), [url]);

  // Reset on close; default the destination to the configured projects dir.
  useEffect(() => {
    if (!open) {
      setMode("local");
      setPath("");
      setUrl("");
      setDest("");
      setName("");
      setErr(null);
      setBusy(false);
      setProgress({});
    } else {
      setDest(projectsDir);
    }
  }, [open, projectsDir]);

  // Clear stale per-row status when the pasted URLs change.
  useEffect(() => {
    setProgress((p) => (Object.keys(p).length ? {} : p));
  }, [url]);

  const finalName = name.trim() || (mode === "local" ? basename(path) : "");

  const canSubmit =
    !busy &&
    (mode === "local"
      ? !!path.trim()
      : mode === "clone"
        ? recognized.length >= 1 && !!dest.trim()
        : !!name.trim() && !!dest.trim());

  async function submit() {
    if (!canSubmit) return;
    setBusy(true);
    setErr(null);
    try {
      if (mode === "local") {
        await addRepo(finalName || "repo", path.trim());
        onOpenChange(false);
      } else if (mode === "new") {
        await createRepo(finalName || "repo", dest.trim());
        onOpenChange(false);
      } else {
        // clone — one or many recognized URLs, each into <dest>/<name>
        const items =
          recognized.length === 1
            ? [
                {
                  url: recognized[0],
                  name: name.trim() || repoNameFromUrl(recognized[0]) || "repo",
                },
              ]
            : recognized.map((u) => ({ url: u, name: repoNameFromUrl(u) || "repo" }));
        setProgress(
          Object.fromEntries(items.map((_, i) => [i, { status: "queued" as RowStatus }])),
        );
        const errors: Array<string | undefined> = [];
        await importRepos(items, dest.trim(), (i, status, error) => {
          setProgress((p) => ({ ...p, [i]: { status, error } }));
          if (status === "error") errors[i] = error;
        });
        const failed = errors.filter(Boolean).length;
        if (failed === 0) {
          if (items.length > 1) toast(t("dialog.importedToast", { count: items.length }));
          onOpenChange(false);
        } else if (items.length === 1) {
          setErr(errors.find(Boolean) ?? t("dialog.importFailed"));
        } else {
          setErr(t("dialog.importSummary", { ok: items.length - failed, failed }));
        }
      }
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function pickInto(setter: (v: string) => void, derive?: (v: string) => void) {
    const d = await api.pickFolder(t("dialog.addRepoTitle"));
    if (!d) return;
    setter(d);
    if (derive) derive(d);
  }

  const cta =
    mode === "local"
      ? busy
        ? t("dialog.creating")
        : t("dialog.addRepo")
      : mode === "clone"
        ? busy
          ? t("dialog.cloning")
          : recognized.length > 1
            ? t("dialog.cloneReposCta", { count: recognized.length })
            : t("dialog.cloneRepo")
        : busy
          ? t("dialog.creating")
          : t("dialog.createRepoCta");

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
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
            <Field label={t("dialog.repoPath")}>
              <PathInput
                value={path}
                placeholder="/Users/you/code/web-app"
                onChange={setPath}
                onPick={() => pickInto(setPath)}
              />
            </Field>
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
                />
              </Field>

              {recognized.length >= 2 && (
                <div className="flex flex-col gap-0.5 rounded-[var(--radius-md)] border border-border bg-bg/50 p-2">
                  <div className="px-1 pb-1 text-[11px] text-ink-faint">
                    {t("dialog.cloneRecognized", { count: recognized.length })}
                  </div>
                  {recognized.map((u, i) => {
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

              {recognized.length === 0 && url.trim() !== "" && (
                <p className="text-[12px] text-ink-faint">{t("dialog.cloneNoneRecognized")}</p>
              )}

              <Field label={t("dialog.repoLocation")}>
                <PathInput
                  value={dest}
                  placeholder="/Users/you/code"
                  onChange={setDest}
                  onPick={() => pickInto(setDest)}
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
              />
            </Field>
          )}

          {(mode === "local" ||
            mode === "new" ||
            (mode === "clone" && recognized.length === 1)) && (
            <Field label={t("dialog.repoName")}>
              <Input
                autoFocus={mode === "new"}
                placeholder={mode === "local" ? basename(path) || "web-app" : "web-app"}
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
              />
            </Field>
          )}

          {err && <p className="text-[12px] leading-relaxed text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
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
}: {
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
  onPick: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center gap-2">
      <Input
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.currentTarget.value)}
      />
      <button
        type="button"
        onClick={onPick}
        title={t("settings.choose")}
        className="grid h-9 w-9 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
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

interface DProps {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}
