import test from "node:test";
import assert from "node:assert/strict";
import type { ParsedNode } from "stream-markdown-parser";
import { injectCaret, WEFT_CARET_TYPE } from "../../src/lib/markdownCaret.ts";

type N = {
  type: string;
  raw: string;
  loading?: boolean;
  content?: string;
  children?: N[];
};

const text = (content: string): N => ({ type: "text", raw: content, content });
const para = (raw: string, children: N[]): N => ({ type: "paragraph", raw, children });
const nodes = (...ns: N[]) => ns as unknown as ParsedNode[];
const types = (ns: N[] | undefined) => (ns ?? []).map((n) => n.type);

test("caret lands inline after the last text and tags the host node's raw", () => {
  const p = para("hello world", [text("hello world")]);
  const tree = nodes(para("first", [text("first")]), p);
  injectCaret(tree);
  assert.deepEqual(types(p.children), ["text", WEFT_CARET_TYPE]);
  // The differ compares top-level nodes by type+raw+loading only, so the
  // caret-bearing node must not compare equal to its caretless successor.
  assert.notEqual(p.raw, "hello world");
  assert.ok(p.raw.startsWith("hello world"));
  assert.equal(p.type, "paragraph");
  assert.equal(p.loading, undefined);
  // Earlier siblings stay byte-identical so the differ can still reuse them.
  assert.equal((tree[0] as unknown as N).raw, "first");
  assert.deepEqual(types((tree[0] as unknown as N).children), ["text"]);
});

test("caret descends into the deepest trailing container", () => {
  const innerPara = para("item two", [text("item two")]);
  const list: N = {
    type: "list",
    raw: "- item one\n- item two",
    children: [
      { type: "list_item", raw: "item one", children: [para("item one", [text("item one")])] },
      { type: "list_item", raw: "item two", children: [innerPara] },
    ],
  };
  const tree = nodes(list);
  injectCaret(tree);
  assert.deepEqual(types(innerPara.children), ["text", WEFT_CARET_TYPE]);
  // Only the TOP-LEVEL node's raw is tagged — that is the granularity the
  // renderer's differ operates on.
  assert.ok(list.raw.startsWith("- item one\n- item two"));
  assert.notEqual(list.raw, "- item one\n- item two");
  assert.equal(innerPara.raw, "item two");
});

test("trailing whitespace text is skipped when placing the caret", () => {
  const p = para("x", [text("x"), text("  \n")]);
  injectCaret(nodes(p));
  assert.deepEqual(types(p.children), ["text", WEFT_CARET_TYPE, "text"]);
});

test("a trailing leaf block gets the caret in its own slot after it", () => {
  const fence: N = { type: "code_block", raw: "```rust\nfn main() {", loading: true };
  const tree = nodes(para("intro", [text("intro")]), fence);
  injectCaret(tree);
  assert.deepEqual(
    tree.map((n) => (n as unknown as N).type),
    ["paragraph", "code_block", WEFT_CARET_TYPE],
  );
  // No raw tag needed: the top-level list length change is visible to the differ.
  assert.equal(fence.raw, "```rust\nfn main() {");
});

test("a trailing container with no visible content gets the caret after it", () => {
  const empty = para("", []);
  const tree = nodes(empty);
  injectCaret(tree);
  assert.deepEqual(
    tree.map((n) => (n as unknown as N).type),
    ["paragraph", WEFT_CARET_TYPE],
  );
  assert.equal(empty.raw, "");
});

test("empty tree stays empty", () => {
  const tree = nodes();
  injectCaret(tree);
  assert.equal(tree.length, 0);
});

test("injection is deterministic across identical fresh parses", () => {
  const make = () => para("stable text", [text("stable text")]);
  const a = make();
  const b = make();
  injectCaret(nodes(a));
  injectCaret(nodes(b));
  // Same input → same tagged raw, so an unchanged tail keeps node reuse (no
  // caret flicker between deltas that do not touch the last block).
  assert.equal(a.raw, b.raw);
});
