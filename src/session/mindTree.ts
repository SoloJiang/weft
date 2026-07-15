/**
 * The test-case document as a plain node tree — a topic plus ordered children.
 * This is the shared shape the editable mindmap round-trips through: markdown in
 * (`parseTestPlanMarkdown`), structural edits on the tree inside mind-elixir,
 * canonical markdown back out (`mindTreeToMarkdown`). Kept dependency-free (no
 * mind-elixir, no i18n) so the round-trip is easy to reason about and the same
 * markmap preview renders the result identically before and after an edit.
 */
export interface MindTree {
  topic: string;
  children: MindTree[];
}

const HEADING = /^(#{1,6})\s+(.*)$/;
const BULLET = /^(\s*)[-*+]\s+(.*)$/;
// Bullets always sit below headings in the outline: offset every bullet past
// the deepest possible heading (6) so a top-level bullet nests under the
// nearest heading chain rather than becoming its sibling.
const BULLET_BASE = 7;

/**
 * Parse a test-case markdown document (headings + nested bullet lists) into a
 * single-root tree. A line's depth comes from its heading level (`#` → 1) or, for
 * a bullet, from `BULLET_BASE` plus its indent (two spaces per level); each line
 * attaches under the nearest earlier line with a smaller depth. Blank lines and
 * non-list prose are skipped — the document is treated as an outline.
 *
 * The lead emits one `#` title, so the common result is that single root. When a
 * document instead starts with bullets or carries several top-level headings, a
 * synthetic empty-topic root holds them, so the return is always exactly one
 * tree; callers give that root a real title before it is shown or serialized.
 */
export function parseTestPlanMarkdown(md: string): MindTree {
  const root: MindTree = { topic: "", children: [] };
  const stack: { depth: number; node: MindTree }[] = [{ depth: 0, node: root }];

  for (const raw of md.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    if (!line.trim()) continue;

    let depth: number;
    let topic: string;
    const h = HEADING.exec(line);
    if (h) {
      depth = h[1].length;
      topic = h[2].trim();
    } else {
      const b = BULLET.exec(line);
      if (!b) continue;
      const indent = b[1].replace(/\t/g, "  ").length;
      depth = BULLET_BASE + Math.floor(indent / 2);
      topic = b[2].trim();
    }
    if (!topic) continue;

    while (stack.length > 1 && stack[stack.length - 1].depth >= depth) stack.pop();
    const node: MindTree = { topic, children: [] };
    stack[stack.length - 1].node.children.push(node);
    stack.push({ depth, node });
  }

  // A single `#` root is the norm — unwrap the synthetic holder in that case.
  if (root.topic === "" && root.children.length === 1) return root.children[0];
  return root;
}

/**
 * Serialize a tree back to canonical test-case markdown: the root as an `#`
 * heading and every descendant as a nested bullet (two spaces per level). markmap
 * and the editor both re-parse this to the same tree, so the mindmap render is
 * stable across an edit even though deep heading levels collapse to bullets. Node
 * topics are flattened to a single line (newlines → spaces) so one node stays one
 * bullet. `fallbackTitle` names the root only when it has no topic of its own.
 */
export function mindTreeToMarkdown(root: MindTree, fallbackTitle: string): string {
  const flat = (s: string) => s.replace(/\r?\n/g, " ").trim();
  const lines: string[] = [`# ${flat(root.topic) || fallbackTitle}`];

  const walk = (nodes: MindTree[], depth: number) => {
    for (const n of nodes) {
      lines.push(`${"  ".repeat(depth)}- ${flat(n.topic)}`);
      if (n.children.length) walk(n.children, depth + 1);
    }
  };
  walk(root.children, 0);

  return lines.join("\n") + "\n";
}
