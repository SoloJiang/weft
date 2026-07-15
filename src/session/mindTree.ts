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
  // `indent` = the line's raw leading-space width, kept so a continuation line can
  // attach to the bullet it's actually nested under (the deepest one indented LESS
  // than the continuation), not merely the current stack top.
  const stack: { depth: number; node: MindTree; indent: number }[] = [
    { depth: 0, node: root, indent: -1 },
  ];
  // Nodes that came from a LEVEL-ONE (`#`) heading. Only such a node is unwrapped
  // to the document root below: a heading-less outline (sole node a bullet) or one
  // whose sole top node is a deeper heading (`## Group`) keeps the synthetic root,
  // so that node stays a branch instead of being promoted to the title.
  const h1Roots = new WeakSet<MindTree>();
  // Nodes that came from a bullet (list item). Only these accept a continuation
  // line — prose under a heading is NOT a list-item wrap, so it isn't folded into
  // the heading's title.
  const bulletNodes = new WeakSet<MindTree>();

  for (const raw of md.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    if (!line.trim()) continue;

    let depth: number;
    let topic: string;
    let rawIndent: number;
    let isHeading = false;
    const h = HEADING.exec(line);
    if (h) {
      depth = h[1].length;
      topic = h[2].trim();
      rawIndent = 0;
      isHeading = true;
    } else {
      const b = BULLET.exec(line);
      if (!b) {
        // A continuation line — a wrapped list item's tail. Attach it to the
        // bullet it's actually nested under: the DEEPEST bullet on the stack whose
        // marker is indented LESS than this line. An unindented line (or prose
        // under a heading) matches no bullet and is skipped, not folded.
        const indent = line.length - line.trimStart().length;
        for (let i = stack.length - 1; i >= 0; i--) {
          const entry = stack[i];
          if (bulletNodes.has(entry.node) && entry.indent < indent) {
            entry.node.topic = `${entry.node.topic} ${line.trim()}`.trim();
            break;
          }
        }
        continue;
      }
      rawIndent = b[1].replace(/\t/g, "  ").length;
      depth = BULLET_BASE + Math.floor(rawIndent / 2);
      topic = b[2].trim();
    }
    if (!topic) continue;

    while (stack.length > 1 && stack[stack.length - 1].depth >= depth) stack.pop();
    const node: MindTree = { topic, children: [] };
    if (isHeading && depth === 1) h1Roots.add(node);
    if (!isHeading) bulletNodes.add(node);
    stack[stack.length - 1].node.children.push(node);
    stack.push({ depth, node, indent: rawIndent });
  }

  // A single `#` (level-one) heading root is the norm — unwrap the holder only
  // then. A heading-less document, or one whose sole top node is a deeper heading
  // (`## Group`), keeps the holder so that node stays a branch (fallbackTitle
  // names the root) instead of being promoted to the title.
  const sole = root.children[0];
  if (root.topic === "" && root.children.length === 1 && h1Roots.has(sole)) {
    return sole;
  }
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
