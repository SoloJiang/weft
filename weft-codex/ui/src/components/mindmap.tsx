import * as React from "react"
import { ChevronDown, ChevronRight, MessageCircleQuestion } from "lucide-react"

import { Button } from "@/components/ui/button"
import { useI18n } from "@/i18n"
import { countLeaves, parseMarkdownTree, pathTo, type TreeNode } from "@/lib/markdown-tree"
import { AsyncButton } from "./shared"

export type NodeQuestionKind = "ask" | "suggest"

export interface NodeQuestion {
  kind: NodeQuestionKind
  path: string[]
  text: string
}

function NodeRow({
  node,
  collapsed,
  onToggle,
  onAsk,
  asking,
  onStartAsk,
  onCancelAsk,
  onError,
  nodes,
}: {
  node: TreeNode
  collapsed: Set<string>
  onToggle: (id: string) => void
  onAsk: (question: NodeQuestion) => Promise<void>
  asking: string | null
  onStartAsk: (id: string) => void
  onCancelAsk: () => void
  onError: (error: unknown) => void
  nodes: TreeNode[]
}) {
  const { t } = useI18n()
  const [kind, setKind] = React.useState<NodeQuestionKind>("ask")
  const [text, setText] = React.useState("")
  const hasChildren = node.children.length > 0
  const isCollapsed = collapsed.has(node.id)
  const composerOpen = asking === node.id

  return (
    <li className="mindmap-node" data-leaf={hasChildren ? "false" : "true"}>
      <div className="mindmap-row">
        {hasChildren ? (
          <button
            type="button"
            className="mindmap-toggle"
            aria-expanded={!isCollapsed}
            aria-label={t(isCollapsed ? "mindmap.expand" : "mindmap.collapse", { text: node.text })}
            onClick={() => onToggle(node.id)}
          >
            {isCollapsed ? <ChevronRight aria-hidden="true" /> : <ChevronDown aria-hidden="true" />}
          </button>
        ) : (
          <span className="mindmap-bullet" aria-hidden="true" />
        )}
        <span className="mindmap-text">{node.text}</span>
        {/* Always in the DOM, never hover-only: a hover-revealed control is
            unreachable by keyboard and absent on touch. */}
        <button
          type="button"
          className="mindmap-ask"
          aria-label={t("mindmap.askAbout", { text: node.text })}
          aria-expanded={composerOpen}
          onClick={() => (composerOpen ? onCancelAsk() : onStartAsk(node.id))}
        >
          <MessageCircleQuestion aria-hidden="true" />
        </button>
      </div>

      {composerOpen ? (
        <div className="mindmap-composer">
          <div className="mindmap-composer-kinds" role="group" aria-label={t("mindmap.kindLabel")}>
            {(["ask", "suggest"] as const).map((option) => (
              <button
                key={option}
                type="button"
                className="mindmap-kind"
                data-active={kind === option ? "true" : "false"}
                aria-pressed={kind === option}
                onClick={() => setKind(option)}
              >
                {t(option === "ask" ? "mindmap.ask" : "mindmap.suggest")}
              </button>
            ))}
          </div>
          <label className="sr-only" htmlFor={`mindmap-composer-${node.id}`}>
            {t("mindmap.composerLabel")}
          </label>
          <textarea
            id={`mindmap-composer-${node.id}`}
            className="mindmap-composer-input"
            value={text}
            rows={2}
            placeholder={t(kind === "ask" ? "mindmap.askPlaceholder" : "mindmap.suggestPlaceholder")}
            onChange={(event) => setText(event.target.value)}
          />
          <div className="mindmap-composer-actions">
            <Button variant="ghost" onClick={onCancelAsk}>{t("mindmap.cancel")}</Button>
            <AsyncButton
              label={t("mindmap.send")}
              pendingLabel={t("mindmap.sending")}
              disabled={!text.trim()}
              onAction={async () => {
                await onAsk({ kind, path: pathTo(nodes, node.id) ?? [node.text], text: text.trim() })
                setText("")
                onCancelAsk()
              }}
              onError={onError}
            />
          </div>
        </div>
      ) : null}

      {hasChildren && !isCollapsed ? (
        <ul className="mindmap-children">
          {node.children.map((child) => (
            <NodeRow
              key={child.id}
              node={child}
              collapsed={collapsed}
              onToggle={onToggle}
              onAsk={onAsk}
              asking={asking}
              onStartAsk={onStartAsk}
              onCancelAsk={onCancelAsk}
              onError={onError}
              nodes={nodes}
            />
          ))}
        </ul>
      ) : null}
    </li>
  )
}

export function MindMap({
  content,
  onAsk,
  onError,
}: {
  content: string
  onAsk: (question: NodeQuestion) => Promise<void>
  onError: (error: unknown) => void
}) {
  const { t } = useI18n()
  const tree = React.useMemo(() => parseMarkdownTree(content), [content])
  const [collapsed, setCollapsed] = React.useState<Set<string>>(new Set())
  const [asking, setAsking] = React.useState<string | null>(null)

  const toggle = (id: string) => {
    setCollapsed((current) => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  if (!tree.nodes.length) {
    return <p className="mindmap-empty">{t("mindmap.empty")}</p>
  }

  return (
    <div className="mindmap">
      {tree.preamble.length ? (
        <p className="mindmap-preamble">{tree.preamble.join(" ")}</p>
      ) : null}
      <ul className="mindmap-root">
        {tree.nodes.map((node) => (
          <NodeRow
            key={node.id}
            node={node}
            collapsed={collapsed}
            onToggle={toggle}
            onAsk={onAsk}
            asking={asking}
            onStartAsk={setAsking}
            onCancelAsk={() => setAsking(null)}
            onError={onError}
            nodes={tree.nodes}
          />
        ))}
      </ul>
    </div>
  )
}

export function caseCount(content: string): number {
  return countLeaves(parseMarkdownTree(content).nodes)
}
