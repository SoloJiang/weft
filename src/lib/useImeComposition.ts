import { useRef } from "react";

/** Treat an Enter within this window after `compositionend` as still composing. */
const IME_ENTER_GRACE_MS = 100;

/**
 * Guard against IME (e.g. pinyin) composition so the Enter that *confirms* a
 * candidate is never mistaken for a send/submit. WebKit/WKWebView — which Weft
 * runs inside on macOS — can dispatch `compositionend` *before* the confirming
 * Enter keydown, so the native `isComposing` flag already reads false by then.
 * We keep our own flag and also treat an Enter within a short grace window after
 * `compositionend` as still composing — the same approach the main composer uses,
 * more robust than a zero-delay timer that can race the separate keydown. Chromium
 * (keydown first, isComposing=true) stays covered by the native flag.
 *
 * Spread `composition` onto the input/textarea and gate Enter with `isComposing(e)`.
 */
export function useImeComposition() {
  const composingRef = useRef(false);
  const lastEndRef = useRef<number | null>(null);

  const onCompositionStart = () => {
    composingRef.current = true;
    lastEndRef.current = null;
  };
  const onCompositionEnd = () => {
    composingRef.current = false;
    lastEndRef.current = performance.now();
  };

  const isComposing = (e: React.KeyboardEvent) => {
    if (e.nativeEvent.isComposing || composingRef.current) return true;
    const last = lastEndRef.current;
    return last != null && performance.now() - last < IME_ENTER_GRACE_MS;
  };

  return {
    composition: { onCompositionStart, onCompositionEnd },
    isComposing,
  };
}
