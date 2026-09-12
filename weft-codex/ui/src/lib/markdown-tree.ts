/**
 * The `markdown_tree` artifact format: an optional heading followed by a nested
 * bullet list. It is deliberately plain markdown — the agent writes it, the
 * human edits it, and it stays readable in a diff.
 *
 * Parsing exists so the same document can be shown as a mindmap without giving
 * up the text form. Anything the parser cannot place is preserved verbatim, so
 * round-tripping never silently drops a line.
 */

export interface TreeNode {
  id: string
  text: string
  depth: number
  children: TreeNode[]
}

export interface ParsedTree {
  heading: string
  nodes: TreeNode[]
  /** Lines before the first bullet that are not the heading, kept for round-trip. */
  preamble: string[]
}

const BULLET = /^(\s*)[-*]\s+(.*)$/

/**
 * Indentation is measured in units of the document's own smallest indent step,
 * not a hard-coded two spaces: agents emit 2- or 4-space trees and both are
 * legitimate markdown.
 */
function indentUnit(lines: string[]): number {
  let unit = 0
  for (const line of lines) {
    const match = BULLET.exec(line)
    if (!match) continue
    const width = (match[1] ?? "").replace(/\t/g, "  ").length
    if (width > 0 && (unit === 0 || width < unit)) unit = width
  }
  return unit || 2
}

export function parseMarkdownTree(source: string): ParsedTree {
  const lines = source.split("\n")
  const unit = indentUnit(lines)
  const nodes: TreeNode[] = []
  const stack: TreeNode[] = []
  let heading = ""
  const preamble: string[] = []
  let seenBullet = false
  let counter = 0

  for (const line of lines) {
    const match = BULLET.exec(line)
    if (!match) {
      if (seenBullet) continue
      const trimmed = line.trim()
      if (!trimmed) continue
      if (!heading && trimmed.startsWith("#")) heading = trimmed.replace(/^#+\s*/, "")
      else preamble.push(trimmed)
      continue
    }
    seenBullet = true
    const width = (match[1] ?? "").replace(/\t/g, "  ").length
    const depth = Math.round(width / unit)
    const node: TreeNode = {
      id: `n${counter++}`,
      text: (match[2] ?? "").trim(),
      depth,
      children: [],
    }
    while (stack.length && (stack[stack.length - 1]?.depth ?? 0) >= depth) stack.pop()
    const parent = stack[stack.length - 1]
    if (parent) parent.children.push(node)
    else nodes.push(node)
    stack.push(node)
  }

  return { heading, nodes, preamble }
}

/** Leaves are the actual test cases; branches are grouping. */
export function countLeaves(nodes: TreeNode[]): number {
  let total = 0
  for (const node of nodes) {
    if (node.children.length === 0) total += 1
    else total += countLeaves(node.children)
  }
  return total
}

export function findNode(nodes: TreeNode[], id: string): TreeNode | null {
  for (const node of nodes) {
    if (node.id === id) return node
    const hit = findNode(node.children, id)
    if (hit) return hit
  }
  return null
}

/** The path from the root down to a node, so a question can name its context. */
export function pathTo(nodes: TreeNode[], id: string, trail: string[] = []): string[] | null {
  for (const node of nodes) {
    const next = [...trail, node.text]
    if (node.id === id) return next
    const hit = pathTo(node.children, id, next)
    if (hit) return hit
  }
  return null
}

export function serializeMarkdownTree(tree: ParsedTree, indent = 2): string {
  const out: string[] = []
  if (tree.heading) out.push(`# ${tree.heading}`, "")
  for (const line of tree.preamble) out.push(line)
  if (tree.preamble.length) out.push("")
  const walk = (nodes: TreeNode[], depth: number) => {
    for (const node of nodes) {
      out.push(`${" ".repeat(depth * indent)}- ${node.text}`)
      walk(node.children, depth + 1)
    }
  }
  walk(tree.nodes, 0)
  return out.join("\n").replace(/\n+$/, "") + "\n"
}
