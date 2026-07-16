import type { ParsedNode } from "stream-markdown-parser";

/** Node type of the injected streaming caret; the Markdown renderer registers a
 *  component under this key to paint it. */
export const WEFT_CARET_TYPE = "weft_caret";

/** Suffix appended to the caret host's `raw`. markstream's streaming differ
 *  reuses a previous top-level node whenever type+raw+loading match — children
 *  are not inspected — so a caret-bearing node must not look identical to its
 *  caretless successor, or the final (caret-off) parse would keep the old
 *  object and the caret would never leave the DOM. */
const CARET_RAW_TAG = "<weft-caret>";

function caretNode(): ParsedNode {
  return { type: WEFT_CARET_TYPE, raw: "" } as unknown as ParsedNode;
}

type NodeWithChildren = ParsedNode & { children?: ParsedNode[]; content?: string };

/**
 * Place the caret right after the last visible content so the streaming cursor
 * hugs the text. Descends into the deepest trailing container and inserts the
 * caret inline there; a trailing leaf block (code fence, hr, image) gets the
 * caret in its own slot right after it instead. Mutation is safe: every parse
 * returns a fresh node tree.
 */
export function injectCaret(nodes: ParsedNode[]): void {
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i] as NodeWithChildren;
    if (n.type === "text" && String(n.content ?? "").trim() === "") continue;
    if (Array.isArray(n.children) && appendCaret(n.children)) {
      n.raw = `${n.raw}${CARET_RAW_TAG}`;
      return;
    }
    nodes.splice(i + 1, 0, caretNode()); // leaf block → caret in its own slot after it
    return;
  }
}

/** Child-level worker for `injectCaret`: true when the caret landed somewhere
 *  inside `nodes`. */
function appendCaret(nodes: ParsedNode[]): boolean {
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i] as NodeWithChildren;
    if (n.type === "text") {
      if (String(n.content ?? "").trim() === "") continue; // skip inter-block whitespace
      nodes.splice(i + 1, 0, caretNode());
      return true;
    }
    if (Array.isArray(n.children)) {
      if (!appendCaret(n.children)) nodes.splice(i + 1, 0, caretNode());
      return true;
    }
    nodes.splice(i + 1, 0, caretNode()); // empty/void element → caret right after it
    return true;
  }
  return false;
}
