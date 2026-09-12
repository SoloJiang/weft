import assert from "node:assert/strict"
import test from "node:test"

import {
  countLeaves,
  parseMarkdownTree,
  pathTo,
  serializeMarkdownTree,
} from "./markdown-tree.ts"

const SAMPLE = `# 结账页优惠码测试用例

- 入口
  - 打开结账页,可看到优惠码输入入口
- 核心功能流
  - 使用有效优惠码
    - 商品合计 50 元时输入 SAVE10,应付金额变为 40 元
    - 已填写地址且金额为 40 元时,提交入口可操作
- 异常与边界
  - 输入非 SAVE10 的码,应付金额仍为 50 元
`

test("nesting follows indentation", () => {
  const tree = parseMarkdownTree(SAMPLE)
  assert.equal(tree.heading, "结账页优惠码测试用例")
  assert.equal(tree.nodes.length, 3)
  assert.equal(tree.nodes[1]?.text, "核心功能流")
  assert.equal(tree.nodes[1]?.children[0]?.text, "使用有效优惠码")
  assert.equal(tree.nodes[1]?.children[0]?.children.length, 2)
})

// Agents emit 2- and 4-space trees; both are valid markdown, so the parser
// measures the document's own step rather than assuming one.
test("a four-space tree nests the same as a two-space tree", () => {
  const four = parseMarkdownTree(`- a\n    - b\n        - c\n`)
  assert.equal(four.nodes.length, 1)
  assert.equal(four.nodes[0]?.children[0]?.text, "b")
  assert.equal(four.nodes[0]?.children[0]?.children[0]?.text, "c")
})

test("leaves are the cases, branches are grouping", () => {
  const tree = parseMarkdownTree(SAMPLE)
  // 1 under 入口 + 2 under 使用有效优惠码 + 1 under 异常与边界
  assert.equal(countLeaves(tree.nodes), 4)
})

test("a node carries the path that gives it meaning", () => {
  const tree = parseMarkdownTree(SAMPLE)
  const leaf = tree.nodes[1]?.children[0]?.children[1]
  assert.ok(leaf)
  assert.deepEqual(pathTo(tree.nodes, leaf.id), [
    "核心功能流",
    "使用有效优惠码",
    "已填写地址且金额为 40 元时,提交入口可操作",
  ])
})

test("round-tripping preserves structure and heading", () => {
  const tree = parseMarkdownTree(SAMPLE)
  const again = parseMarkdownTree(serializeMarkdownTree(tree))
  assert.equal(again.heading, tree.heading)
  assert.equal(countLeaves(again.nodes), countLeaves(tree.nodes))
  assert.deepEqual(
    again.nodes.map((n) => n.text),
    tree.nodes.map((n) => n.text),
  )
})

test("prose before the first bullet survives a round trip", () => {
  const source = "# T\n\n本文档描述结账流程。\n\n- 入口\n  - 打开页面\n"
  const tree = parseMarkdownTree(source)
  assert.deepEqual(tree.preamble, ["本文档描述结账流程。"])
  assert.match(serializeMarkdownTree(tree), /本文档描述结账流程。/)
})

test("an empty document parses to an empty tree rather than throwing", () => {
  const tree = parseMarkdownTree("")
  assert.equal(tree.nodes.length, 0)
  assert.equal(countLeaves(tree.nodes), 0)
})
