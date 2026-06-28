import test from "node:test";
import assert from "node:assert/strict";
import { classifyHref, filePathsRehype } from "../../src/lib/fileLinkMarkdown.ts";
import { splitTextForPaths } from "../../src/lib/filePathParsing.ts";
import { compactToolTarget } from "../../src/session/transcriptBits.ts";

test("recognizes agent path labels with parenthesized line numbers", () => {
  assert.deepEqual(splitTextForPaths("cancel/route.ts (line 43)。"), [
    { type: "path", value: "cancel/route.ts:43", label: "cancel/route.ts:43" },
    { type: "text", value: "。" },
  ]);
});

test("recognizes bracketed dynamic route paths with line numbers", () => {
  assert.deepEqual(splitTextForPaths("见 jobs/[id]/route.ts (line 122)。"), [
    { type: "text", value: "见 " },
    { type: "path", value: "jobs/[id]/route.ts:122", label: "jobs/[id]/route.ts:122" },
    { type: "text", value: "。" },
  ]);
});

test("keeps wrappers outside path labels with line numbers", () => {
  assert.deepEqual(splitTextForPaths("[src/App.tsx (line 3)]"), [
    { type: "text", value: "[" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "]" },
  ]);
  assert.deepEqual(splitTextForPaths("【src/App.tsx (line 3)】"), [
    { type: "text", value: "【" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "】" },
  ]);
});

test("preserves leading dynamic route segments in line labels", () => {
  assert.deepEqual(splitTextForPaths("[id]/route.ts (line 122)"), [
    { type: "path", value: "[id]/route.ts:122", label: "[id]/route.ts:122" },
  ]);
});

test("recognizes route-group paths with line numbers", () => {
  assert.deepEqual(splitTextForPaths("src/app/(auth)/page.tsx (line 1)"), [
    {
      type: "path",
      value: "src/app/(auth)/page.tsx:1",
      label: "src/app/(auth)/page.tsx:1",
    },
  ]);
});

test("keeps full path token for tool summaries while showing a compact label", () => {
  assert.deepEqual(compactToolTarget("read", "Reading files src/app/layout.tsx"), {
    target: "app/layout.tsx",
    targetToken: "src/app/layout.tsx",
    added: undefined,
    removed: undefined,
  });
});

test("does not turn search patterns into file targets", () => {
  assert.deepEqual(compactToolTarget("grep", "api/v1"), {
    target: "api/v1",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("grep", "package.json"), {
    target: "package.json",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("ripgrep", "index.ts"), {
    target: "index.ts",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
});

test("does not turn command slash arguments into file targets", () => {
  assert.deepEqual(compactToolTarget("command_execution", "pnpm add @radix-ui/react-dialog"), {
    target: "pnpm add @radix-ui/react-dialog",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("command_execution", "git merge origin/main"), {
    target: "git merge origin/main",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
});

test("keeps slash-only targets for file listing tools", () => {
  assert.deepEqual(compactToolTarget("list", "src/components"), {
    target: "src/components",
    targetToken: "src/components",
    added: undefined,
    removed: undefined,
  });
});

test("recognizes Windows and route-group paths in tool summaries", () => {
  assert.deepEqual(compactToolTarget("read", String.raw`Reading files src\App.tsx`), {
    target: "src/App.tsx",
    targetToken: String.raw`src\App.tsx`,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("read", String.raw`Reading files C:\repo\src\App.tsx`), {
    target: "src/App.tsx",
    targetToken: String.raw`C:\repo\src\App.tsx`,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("file_change", "src/app/(auth)/page.tsx"), {
    target: "(auth)/page.tsx",
    targetToken: "src/app/(auth)/page.tsx",
    added: undefined,
    removed: undefined,
  });
});

test("wraps assistant prose file references for markdown rendering", () => {
  const tree = {
    type: "root",
    children: [
      {
        type: "element",
        tagName: "p",
        children: [
          {
            type: "text",
            value:
              "取消接口在 cancel/route.ts (line 43)，删除接口见 jobs/[id]/route.ts (line 122)。",
          },
        ],
      },
    ],
  };
  filePathsRehype()(tree);
  assert.deepEqual(tree.children[0].children, [
    { type: "text", value: "取消接口在 " },
    {
      type: "element",
      tagName: "span",
      properties: { dataFilepath: "cancel/route.ts:43" },
      children: [{ type: "text", value: "cancel/route.ts:43" }],
    },
    { type: "text", value: "，删除接口见 " },
    {
      type: "element",
      tagName: "span",
      properties: { dataFilepath: "jobs/[id]/route.ts:122" },
      children: [{ type: "text", value: "jobs/[id]/route.ts:122" }],
    },
    { type: "text", value: "。" },
  ]);
});

test("does not wrap paths inside markdown links or inline code in rehype prose pass", () => {
  const tree = {
    type: "root",
    children: [
      {
        type: "element",
        tagName: "p",
        children: [
          {
            type: "element",
            tagName: "a",
            properties: { href: "src/App.tsx" },
            children: [{ type: "text", value: "src/App.tsx" }],
          },
          { type: "text", value: " " },
          {
            type: "element",
            tagName: "code",
            children: [{ type: "text", value: "src/app/layout.tsx" }],
          },
        ],
      },
    ],
  };
  filePathsRehype()(tree);
  assert.equal(tree.children[0].children[0].tagName, "a");
  assert.equal(tree.children[0].children[2].tagName, "code");
});

test("classifies markdown hrefs that point at local files", () => {
  assert.deepEqual(classifyHref("src/App.tsx"), { kind: "file", token: "src/App.tsx" });
  assert.deepEqual(classifyHref("src/App.tsx:12"), { kind: "file", token: "src/App.tsx:12" });
  assert.deepEqual(classifyHref("file:///Users/me/project/src/App.tsx#L12"), {
    kind: "file",
    token: "file:///Users/me/project/src/App.tsx#L12",
  });
  assert.deepEqual(classifyHref("https://example.com/src/App.tsx"), {
    kind: "web",
    url: "https://example.com/src/App.tsx",
  });
});
