import { useState } from "react";
import { Check } from "lucide-react";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { Input, Field } from "../components/ui/Input";
import { Select } from "../components/ui/Select";
import { useStore } from "../state/store";
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
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={!value.trim() || busy}>
              {busy ? "Working…" : cta}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function CreateWorkspaceDialog({ open, onOpenChange }: DProps) {
  const { createWorkspace } = useStore();
  return (
    <TextDialog
      open={open}
      onOpenChange={onOpenChange}
      title="New workspace"
      description="A logical group of repositories. References, not copies."
      label="Name"
      placeholder="payments platform"
      cta="Create workspace"
      onSubmit={createWorkspace}
    />
  );
}

export function AddRepoDialog({ open, onOpenChange }: DProps) {
  const { addRepo } = useStore();
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  async function submit() {
    if (!path.trim() || busy) return;
    setBusy(true);
    setErr(null);
    try {
      const fallbackName = name.trim() || path.trim().split("/").filter(Boolean).pop() || "repo";
      await addRepo(fallbackName, path.trim());
      setName("");
      setPath("");
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
        title="Add repository"
        description="Reference an existing local git repo by path."
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          <Field label="Local git path">
            <Input
              autoFocus
              placeholder="/Users/you/code/web-app"
              value={path}
              onChange={(e) => setPath(e.currentTarget.value)}
            />
          </Field>
          <Field label="Name" hint="Defaults to the folder name.">
            <Input
              placeholder="web-app"
              value={name}
              onChange={(e) => setName(e.currentTarget.value)}
            />
          </Field>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={!path.trim() || busy}>
              {busy ? "Adding…" : "Add repository"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function CreateThreadDialog({ open, onOpenChange }: DProps) {
  const { createThread } = useStore();
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
        title="New thread"
        description="A work line. Its planning ceremony scales with type."
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-4"
        >
          <Field label="Title">
            <Input
              autoFocus
              placeholder="Add SSO login"
              value={title}
              onChange={(e) => setTitle(e.currentTarget.value)}
            />
          </Field>
          <Field label="Type">
            <Select
              value={kind}
              onValueChange={setKind}
              ariaLabel="Thread type"
              options={[
                { value: "feature", label: "Feature" },
                { value: "bugfix", label: "Bugfix" },
                { value: "refactor", label: "Refactor" },
                { value: "spike", label: "Spike" },
              ]}
            />
          </Field>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={!title.trim() || busy}>
              {busy ? "Creating…" : "Create thread"}
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
  const { repos, createDirection } = useStore();
  const [name, setName] = useState("main");
  const [tool, setTool] = useState("claude");
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
        title="New direction"
        description="Pick which repos this direction will write. Each gets its own worktree."
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
            <Field label="Name">
              <Input
                autoFocus
                placeholder="main"
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
              />
            </Field>
            <Field label="Tool">
              <div className="w-32">
                <Select
                  value={tool}
                  onValueChange={setTool}
                  ariaLabel="Tool"
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
              Writes
            </span>
            {repos.length === 0 ? (
              <p className="rounded-[var(--radius-md)] border border-dashed border-border px-3 py-4 text-center text-[12px] text-ink-faint">
                No repos in this workspace yet. Add one first.
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
            <span className="text-[11px] text-ink-faint">
              Each write repo gets an isolated worktree. Agents read any repo
              freely, so reads aren't listed.
            </span>
          </div>

          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex items-center justify-between gap-2">
            <span className="text-[11px] text-ink-faint">
              {writeCount} write {writeCount === 1 ? "repo" : "repos"}
            </span>
            <div className="flex gap-2">
              <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                type="submit"
                variant="primary"
                disabled={!name.trim() || busy || writeCount === 0}
              >
                {busy ? "Materializing…" : "Create direction"}
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
