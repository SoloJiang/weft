export function isNearBottom({
  scrollTop,
  scrollHeight,
  clientHeight,
}: {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}, threshold = 80): boolean {
  return scrollHeight - scrollTop - clientHeight <= threshold;
}

export function isInitialBottomSettled({
  lastItemRendered,
  scrollTop,
  scrollHeight,
  clientHeight,
}: {
  lastItemRendered: boolean;
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}): boolean {
  if (!lastItemRendered) return false;
  return isNearBottom({ scrollTop, scrollHeight, clientHeight }, 1);
}
