// Reserve room for the nav rail (WorkspaceNav, w-72 = 288px) PLUS a readable
// main column so a resizable right-side panel can't crowd them out. The rail
// stays visible down to the 800px auto-collapse threshold (default window is
// 1000), so the reserve must include it — otherwise opening the diff/files panel
// at the 800–1000px range squeezes the main column to a sliver. Below 800 the
// rail is gone, but the panel is already pinned to its min at those widths, so
// keeping the rail in the reserve is harmless there.
const REST_RESERVE = 288 + 360; // nav rail + min readable main

/**
 * Clamp a right-side panel width to [min, max] AND to the current window, so the
 * main column stays readable even at the 600px floor. The window cap only bites
 * near the floor; on wide screens the absolute `max` governs.
 */
export function clampPanelWidth(x: number, min: number, max: number): number {
  const cap = Math.min(max, Math.max(min, window.innerWidth - REST_RESERVE));
  return Math.max(min, Math.min(cap, x));
}
