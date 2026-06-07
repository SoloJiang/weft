import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, FolderOpen } from "lucide-react";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { Input, Field } from "../components/ui/Input";
import { Select } from "../components/ui/Select";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import { cn } from "../lib/cn";

/** Generic single-text-field create dialog. */
function TextDialog({
  open,
  onOpenChange,
  title,
  description,
  label,
  placeholder,
  cta,
  onSubmit,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  title: string;
  description?: string;
  label: string;
  placeholder: string;
  cta: string;
  onSubmit: (value: string) => Promise<void>;
}) {
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const { t } = useTranslation();
  async function submit() {
    if (!value.trim() || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await onSubmit(value.trim());
      setValue("");
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={title} description={description}>
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
              placeholder={placeholder}
              value={value}
              onChange={(e) => setValue(e.currentTarget.value)}
            />
          </Field>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button type="submit" variant="primary" disabled={!value.trim() || busy}>
              {busy ? t("dialog.creating") : cta}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function CreateWorkspaceDialog({ open, onOpenChange }: DProps) {
  const { createWorkspace } = useStore();
  const { t } = useTranslation();
  return (
    <TextDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t("dialog.newWorkspaceTitle")}
      description={t("dialog.newWorkspaceDesc")}
      label={t("dialog.workspaceName")}
      placeholder={t("dialog.workspaceNamePlaceholder")}
      cta={t("dialog.createWorkspace")}
      onSubmit={createWorkspace}
    />
  );
}

type RepoMode = "local" | "clone" | "new";

const basename = (p: string) => p.trim().replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? "";
const repoNameFromUrl = (u: string) =>
  u.trim().replace(/\.git$/, "").replace(/\/+$/, "").split(/[/:]/).filter(Boolean).pop() ?? "";

export function AddRepoDialog({ open, onOpenChange }: DProps) {
  const { addRepo, cloneRepo, createRepo, projectsDir } = useStore();
  const { t } = useTranslation();
  const [mode, setMode] = useState<RepoMode>("local");
  const [path, setPath] = useState(""); // local
  const [url, setUrl] = useState(""); // clone
  const [dest, setDest] = useState(""); // clone/new parent
  const [name, setName] = useState(""); // all
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

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
    } else {
      setDest(projectsDir);
    }
  }, [open, projectsDir]);

  const finalName =
    name.trim() ||
    (mode === "local" ? basename(path) : mode === "clone" ? repoNameFromUrl(url) : "");

  const canSubmit =
    !busy &&
    (mode === "local"
      ? !!path.trim()
      : mode === "clone"
        ? !!url.trim() && !!dest.trim()
        : !!name.trim() && !!dest.trim());

  async function submit() {
    if (!canSubmit) return;
    setBusy(true);
    setErr(null);
    try {
      const n = finalName || "repo";
      if (mode === "local") await addRepo(n, path.trim());
      else if (mode === "clone") await cloneRepo(url.trim(), dest.trim(), n);
      else await createRepo(n, dest.trim());
      onOpenChange(false);
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
          : t("dialog.cloneRepo")
        : busy
          ? t("dialog.creating")
          : t("dialog.createRepoCta");

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t("dialog.addRepoTitle")} description={t("dialog.addRepoDesc")}>
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
              <Field label={t("dialog.repoUrl")}>
                <Input
                  autoFocus
                  placeholder="https://github.com/acme/web-app.git"
                  value={url}
                  onChange={(e) => setUrl(e.currentTarget.value)}
                />
              </Field>
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

          {mode !== "local" && (
            <Field label={t("dialog.repoName")}>
              <Input
                autoFocus={mode === "new"}
                placeholder="web-app"
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
              />
            </Field>
          )}
          {mode === "local" && (
            <Field label={t("dialog.repoName")}>
              <Input
                placeholder={basename(path) || "web-app"}
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
              />
            </Field>
          )}

          {mode !== "local" && dest.trim() && finalName && (
            <p className="-mt-1 text-[11px] text-ink-faint">
              {t(mode === "clone" ? "dialog.cloneHint" : "dialog.newHint", {
                path: `${dest.trim().replace(/\/+$/, "")}/${finalName}`,
              })}
            </p>
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
  const [kind, setKind] = useState("feature");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  async function submit() {
    if (!title.trim() || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await createThread(title.trim(), kind);
      setTitle("");
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
            <Button type="submit" variant="primary" disabled={!title.trim() || busy}>
              {busy ? t("dialog.creating") : t("dialog.createThread")}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function CreateDirectionDialog({
  open,
  onOpenChange,
  threadId,
}: DProps & { threadId: number }) {
  const { repos, createDirection, defaultTool } = useStore();
  const { t } = useTranslation();
  const [name, setName] = useState("main");
  const [tool, setTool] = useState(defaultTool);
  const [writes, setWrites] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const writeCount = writes.size;

  function toggle(repoId: number) {
    setWrites((cur) => {
      const next = new Set(cur);
      if (next.has(repoId)) next.delete(repoId);
      else next.add(repoId);
      return next;
    });
  }

  async function submit() {
    if (!name.trim() || busy || writeCount === 0) return;
    setBusy(true);
    setErr(null);
    try {
      const items = [...writes].map((id) => ({ repo_id: id, role: "write" as const }));
      await createDirection(threadId, name.trim(), tool, items);
      onOpenChange(false);
      setWrites(new Set());
      setName("main");
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t("dialog.newDirectionTitle")}
        description={t("dialog.newDirectionDesc")}
        className="w-[min(520px,calc(100vw-2rem))]"
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          <div className="grid grid-cols-[1fr_auto] gap-3">
            <Field label={t("dialog.directionName")}>
              <Input
                autoFocus
                placeholder="main"
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
              />
            </Field>
            <Field label={t("dialog.tool")}>
              <div className="w-32">
                <Select
                  value={tool}
                  onValueChange={setTool}
                  ariaLabel={t("dialog.tool")}
                  options={[
                    { value: "claude", label: "Claude Code" },
                    { value: "codex", label: "Codex" },
                    { value: "opencode", label: "OpenCode" },
                  ]}
                />
              </div>
            </Field>
          </div>

          <div className="flex flex-col gap-1.5">
            <span className="text-[12px] font-medium text-ink-muted">
              {t("dialog.writes")}
            </span>
            {repos.length === 0 ? (
              <p className="rounded-[var(--radius-md)] border border-dashed border-border px-3 py-4 text-center text-[12px] text-ink-faint">
                {t("scope.addReposFirst")}
              </p>
            ) : (
              <div className="flex flex-col gap-0.5 rounded-[var(--radius-md)] border border-border bg-bg/50 p-1">
                {repos.map((r) => {
                  const on = writes.has(r.id);
                  return (
                    <button
                      key={r.id}
                      type="button"
                      onClick={() => toggle(r.id)}
                      className={cn(
                        "flex items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left transition-colors",
                        on ? "bg-running/10" : "hover:bg-raised",
                      )}
                    >
                      <span
                        className={cn(
                          "grid h-4 w-4 shrink-0 place-items-center rounded border transition-colors",
                          on
                            ? "border-running bg-running/20 text-running"
                            : "border-border text-transparent",
                        )}
                      >
                        <Check size={11} />
                      </span>
                      <span className={cn("text-[13px]", on ? "text-ink" : "text-ink-muted")}>
                        {r.name}
                      </span>
                      <span className="font-mono text-[11px] text-ink-faint">
                        @{r.base_ref}
                      </span>
                    </button>
                  );
                })}
              </div>
            )}
            <span className="text-[11px] text-ink-faint">{t("dialog.writesHint")}</span>
          </div>

          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex items-center justify-between gap-2">
            <span className="text-[11px] text-ink-faint">
              {t("dialog.writeRepos", { count: writeCount })}
            </span>
            <div className="flex gap-2">
              <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                {t("common.cancel")}
              </Button>
              <Button
                type="submit"
                variant="primary"
                disabled={!name.trim() || busy || writeCount === 0}
              >
                {busy ? t("dialog.materializing") : t("dialog.createDirection")}
              </Button>
            </div>
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
