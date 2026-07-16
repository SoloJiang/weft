import { useEffect, useState } from "react";
import { clampPanelWidth } from "./panelWidth";

interface UseResizablePanelOptions {
  storageKey: string;
  min: number;
  max: number;
  default: number;
}

interface UseResizablePanelResult {
  width: number;
  setWidth: (w: number) => void;
  dragging: boolean;
  startDrag: () => void;
}

function readNum(key: string, fallback: number): number {
  const raw = localStorage.getItem(key);
  if (raw === null) return fallback;
  const n = Number(raw);
  return Number.isFinite(n) ? n : fallback;
}

export function useResizablePanel(options: UseResizablePanelOptions): UseResizablePanelResult {
  const clampW = (x: number) => clampPanelWidth(x, options.min, options.max);
  const [w, setW] = useState(() => clampW(readNum(options.storageKey, options.default)));
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    localStorage.setItem(options.storageKey, String(w));
  }, [options.storageKey, w]);

  useEffect(() => {
    const onResize = () => setW((cur) => clampW(cur));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [options.min, options.max]);

  useEffect(() => {
    if (!dragging) return;
    const move = (e: PointerEvent) => setW(clampW(window.innerWidth - e.clientX));
    const up = () => setDragging(false);
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
  }, [dragging, options.min, options.max]);

  return {
    width: w,
    setWidth: setW,
    dragging,
    startDrag: () => setDragging(true),
  };
}
