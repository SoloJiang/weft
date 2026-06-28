import test from "node:test";
import assert from "node:assert/strict";
import { classifyHref, filePathsRehype } from "../../src/lib/fileLinkMarkdown.ts";
import { hasLineLabelSyntax, isPathLike, splitTextForPaths } from "../../src/lib/filePathParsing.ts";
import {
  compactToolTarget,
  toolAllowsFileTarget,
  toolDoneLabelKey,
} from "../../src/session/transcriptBits.ts";

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
  assert.deepEqual(splitTextForPaths("[src/App.tsx] (line 3)"), [
    { type: "text", value: "[" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "]" },
  ]);
  assert.deepEqual(splitTextForPaths("[[src/App.tsx]] (line 3)"), [
    { type: "text", value: "[[" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "]]" },
  ]);
  assert.deepEqual(splitTextForPaths("((src/App.tsx)) (line 3)"), [
    { type: "text", value: "((" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "))" },
  ]);
  assert.deepEqual(splitTextForPaths("【src/App.tsx (line 3)】"), [
    { type: "text", value: "【" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "】" },
  ]);
  assert.deepEqual(splitTextForPaths("【src/App.tsx】 (line 3)"), [
    { type: "text", value: "【" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "】" },
  ]);
  assert.deepEqual(splitTextForPaths("[[src/App.tsx (line 3)]]"), [
    { type: "text", value: "[[" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "]]" },
  ]);
  assert.deepEqual(splitTextForPaths("((src/App.tsx (line 3)))"), [
    { type: "text", value: "((" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "))" },
  ]);
  assert.deepEqual(splitTextForPaths('"src/App.tsx" (line 3)'), [
    { type: "text", value: '"' },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: '"' },
  ]);
  assert.deepEqual(splitTextForPaths("`src/App.tsx` (line 3)"), [
    { type: "text", value: "`" },
    { type: "path", value: "src/App.tsx:3", label: "src/App.tsx:3" },
    { type: "text", value: "`" },
  ]);
  assert.deepEqual(splitTextForPaths("（crates/foo/src/lib.rs (line 3)）"), [
    { type: "text", value: "（" },
    { type: "path", value: "crates/foo/src/lib.rs:3", label: "crates/foo/src/lib.rs:3" },
    { type: "text", value: "）" },
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

test("recognizes dotless manifest paths with line numbers", () => {
  assert.deepEqual(splitTextForPaths("Dockerfile (line 12)"), [
    { type: "path", value: "Dockerfile:12", label: "Dockerfile:12" },
  ]);
  assert.deepEqual(splitTextForPaths("Makefile (line 4)"), [
    { type: "path", value: "Makefile:4", label: "Makefile:4" },
  ]);
  assert.deepEqual(splitTextForPaths(".gitignore (line 2)"), [
    { type: "path", value: ".gitignore:2", label: ".gitignore:2" },
  ]);
});

test("recognizes extensionless relative paths with line numbers", () => {
  assert.deepEqual(splitTextForPaths("scripts/pre-commit (line 1)"), [
    { type: "path", value: "scripts/pre-commit:1", label: "scripts/pre-commit:1" },
  ]);
  assert.deepEqual(splitTextForPaths("src/bin/tool (line 2)"), [
    { type: "path", value: "src/bin/tool:2", label: "src/bin/tool:2" },
  ]);
  assert.deepEqual(splitTextForPaths("src/A.ts (line 1),scripts/pre-commit (line 2)"), [
    { type: "path", value: "src/A.ts:1", label: "src/A.ts:1" },
    { type: "text", value: "," },
    { type: "path", value: "scripts/pre-commit:2", label: "scripts/pre-commit:2" },
  ]);
});

test("does not suffix-match spaced line-label paths", () => {
  assert.deepEqual(splitTextForPaths("/Users/me/My Repo/src/App.tsx (line 1)"), [
    { type: "text", value: "/Users/me/My Repo/src/App.tsx (line 1)" },
  ]);
});

test("requires a boundary before line-label paths", () => {
  assert.deepEqual(splitTextForPaths("见jobs/[id]/route.ts (line 122)"), [
    { type: "text", value: "见" },
    { type: "path", value: "jobs/[id]/route.ts:122", label: "jobs/[id]/route.ts:122" },
  ]);
  assert.deepEqual(splitTextForPaths("insrc/App.tsx (line 3)"), [
    { type: "path", value: "insrc/App.tsx:3", label: "insrc/App.tsx:3" },
  ]);
});

test("keeps earlier file chips before rejected line-label paths", () => {
  assert.deepEqual(splitTextForPaths("src/A.ts and /Users/me/My Repo/src/App.tsx (line 1)"), [
    { type: "path", value: "src/A.ts" },
    { type: "text", value: " and /Users/me/My Repo/src/App.tsx (line 1)" },
  ]);
});

test("preserves text trimmed from embedded line-label paths", () => {
  assert.deepEqual(splitTextForPaths("src/A.ts (line 1),src/B.ts (line 2)"), [
    { type: "path", value: "src/A.ts:1", label: "src/A.ts:1" },
    { type: "text", value: "," },
    { type: "path", value: "src/B.ts:2", label: "src/B.ts:2" },
  ]);
  assert.deepEqual(splitTextForPaths("src/A.ts (line 1),foo/bar.ts (line 2)"), [
    { type: "path", value: "src/A.ts:1", label: "src/A.ts:1" },
    { type: "text", value: "," },
    { type: "path", value: "foo/bar.ts:2", label: "foo/bar.ts:2" },
  ]);
  assert.deepEqual(splitTextForPaths("src/A.ts,src/B.ts (line 2)"), [
    { type: "path", value: "src/A.ts" },
    { type: "text", value: "," },
    { type: "path", value: "src/B.ts:2", label: "src/B.ts:2" },
  ]);
  assert.deepEqual(splitTextForPaths("src/A.ts、src/B.ts (line 2)"), [
    { type: "path", value: "src/A.ts" },
    { type: "text", value: "、" },
    { type: "path", value: "src/B.ts:2", label: "src/B.ts:2" },
  ]);
});

test("rejects unrescued prose prefixes before anchored line-label paths", () => {
  assert.deepEqual(splitTextForPaths("见./src/App.tsx (line 3)"), [
    { type: "text", value: "见./src/App.tsx (line 3)" },
  ]);
  assert.deepEqual(splitTextForPaths("见~/repo/src/App.tsx (line 3)"), [
    { type: "text", value: "见~/repo/src/App.tsx (line 3)" },
  ]);
});

test("preserves valid parent directories before line-label paths", () => {
  assert.deepEqual(splitTextForPaths("crates/foo/src/lib.rs (line 3)"), [
    {
      type: "path",
      value: "crates/foo/src/lib.rs:3",
      label: "crates/foo/src/lib.rs:3",
    },
  ]);
  assert.deepEqual(splitTextForPaths("frontend/src/App.tsx (line 3)"), [
    {
      type: "path",
      value: "frontend/src/App.tsx:3",
      label: "frontend/src/App.tsx:3",
    },
  ]);
  assert.deepEqual(splitTextForPaths("my-app/src/App.tsx (line 1)"), [
    {
      type: "path",
      value: "my-app/src/App.tsx:1",
      label: "my-app/src/App.tsx:1",
    },
  ]);
  assert.deepEqual(splitTextForPaths("foo-components/Button.tsx (line 4)"), [
    {
      type: "path",
      value: "foo-components/Button.tsx:4",
      label: "foo-components/Button.tsx:4",
    },
  ]);
  assert.deepEqual(splitTextForPaths("my.app/src/App.tsx (line 1)"), [
    {
      type: "path",
      value: "my.app/src/App.tsx:1",
      label: "my.app/src/App.tsx:1",
    },
  ]);
  assert.deepEqual(splitTextForPaths("foo.src/App.tsx (line 3)"), [
    {
      type: "path",
      value: "foo.src/App.tsx:3",
      label: "foo.src/App.tsx:3",
    },
  ]);
});

test("separates prose before colon-prefixed line labels", () => {
  assert.deepEqual(splitTextForPaths("见：crates/foo/src/lib.rs (line 3)"), [
    { type: "text", value: "见：" },
    {
      type: "path",
      value: "crates/foo/src/lib.rs:3",
      label: "crates/foo/src/lib.rs:3",
    },
  ]);
  assert.deepEqual(splitTextForPaths("see:crates/foo/src/lib.rs (line 3)"), [
    { type: "text", value: "see:" },
    {
      type: "path",
      value: "crates/foo/src/lib.rs:3",
      label: "crates/foo/src/lib.rs:3",
    },
  ]);
  assert.deepEqual(splitTextForPaths("see:scripts/pre-commit (line 1)"), [
    { type: "text", value: "see:" },
    {
      type: "path",
      value: "scripts/pre-commit:1",
      label: "scripts/pre-commit:1",
    },
  ]);
  assert.deepEqual(splitTextForPaths("见：foo/bar (line 1)"), [
    { type: "text", value: "见：" },
    {
      type: "path",
      value: "foo/bar:1",
      label: "foo/bar:1",
    },
  ]);
  assert.deepEqual(splitTextForPaths("see:/tmp/foo.ts (line 1)"), [
    { type: "text", value: "see:" },
    {
      type: "path",
      value: "/tmp/foo.ts:1",
      label: "/tmp/foo.ts:1",
    },
  ]);
  assert.deepEqual(splitTextForPaths("routes/users/:id.tsx (line 5)"), [
    {
      type: "path",
      value: "routes/users/:id.tsx:5",
      label: "routes/users/:id.tsx:5",
    },
  ]);
  assert.deepEqual(splitTextForPaths("src/foo:bar.ts (line 1)"), [
    {
      type: "path",
      value: "src/foo:bar.ts:1",
      label: "src/foo:bar.ts:1",
    },
  ]);
});

test("preserves Windows drive prefixes in line labels", () => {
  assert.deepEqual(splitTextForPaths(String.raw`C:\repo\src\App.tsx (line 3)`), [
    {
      type: "path",
      value: String.raw`C:\repo\src\App.tsx:3`,
      label: String.raw`C:\repo\src\App.tsx:3`,
    },
  ]);
});

test("detects rejected inline-code line labels before path fallback", () => {
  const spacedLineLabel = "/Users/me/My Repo/src/App.tsx (line 1)";
  const punctuatedSpacedLineLabel = "/Users/me/My Repo/src/App.tsx (line 1).";
  assert.equal(hasLineLabelSyntax(spacedLineLabel), true);
  assert.equal(hasLineLabelSyntax(punctuatedSpacedLineLabel), true);
  assert.equal(isPathLike(spacedLineLabel, true), true);
  assert.equal(isPathLike(punctuatedSpacedLineLabel, true), true);
  assert.deepEqual(splitTextForPaths(spacedLineLabel), [
    { type: "text", value: spacedLineLabel },
  ]);
  const punctuatedSegments = splitTextForPaths(punctuatedSpacedLineLabel);
  assert.equal(punctuatedSegments.every((seg) => seg.type === "text"), true);
  assert.equal(punctuatedSegments.map((seg) => seg.value).join(""), punctuatedSpacedLineLabel);
  assert.deepEqual(splitTextForPaths("https://example.com/src/App.tsx (line 3)"), [
    { type: "text", value: "https://example.com/src/App.tsx (line 3)" },
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

test("keeps multi-dot root tool file targets", () => {
  assert.deepEqual(compactToolTarget("read", "vite.config.ts"), {
    target: "vite.config.ts",
    targetToken: "vite.config.ts",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("file_change", ".eslintrc.js"), {
    target: ".eslintrc.js",
    targetToken: ".eslintrc.js",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("edit", "foo.test.ts"), {
    target: "foo.test.ts",
    targetToken: "foo.test.ts",
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
  assert.equal(toolDoneLabelKey("commandExecution"), "session.toolRan");
  assert.deepEqual(compactToolTarget("command_execution", "pnpm add @radix-ui/react-dialog"), {
    target: "pnpm add @radix-ui/react-dialog",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("commandExecution", "node scripts/foo.js"), {
    target: "node scripts/foo.js",
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
  assert.deepEqual(compactToolTarget("command_execution", "git checkout package.json"), {
    target: "git checkout package.json",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("exec_command", "node scripts/foo.js"), {
    target: "node scripts/foo.js",
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
  assert.deepEqual(compactToolTarget("mcp__weft_curator__set_classification", "services/api"), {
    target: "services/api",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
});

test("does not turn URL summaries into local file targets", () => {
  assert.deepEqual(compactToolTarget("read_url", "https://example.com/api.json"), {
    target: "https://example.com/api.json",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
});

test("keeps extensionless targets for known file-change tools", () => {
  assert.deepEqual(compactToolTarget("file_change", "src/bin/tool"), {
    target: "bin/tool",
    targetToken: "src/bin/tool",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("edit", "scripts/pre-commit"), {
    target: "scripts/pre-commit",
    targetToken: "scripts/pre-commit",
    added: undefined,
    removed: undefined,
  });
});

test("does not classify arbitrary mcp tools by substring-only path verbs", () => {
  assert.equal(toolDoneLabelKey("mcp__weft_curator__set_classification"), "session.toolCalled");
  assert.equal(toolAllowsFileTarget("mcp__weft_curator__set_classification"), false);
  assert.deepEqual(compactToolTarget("mcp__weft_curator__set_classification", "package.json"), {
    target: "package.json",
    targetToken: undefined,
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
  assert.deepEqual(compactToolTarget("read", "~/repo/src/App.tsx"), {
    target: "src/App.tsx",
    targetToken: "~/repo/src/App.tsx",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("read", "src/组件/Button.tsx"), {
    target: "组件/Button.tsx",
    targetToken: "src/组件/Button.tsx",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("read", "src/中文.ts"), {
    target: "src/中文.ts",
    targetToken: "src/中文.ts",
    added: undefined,
    removed: undefined,
  });
});

test("recognizes shared path extensions in edit tool summaries", () => {
  assert.deepEqual(compactToolTarget("file_change", "src/main.py"), {
    target: "src/main.py",
    targetToken: "src/main.py",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("edit", "cmd/server.go"), {
    target: "cmd/server.go",
    targetToken: "cmd/server.go",
    added: undefined,
    removed: undefined,
  });
});

test("recognizes manifest paths in edit tool summaries", () => {
  assert.deepEqual(compactToolTarget("file_change", "src/Dockerfile"), {
    target: "src/Dockerfile",
    targetToken: "src/Dockerfile",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("edit", "src/.gitignore"), {
    target: "src/.gitignore",
    targetToken: "src/.gitignore",
    added: undefined,
    removed: undefined,
  });
});

test("does not suffix-match spaced paths in tool summaries", () => {
  assert.deepEqual(compactToolTarget("file_change", "src/My File.tsx"), {
    target: "src/My File.tsx",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("read", "/Users/me/My Repo/src/App.tsx"), {
    target: "/Users/me/My Repo/src/App.tsx",
    targetToken: undefined,
    added: undefined,
    removed: undefined,
  });
});

test("peels balanced parentheses around tool paths", () => {
  assert.deepEqual(compactToolTarget("read", "Reading (src/App.tsx)"), {
    target: "src/App.tsx",
    targetToken: "src/App.tsx",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("file_change", "(src/app/(auth)/page.tsx)"), {
    target: "(auth)/page.tsx",
    targetToken: "src/app/(auth)/page.tsx",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("edit", "(scripts/pre-commit)"), {
    target: "scripts/pre-commit",
    targetToken: "scripts/pre-commit",
    added: undefined,
    removed: undefined,
  });
  assert.deepEqual(compactToolTarget("read", "(src/bin/tool)"), {
    target: "bin/tool",
    targetToken: "src/bin/tool",
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
