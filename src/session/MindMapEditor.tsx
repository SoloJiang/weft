import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import MindElixir, {
  type MindElixirInstance,
  type NodeObj,
  type Options,
} from "mind-elixir";
import { en, zh_CN } from "mind-elixir/i18n";
import "mind-elixir/style.css";

import { parseTestPlanMarkdown, mindTreeToMarkdown, type MindTree } from "./mindTree";

/** Imperative handle: Save reads the live tree synchronously, bypassing the
 *  debounced onChange so the final edit before a quick Save is never lost. */
export interface MindMapEditorHandle {
  /** Current tree serialized to markdown, or null before the editor mounts. */
  flush: () => string | null;
  /** Whether the user has made any structural edit since mount — lets Save treat
   *  an open-and-Save (no edit) as a no-op without rewriting the stored source. */
  isDirty: () => boolean;
}

// mind-elixir needs a unique id per node; a module counter is enough (ids are
// never persisted — we serialize topics/structure back to markdown, not ids).
let uid = 0;
const toNodeObj = (t: MindTree): NodeObj => ({
  id: `weft-me-${uid++}`,
  topic: t.topic,
  children: t.children.map(toNodeObj),
});
const fromNodeObj = (n: NodeObj): MindTree => ({
  topic: n.topic ?? "",
  children: (n.children ?? []).map(fromNodeObj),
});

// mind-elixir reads these off the container; pointing them at our `--c-*` tokens
// (which flip on `data-theme`) makes the editor follow the app's light/dark
// theme with no JS theme-switching. Matches the markmap preview's brand palette.
const CSS_VAR = {
  "--main-color": "var(--c-ink)",
  "--main-bgcolor": "var(--c-bg)",
  "--color": "var(--c-ink)",
  "--bgcolor": "var(--c-surface)",
  "--selected": "var(--c-brand)",
  "--root-color": "var(--c-brand-ink)",
  "--root-bgcolor": "var(--c-brand)",
  "--root-border-color": "var(--c-brand)",
  "--topic-padding": "8px",
  "--panel-color": "var(--c-ink)",
  "--panel-bgcolor": "var(--c-surface)",
  "--panel-border-color": "var(--c-border)",
} as const;

const PALETTE = ["#26a6ba", "#f27d53", "#8aa9c9", "#7c9885", "#b087c9", "#c9a15f"];

/**
 * Editable mindmap for the test-case document (mind-elixir). Where the markmap
 * preview is a read-only SVG, this surface supports real structural editing:
 * double-click a node to rename, Enter / Tab (or the context menu) to add a
 * sibling / child, drag a node onto another to reparent, and undo/redo. Edits
 * mutate mind-elixir's own tree; on each operation we serialize that tree back to
 * canonical markdown and hand it up via `onChange`, so the panel's Save persists
 * exactly what is on screen. Lazy-loaded so mind-elixir stays out of the main
 * bundle (the panel is rarely open, and never in edit mode until asked).
 */
const MindMapEditor = forwardRef<
  MindMapEditorHandle,
  {
    markdown: string;
    /** Names the root when the document has no single heading of its own. */
    rootLabel: string;
    locale: "en" | "zh";
    onChange: (markdown: string) => void;
  }
>(function MindMapEditor({ markdown, rootLabel, locale, onChange }, ref) {
  const elRef = useRef<HTMLDivElement>(null);
  // The live instance, exposed via the imperative handle so Save can read the
  // current tree synchronously (the debounced onChange may not have fired yet).
  const mindRef = useRef<MindElixirInstance | null>(null);
  // Set on the first real operation (after the ready guard). Save reads it to
  // treat an untouched open-and-Save as a no-op — no rewrite, no lead notify.
  // The tree serialized at init; dirty = the live tree serializes differently.
  const initialMarkdownRef = useRef<string>("");
  // Keep the latest callbacks/labels in refs so the init effect runs exactly
  // once per mount — re-initializing would discard the user's in-progress edits.
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const rootLabelRef = useRef(rootLabel);
  rootLabelRef.current = rootLabel;

  useImperativeHandle(
    ref,
    () => {
      const serialize = () => {
        const mind = mindRef.current;
        return mind
          ? mindTreeToMarkdown(fromNodeObj(mind.getData().nodeData), rootLabelRef.current)
          : null;
      };
      return {
        flush: serialize,
        // Compare the live serialization to the init snapshot rather than trusting
        // the operation stream: a finishEdit after an inspect-only double-click
        // fires an `operation` but leaves the tree unchanged, which must not count.
        isDirty: () => {
          const cur = serialize();
          return cur != null && cur !== initialMarkdownRef.current;
        },
      };
    },
    [],
  );

  useEffect(() => {
    const el = elRef.current;
    if (!el) return;

    const tree = parseTestPlanMarkdown(markdown);
    if (!tree.topic.trim()) tree.topic = rootLabelRef.current;

    const options: Options = {
      el,
      direction: MindElixir.RIGHT,
      editable: true,
      toolBar: false,
      keypress: true,
      allowUndo: true,
      contextMenu: {
        locale: locale === "zh" ? zh_CN : en,
        focus: false,
        link: false,
        extend: [],
      },
      // Veto removing the root: Delete/Backspace on the title routes through
      // removeNodes, and the rootless node leaves the map in a broken half-state.
      before: {
        removeNodes: (tpcs) =>
          !tpcs.some(
            (t) => (t as HTMLElement).parentElement?.tagName.toLowerCase() === "me-root",
          ),
      },
      theme: { name: "weft", palette: PALETTE, cssVar: CSS_VAR },
    };
    const mind: MindElixirInstance = new MindElixir(options);
    mind.init({ nodeData: toNodeObj(tree) });
    mindRef.current = mind;
    // Snapshot the initial serialization for the no-op (isDirty) check.
    initialMarkdownRef.current = mindTreeToMarkdown(
      fromNodeObj(mind.getData().nodeData),
      rootLabelRef.current,
    );
    // mind-elixir's built-in paste renders a copied NodeObj straight from the
    // clipboard (its MIND-ELIXIR-WAIT-COPY payload) before our serializer runs —
    // arbitrary node HTML in the webview. Block that paste in the capture phase
    // (before mind-elixir's handler); ordinary text paste is unaffected.
    const onPaste = (e: ClipboardEvent) => {
      if ((e.clipboardData?.getData("text/plain") ?? "").includes("MIND-ELIXIR-WAIT-COPY")) {
        e.preventDefault();
        e.stopImmediatePropagation();
      }
    };
    el.addEventListener("paste", onPaste, true);
    // Fit the whole tree into the panel on open — mind-elixir's default view
    // centers the root at native scale, which pushes a right-growing tree off
    // the edge in a narrow column. A rAF lets the initial layout settle first.
    const fitFrame = requestAnimationFrame(() => mind.scaleFit());

    // Ignore the operations mind-elixir may fire while wiring up — only a real
    // user edit should mark the draft dirty (so an open-then-Save with no change
    // writes the document back untouched, not a re-canonicalized diff).
    let ready = false;
    const readyTimer = setTimeout(() => {
      ready = true;
    }, 0);

    let emitTimer: ReturnType<typeof setTimeout> | null = null;
    const emit = () => {
      if (!ready) return;
      if (emitTimer) clearTimeout(emitTimer);
      emitTimer = setTimeout(() => {
        const data = mind.getData();
        onChangeRef.current(
          mindTreeToMarkdown(fromNodeObj(data.nodeData), rootLabelRef.current),
        );
      }, 300);
    };
    mind.bus.addListener("operation", emit);

    return () => {
      cancelAnimationFrame(fitFrame);
      clearTimeout(readyTimer);
      if (emitTimer) clearTimeout(emitTimer);
      el.removeEventListener("paste", onPaste, true);
      mind.bus.removeListener("operation", emit);
      mind.destroy();
      mindRef.current = null;
    };
    // Init once per mount. `markdown` is the seed only; after mount THIS editor
    // is the source of truth (it pushes changes out, never takes them back in),
    // and the panel remounts it on thread switch / re-emit via React key + the
    // conditional edit-mode render, so a fresh document always re-seeds cleanly.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <div ref={elRef} className="mind-elixir h-full w-full" />;
});

export default MindMapEditor;
