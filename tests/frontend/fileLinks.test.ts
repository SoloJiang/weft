import test from "node:test";
import assert from "node:assert/strict";
import { splitTextForPaths } from "../../src/lib/filePathParsing.ts";
import { compactToolTarget } from "../../src/session/transcriptBits.ts";

test("recognizes agent path labels with parenthesized line numbers", () => {
  assert.deepEqual(splitTextForPaths("cancel/route.ts (line 43)。"), [
    { type: "path", value: "cancel/route.ts:43", label: "cancel/route.ts (line 43)" },
    { type: "text", value: "。" },
  ]);
});

test("recognizes bracketed dynamic route paths with line numbers", () => {
  assert.deepEqual(splitTextForPaths("见 jobs/[id]/route.ts (line 122)。"), [
    { type: "text", value: "见 " },
    { type: "path", value: "jobs/[id]/route.ts:122", label: "jobs/[id]/route.ts (line 122)" },
    { type: "text", value: "。" },
  ]);
});

test("keeps wrappers outside path labels with line numbers", () => {
  assert.deepEqual(splitTextForPaths("[src/App.tsx (line 3)]"), [
    { type: "text", value: "[" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx (line 3)" },
    { type: "text", value: "]" },
  ]);
  assert.deepEqual(splitTextForPaths("【src/App.tsx (line 3)】"), [
    { type: "text", value: "【" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx (line 3)" },
    { type: "text", value: "】" },
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
