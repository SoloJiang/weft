// Reserve room for a readable main column so a resizable right-side session
// panel can't crowd it out near the 600px window floor. At narrow widths the
// nav rail auto-collapses, so this only needs to cover the main column.
const REST_RESERVE = 440;

/**
 * Clamp a right-side panel width to [min, max] AND to the current window, so the
 * main column stays readable even at the 600px floor. The window cap only bites
 * near the floor; on wide screens the absolute `max` governs.
 */
export function clampPanelWidth(x: number, min: number, max: number): number {
  const cap = Math.min(max, Math.max(min, window.innerWidth - REST_RESERVE));
  return Math.max(min, Math.min(cap, x));
}
