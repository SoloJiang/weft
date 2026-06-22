import { useRef } from "react";

/** A hit-area on a right-docked panel's left border to drag-resize its width.
 *  The panel is on the right, so dragging left widens it (delta inverted). */
export function ResizeEdge({ width, onResize }: { width: number; onResize: (w: number) => void }) {
  const drag = useRef<{ x: number; w: number } | null>(null);
  return (
    <div
      onPointerDown={(e) => {
        drag.current = { x: e.clientX, w: width };
        e.currentTarget.setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (!drag.current) return;
        onResize(drag.current.w + (drag.current.x - e.clientX));
      }}
      onPointerUp={(e) => {
        drag.current = null;
        try {
          e.currentTarget.releasePointerCapture(e.pointerId);
        } catch {
          /* ignore */
        }
      }}
      className="absolute left-0 top-0 z-10 h-full w-1 cursor-col-resize hover:bg-brand/40"
      style={{ touchAction: "none" }}
    />
  );
}
