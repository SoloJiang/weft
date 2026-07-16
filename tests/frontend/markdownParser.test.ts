import test from "node:test";
import assert from "node:assert/strict";
import { parseMarkdownToStructure, type ParsedNode } from "stream-markdown-parser";
import { allowHref, createWeftMarkdown } from "../../src/lib/markdownParser.ts";

type AnyNode = ParsedNode & {
  content?: string;
  href?: string;
  text?: string;
  code?: string;
  raw: string;
  diff?: boolean;
  language?: string;
  children?: AnyNode[];
};

const md = createWeftMarkdown("test");
const parse = (doc: string, final = true) =>
  parseMarkdownToStructure(doc, md, { final, validateLink: allowHref }) as AnyNode[];

function collect(nodes: AnyNode[], type: string): AnyNode[] {
  const out: AnyNode[] = [];
  const walk = (ns: AnyNode[]) => {
    for (const n of ns) {
      if (n.type === type) out.push(n);
      if (Array.isArray(n.children)) walk(n.children);
    }
  };
  walk(nodes);
  return out;
}

function flatText(nodes: AnyNode[]): string {
  return collect(nodes, "text").map((n) => n.content ?? "").join("");
}

test("typographer stays off: quotes, dashes, (c), ellipsis render verbatim", () => {
  const text = flatText(parse('Set "model" to "gpt-x" -- (c) fine...'));
  assert.equal(text, 'Set "model" to "gpt-x" -- (c) fine...');
});

test("math stays off: dollar amounts are literal text", () => {
  const tree = parse("The cost is $100 and $200 total, plus $5.");
  assert.equal(collect(tree, "math_inline").length, 0);
  assert.equal(flatText(tree), "The cost is $100 and $200 total, plus $5.");
});

test("file:, app-scheme, and path:line hrefs stay links; javascript: never does", () => {
  const links = collect(
    parse("[f](file:///etc/a.toml) [v](vscode://file/a.ts) [l](src/App.tsx:42) [e](javascript:alert(1))"),
    "link",
  );
  assert.deepEqual(
    links.map((l) => l.href),
    ["file:///etc/a.toml", "vscode://file/a.ts", "src/App.tsx:42"],
  );
});

test("code span + file link + later link keeps every link (stock validateLink bug)", () => {
  const links = collect(parse("A `x.ts` [f](file:///a/b.toml) [w](https://ex.com) end"), "link");
  assert.deepEqual(links.map((l) => l.href), ["file:///a/b.toml", "https://ex.com"]);
});

test("diff fences keep the full patch in raw with the diff flag set", () => {
  const [block] = collect(parse("```diff\n- old line\n+ new line\n  context\n```"), "code_block");
  assert.equal(block.diff, true);
  // node.code holds only the updated side — the renderer must show raw instead.
  assert.equal(block.raw, "- old line\n+ new line\n  context\n");
  assert.ok(!String(block.code).includes("old line"));
});

test("linkify turns TLD-like bare filenames into schemaless http links (rerouted by WeftLink)", () => {
  // Documents the parser-level fact the link component compensates for: the
  // label equals the href minus the injected scheme, which is the reroute key.
  const links = collect(parse("Deploy server.py then visit example.com"), "link");
  assert.deepEqual(
    links.map((l) => [l.href, l.text]),
    [
      ["http://server.py", "server.py"],
      ["http://example.com", "example.com"],
    ],
  );
});

test("www and scheme autolinks still work", () => {
  const links = collect(parse("see www.example.com and https://weft.dev"), "link");
  assert.deepEqual(links.map((l) => l.href), ["http://www.example.com", "https://weft.dev"]);
});
