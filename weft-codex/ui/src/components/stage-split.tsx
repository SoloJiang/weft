import * as React from "react"

import { api, jsonRequest } from "@/api"
import { useI18n } from "@/i18n"
import type { UiState } from "@/types"

/**
 * Weft's own two-pane stage: board on the left, issue detail on the right.
 *
 * The detail does not live in Codex's side panel. That panel is a tab
 * container whose selection lives in the host's store, so an injected tab can
 * never become selected and the only way to show anything there would be to
 * cover the host's own panel — see
 * `docs/specs/2026-08-16-host-container.md` §5. Weft therefore splits the one
 * slot it does own.
 *
 * Treatment is measured off the host's own resizable panel (§5.1.1): the
 * boundary is a directional shadow with no hairline, and the drag handle is a
 * 16px hit area straddling that edge.
 */

/** Weft's choices, not host measurements — see §5.1.1. */
const DEFAULT_WIDTH = 420
const MIN_WIDTH = 320
const MAX_FRACTION = 0.6

/**
 * Below this the panes stop sharing and the detail takes the whole stage.
 *
 * The host drops its secondary pane when the window gets narrow, but that is a
 * different situation: its panel is something you left open, while the detail
 * here is what the user just clicked a card to read. Taking over is the
 * degradation that keeps the click meaningful, and it is also what the
 * standalone preview gets for free.
 */
const SPLIT_FLOOR = 720

function clampWidth(width: number, stageWidth: number): number {
  const max = Math.max(MIN_WIDTH, Math.round(stageWidth * MAX_FRACTION))
  return Math.min(max, Math.max(MIN_WIDTH, Math.round(width)))
}

export function StageSplit({
  board,
  detail,
}: {
  board: React.ReactNode
  /** Absent means the board owns the whole stage. */
  detail: React.ReactNode | null
}) {
  const { t } = useI18n()
  const rootRef = React.useRef<HTMLDivElement>(null)
  const [width, setWidth] = React.useState(DEFAULT_WIDTH)
  const [stageWidth, setStageWidth] = React.useState(0)

  // Persisted server-side rather than in the browser: this UI runs inside a
  // shadow root in Codex's document and keeps no client storage of its own.
  React.useEffect(() => {
    let active = true
    api<UiState>("/api/ui-state")
      .then((state) => {
        if (active && state.detailPaneWidth) setWidth(state.detailPaneWidth)
      })
      .catch(() => {
        // A width is a preference, not state worth reporting a failure over.
      })
    return () => { active = false }
  }, [])

  const rememberWidth = React.useCallback((next: number) => {
    void api("/api/ui-state", jsonRequest("POST", { detailPaneWidth: next })).catch(() => {})
  }, [])

  React.useEffect(() => {
    const root = rootRef.current
    if (!root) return
    const observer = new ResizeObserver(([entry]) => {
      if (entry) setStageWidth(Math.round(entry.contentRect.width))
    })
    observer.observe(root)
    return () => observer.disconnect()
  }, [])

  // Wide enough to share, and only once measured — before the first
  // observation `stageWidth` is 0, and starting split-open would flash the
  // board at a wrong width on every open.
  const shared = Boolean(detail) && stageWidth >= SPLIT_FLOOR

  const onDragStart = React.useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    const root = rootRef.current
    if (!root) return
    event.preventDefault()
    const handle = event.currentTarget
    handle.setPointerCapture(event.pointerId)
    const rootRight = root.getBoundingClientRect().right
    const move = (moveEvent: PointerEvent) => {
      setWidth(clampWidth(rootRight - moveEvent.clientX, root.getBoundingClientRect().width))
    }
    const stop = () => {
      handle.releasePointerCapture(event.pointerId)
      handle.removeEventListener("pointermove", move)
      handle.removeEventListener("pointerup", stop)
      handle.removeEventListener("pointercancel", stop)
      // Written once on release, not on every frame of the drag.
      setWidth((current) => { rememberWidth(current); return current })
    }
    handle.addEventListener("pointermove", move)
    handle.addEventListener("pointerup", stop)
    handle.addEventListener("pointercancel", stop)
  }, [rememberWidth])

  const detailWidth = shared ? clampWidth(width, stageWidth) : 0

  return (
    <div
      ref={rootRef}
      className="stage-split"
      data-detail={detail ? (shared ? "shared" : "full") : "closed"}
      style={shared ? { ["--stage-detail-w" as string]: `${detailWidth}px` } : undefined}
    >
      <div className="stage-board">{board}</div>
      {detail ? (
        <>
          {shared ? (
            <div
              className="stage-divider"
              role="separator"
              aria-orientation="vertical"
              aria-label={t("stage.resizeDetail")}
              onPointerDown={onDragStart}
            />
          ) : null}
          <aside className="stage-detail">{detail}</aside>
        </>
      ) : null}
    </div>
  )
}
