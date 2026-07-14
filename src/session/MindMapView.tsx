import { useEffect, useMemo, useRef } from "react";
import { Transformer } from "markmap-lib";
import { Markmap } from "markmap-view";

/** Path of ancestor titles down to the clicked node, root first. */
export type NodePath = string[];

// markmap-view binds each rendered node's datum via d3; the datum carries
// `content` (HTML-ish text) and `state.path` ("1.2.3" — ancestor chain by
// index), which we use to rebuild the human-readable path for node actions.
interface MarkmapDatum {
  content: string;
  state?: { path?: string };
  children?: MarkmapDatum[];
}

const transformer = new Transformer();
// markmap ships markdown-it with `html: true` and the view inserts node
// content via `.html(...)` — raw HTML in a lead/user-authored document (e.g.
// `<img onerror=…>`) would execute inside the Tauri webview. Disable raw HTML;
// it renders as escaped text instead.
transformer.md.set({ html: false });

function textOf(content: string): string {
  // Node content may carry inline HTML (markmap renders via foreignObject);
  // strip tags for the plain-text path label. DOMParser yields an inert
  // document — nothing executes or loads, unlike innerHTML on a live element.
  const doc = new DOMParser().parseFromString(content, "text/html");
  return (doc.body.textContent || "").trim();
}

/** Index rendered nodes by their `state.path` at click time. The transformer's
 *  output carries no `state` — markmap-view fills it in `setData` — so the
 *  index must come from the LIVE DOM datums, which are always in sync with
 *  what is on screen (and a visible node's ancestors are visible too). */
function liveIndexByPath(svg: SVGSVGElement): Map<string, MarkmapDatum> {
  const out = new Map<string, MarkmapDatum>();
  svg.querySelectorAll("g.markmap-node").forEach((g) => {
    const datum = (g as Element & { __data__?: MarkmapDatum }).__data__;
    const p = datum?.state?.path;
    if (p && datum) out.set(p, datum);
  });
  return out;
}

/**
 * Interactive mindmap for a markdown tree (markmap). Renders into an SVG that
 * fills its parent; circles keep markmap's native fold/unfold, while clicking a
 * node's TEXT reports the node path upward for the panel's ask/suggest actions.
 * This module is lazy-loaded (React.lazy) so markmap/d3 stay out of the main bundle.
 */
export default function MindMapView({
  markdown,
  onNodeClick,
}: {
  markdown: string;
  /** Called with the root-first title path of the clicked node. */
  onNodeClick?: (path: NodePath) => void;
}) {
  const svgRef = useRef<SVGSVGElement>(null);
  const mmRef = useRef<Markmap | null>(null);
  const { root } = useMemo(() => transformer.transform(markdown), [markdown]);

  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    if (!mmRef.current) {
      mmRef.current = Markmap.create(svg, {
        autoFit: true,
        duration: 200,
      });
    }
    mmRef.current.setData(root);
    void mmRef.current.fit();
  }, [root]);

  useEffect(() => {
    return () => {
      mmRef.current?.destroy();
      mmRef.current = null;
    };
  }, []);

  useEffect(() => {
    const svg = svgRef.current;
    if (!svg || !onNodeClick) return;
    const onClick = (e: MouseEvent) => {
      const target = e.target as Element | null;
      // Circles are markmap's fold toggles — leave them alone.
      if (!target || target.closest("circle")) return;
      const g = target.closest("g.markmap-node") as (Element & { __data__?: MarkmapDatum }) | null;
      const datum = g?.__data__;
      const path = datum?.state?.path;
      if (!path) return;
      const index = liveIndexByPath(svg);
      // "1.55.60" → prefix chain "1", "1.55", "1.55.60" — root-first titles.
      const segs = path.split(".");
      const titles: string[] = [];
      for (let i = 1; i <= segs.length; i++) {
        const n = index.get(segs.slice(0, i).join("."));
        if (n) {
          const t = textOf(n.content);
          if (t) titles.push(t);
        }
      }
      if (titles.length > 0) onNodeClick(titles);
    };
    svg.addEventListener("click", onClick);
    return () => svg.removeEventListener("click", onClick);
  }, [onNodeClick]);

  return <svg ref={svgRef} className="h-full w-full [&_.markmap-node]:cursor-pointer" />;
}
