import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Maximize2, Pencil, X } from "lucide-react";

import { api } from "../lib/api";
import type { TestPlan } from "../lib/types";
import { currentLang } from "../i18n";
import { toast } from "../components/Toast";
import { Button } from "../components/ui/Button";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { clampPanelWidth } from "./panelWidth";
import { cn } from "../lib/cn";
import type { NodePath } from "./MindMapView";
import type { MindMapEditorHandle } from "./MindMapEditor";

// markmap + d3 (preview) and mind-elixir (editor) stay out of the main bundle;
// the panel is rarely open, and the editor loads only when editing begins.
const MindMapView = lazy(() => import("./MindMapView"));
const MindMapEditor = lazy(() => import("./MindMapEditor"));

const MIN_W = 360;
const MAX_W = 860;
const clampW = (x: number) => clampPanelWidth(x, MIN_W, MAX_W);

type Mode = "preview" | "edit";

/**
 * Count the leaf cases in a test-plan markdown tree — a list item with no
 * deeper list item directly below it (nested items are groupings). Faithfully
 * mirrors the backend `lead_chat::test_plan::summarize` `caseCount`, the single
 * source of truth the timeline summary card carries. Reproduced here so the plan
 * card can count against the LIVE document (this table) instead of a stale
 * summary row, keeping "shaped against N cases" in step with what View opens.
 */
export function testPlanCaseCount(md: string): number {
  // Indent (leading-whitespace width) of each list item, in document order.
  const indents: number[] = [];
  for (const line of md.split("\n")) {
    const trimmed = line.trimStart();
    const indent = line.length - trimmed.length;
    // `#`/`##` are the title and group headings, never individual cases.
    if (trimmed.startsWith("# ") || trimmed.startsWith("## ")) continue;
    if (!trimmed.startsWith("- ") && !trimmed.startsWith("* ")) continue;
    if (!trimmed.slice(2).trim()) continue;
    indents.push(indent);
  }
  let count = 0;
  for (let i = 0; i < indents.length; i++) {
    const next = indents[i + 1];
    // A leaf: the next item (if any) is not more deeply indented.
    if (next === undefined || next <= indents[i]) count += 1;
  }
  return count;
}

/**
 * The issue's test-case document as a real third column (same pattern as
 * DiffPanel): markmap preview, markdown source editing, node-anchored
 * ask/suggest actions that post back into the chat, and a fullscreen dialog.
 * The table is the single source of truth — the panel refetches on open and
 * whenever the lead re-emits (a new test_cases card lands in the timeline).
 */
export function TestPlanPanel({
  threadId,
  refreshKey,
  onClose,
  onSendToLead,
  onEdited,
}: {
  threadId: number;
  /** Bump when the lead re-emits (latest test_cases card id) → refetch. */
  refreshKey?: number;
  onClose: () => void;
  /** Deliver a node-anchored question/suggestion as a chat message to the lead. */
  onSendToLead?: (text: string) => void;
  /** A user save rewrote the test_plan table (no test_cases card is emitted for
   *  panel edits) — lets the host recount the plan card against the new doc. */
  onEdited?: () => void;
}) {
  const { t } = useTranslation();
  const [w, setW] = useState(() => clampW(Number(localStorage.getItem("weft-testplan-w")) || 560));
  const [dragging, setDragging] = useState(false);
  const [plan, setPlan] = useState<TestPlan | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [mode, setMode] = useState<Mode>("preview");
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [selectedPath, setSelectedPath] = useState<NodePath | null>(null);
  const [noteDraft, setNoteDraft] = useState("");

  useEffect(() => {
    localStorage.setItem("weft-testplan-w", String(w));
  }, [w]);

  useEffect(() => {
    const onResize = () => setW((cur) => clampW(cur));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  useEffect(() => {
    if (!dragging) return;
    const move = (e: PointerEvent) => setW(clampW(window.innerWidth - e.clientX));
    const up = () => setDragging(false);
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
  }, [dragging]);

  // Refetch on open, thread switch, and lead re-emit. Drop stale responses.
  // A re-emit while EDITING would make Save overwrite the freshly derived
  // document with a draft based on the old one — drop the stale draft back to
  // preview and say so. (Thread switches remount via key, so mode is fresh.)
  // The toast's `t` lives in a ref: locale toggles swap the `t` identity, and
  // a translation change must never re-run this effect and discard a draft.
  // The live mindmap editor (edit mode only) — Save flushes its current tree
  // through this rather than trusting the debounced draft.
  const editorRef = useRef<MindMapEditorHandle>(null);
  const tRef = useRef(t);
  tRef.current = t;
  useEffect(() => {
    let alive = true;
    setLoaded(false);
    void api
      .getTestPlan(threadId)
      .then((p) => {
        if (!alive) return;
        setPlan(p);
        setLoaded(true);
        setSelectedPath(null);
        setMode((m) => {
          if (m === "edit") toast(tRef.current("testPlan.refreshedWhileEditing"));
          return "preview";
        });
        setDraft("");
      })
      .catch(() => {
        if (alive) setLoaded(true);
      });
    return () => {
      alive = false;
    };
  }, [threadId, refreshKey]);

  const startEdit = () => {
    setDraft(plan?.content ?? "");
    setMode("edit");
  };

  const saveEdit = async () => {
    // No-op guard: if the editor isn't mounted yet (lazy Suspense still loading —
    // ref is null, so nothing could have been edited) OR the user made no
    // structural edit (open-and-Save), leave edit mode WITHOUT persisting. flush()
    // serializes to canonical markdown (headings → bullets, heading-less root gets
    // a fallback title), so persisting a no-op would needlessly reshape the source
    // and announce a phantom edit to the lead.
    if (!editorRef.current || !editorRef.current.isDirty()) {
      setMode("preview");
      return;
    }
    // Flush the editor's live tree — a rename/add/drag right before Save schedules
    // its onChange on a debounce that may not have fired, so `draft` can lag. Fall
    // back to `draft` if the editor is gone (unmounted between click and read).
    const content = (editorRef.current?.flush() ?? draft).trim();
    if (!content) {
      toast(t("testPlan.emptyError"));
      return;
    }
    setSaving(true);
    try {
      await api.saveTestPlan(threadId, content);
      setPlan((p) =>
        p ? { ...p, content, source: "user" } : { id: 0, thread_id: threadId, content, source: "user", updated_at: "" },
      );
      setMode("preview");
      // The table changed but no test_cases card lands for panel edits — tell the
      // host so the plan card recounts against the saved document.
      onEdited?.();
      // Best-effort: the lead learns the new content; persisting already
      // succeeded, so a stopped lead only means it catches up via the
      // get_test_cases planner tool later.
      const delivered = await api.postLeadToolResult(
        threadId,
        { tool: "test_cases_updated", source: "user", content },
        currentLang(),
      );
      if (!delivered) toast(t("testPlan.savedLeadOffline"));
      // Also drop a SHORT visible message: hidden feedback creates no user row,
      // so without this a pending plan card would stay approvable even though
      // the plan was shaped against the pre-edit cases. The visible row engages
      // the existing pending-reply guards (and gives the edit a chat anchor).
      onSendToLead?.(t("testPlan.editedNotice"));
    } catch (e) {
      toast(String(e));
    } finally {
      setSaving(false);
    }
  };

  const onNodeClick = useCallback((path: NodePath) => {
    setSelectedPath(path);
    setNoteDraft("");
  }, []);

  const sendNote = (kind: "ask" | "suggest") => {
    const note = noteDraft.trim();
    if (!note || !selectedPath || !onSendToLead) return;
    const anchor = selectedPath.join(" › ");
    const label = kind === "ask" ? t("testPlan.askPrefix") : t("testPlan.suggestPrefix");
    onSendToLead(`【${t("testPlan.defaultTitle")} › ${anchor}】${label}${note}`);
    setSelectedPath(null);
    setNoteDraft("");
  };

  const body = () => {
    if (!loaded) return null;
    if (!plan) {
      return (
        <div className="grid flex-1 place-items-center px-6 text-center text-xs leading-relaxed text-ink-faint">
          {t("testPlan.empty")}
        </div>
      );
    }
    if (mode === "edit") {
      return (
        <div className="flex min-h-0 flex-1 flex-col gap-2 p-3">
          <div className="min-h-0 flex-1 overflow-hidden rounded-[var(--radius-md)] border border-border bg-surface">
            <Suspense fallback={<div className="p-4 text-xs text-ink-faint">{t("testPlan.loading")}</div>}>
              <MindMapEditor
                ref={editorRef}
                markdown={plan.content}
                rootLabel={t("testPlan.defaultTitle")}
                locale={currentLang()}
                onChange={setDraft}
              />
            </Suspense>
          </div>
          <div className="flex items-center gap-2">
            <span className="mr-auto min-w-0 truncate text-[11px] text-ink-faint">
              {t("testPlan.editHint")}
            </span>
            <Button variant="ghost" size="sm" onClick={() => setMode("preview")} disabled={saving}>
              {t("testPlan.cancel")}
            </Button>
            <Button variant="primary" size="sm" onClick={() => void saveEdit()} disabled={saving}>
              {saving ? "…" : t("testPlan.save")}
            </Button>
          </div>
        </div>
      );
    }
    return (
      <div className="relative min-h-0 flex-1">
        <Suspense fallback={<div className="p-4 text-xs text-ink-faint">{t("testPlan.loading")}</div>}>
          <MindMapView markdown={plan.content} onNodeClick={onSendToLead ? onNodeClick : undefined} />
        </Suspense>
        {selectedPath && (
          <div className="absolute inset-x-3 bottom-3 rounded-[var(--radius-md)] border border-border bg-raised p-2 shadow-[0_8px_24px_-12px_rgba(0,0,0,0.6)]">
            <div className="mb-1.5 truncate text-[11px] text-ink-faint">
              {selectedPath.join(" › ")}
            </div>
            <div className="flex items-center gap-2">
              <input
                autoFocus
                value={noteDraft}
                onChange={(e) => setNoteDraft(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    sendNote("ask");
                  }
                  if (e.key === "Escape") setSelectedPath(null);
                }}
                placeholder={t("testPlan.notePlaceholder")}
                className="min-w-0 flex-1 rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-1.5 text-[12px] text-ink outline-none focus:border-brand/60"
              />
              <Button variant="ghost" size="sm" onClick={() => sendNote("suggest")} disabled={!noteDraft.trim()}>
                {t("testPlan.suggest")}
              </Button>
              <Button variant="primary" size="sm" onClick={() => sendNote("ask")} disabled={!noteDraft.trim()}>
                {t("testPlan.ask")}
              </Button>
            </div>
          </div>
        )}
      </div>
    );
  };

  return (
    <div
      style={{ width: w }}
      className={cn(
        "relative flex shrink-0 overflow-hidden border-l border-border bg-bg",
        !dragging && "transition-[width] duration-200 ease-out motion-reduce:transition-none",
      )}
    >
      {/* resize handle on the column's left edge */}
      <div
        onPointerDown={() => setDragging(true)}
        className="absolute inset-y-0 left-0 z-10 w-1 cursor-col-resize hover:bg-brand/40"
      />
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex items-center gap-1.5 border-b border-border px-3 py-2">
          <span className="min-w-0 truncate text-[12px] font-semibold text-ink">
            {t("testPlan.title")}
          </span>
          <span className="ml-auto" />
          {plan && mode === "preview" && (
            <>
              <button
                onClick={() => setFullscreen(true)}
                title={t("testPlan.fullscreen")}
                aria-label={t("testPlan.fullscreen")}
                className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
              >
                <Maximize2 size={13} />
              </button>
              <button
                onClick={startEdit}
                title={t("testPlan.edit")}
                aria-label={t("testPlan.edit")}
                className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
              >
                <Pencil size={13} />
              </button>
            </>
          )}
          <button
            onClick={onClose}
            aria-label={t("common.close")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <X size={15} />
          </button>
        </header>
        {body()}
      </div>

      <Dialog open={fullscreen} onOpenChange={setFullscreen}>
        {fullscreen && plan && (
          <DialogContent
            title={t("testPlan.title")}
            className="h-[calc(100vh-4rem)] w-[calc(100vw-4rem)] max-w-none"
          >
            <div className="h-[calc(100%-2.5rem)]">
              <Suspense fallback={<div className="p-4 text-xs text-ink-faint">{t("testPlan.loading")}</div>}>
                <MindMapView markdown={plan.content} />
              </Suspense>
            </div>
          </DialogContent>
        )}
      </Dialog>
    </div>
  );
}
