import { useEffect, useRef } from "react";

/**
 * Guard against IME (e.g. pinyin) composition so the Enter that *confirms* a
 * candidate is never mistaken for a send. WebKit/WKWebView — which Weft runs
 * inside on macOS — fires `compositionend` *before* that Enter's keydown, so by
 * the time the keydown handler runs the native `isComposing` flag already reads
 * false. We therefore keep our own flag and clear it on a deferred task, holding
 * it true across the trailing keydown; Chromium (keydown first, isComposing=true)
 * stays covered by also trusting the native flag.
 *
 * Spread `composition` onto the input/textarea and gate Enter with `isComposing(e)`.
 */
export function useImeComposition() {
  const composingRef = useRef(false);
  const clearTimer = useRef<number | undefined>(undefined);

  useEffect(() => () => window.clearTimeout(clearTimer.current), []);

  const onCompositionStart = () => {
    // A new composition starting cancels a pending reset from a back-to-back word.
    window.clearTimeout(clearTimer.current);
    composingRef.current = true;
  };
  const onCompositionEnd = () => {
    // Defer: on WebKit the confirming Enter's keydown fires *after* this event,
    // so clearing synchronously would let that Enter through as a send.
    clearTimer.current = window.setTimeout(() => {
      composingRef.current = false;
    }, 0);
  };

  const isComposing = (e: React.KeyboardEvent) =>
    composingRef.current || e.nativeEvent.isComposing;

  return {
    composition: { onCompositionStart, onCompositionEnd },
    isComposing,
  };
}
