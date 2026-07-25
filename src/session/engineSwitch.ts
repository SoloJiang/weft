// Pure helpers shared by EngineSwitchDialog and ChatTimeline's
// EngineSwitchMarker (issue #96/#98) — kept JSX-free so both the dialog and
// the timeline marker read the same verdict off the same two strings, and so
// this logic is directly unit-testable (node:test can't import a .tsx file).

/** Whether `tool` accepts weft's optional `--model` override. Only claude and
 *  codex's per-turn argv builder consumes `extra_args` the way this needs
 *  (see `AgentAdapter::build_argv` in the Rust engine) — opencode's ignores
 *  it entirely, so the dialog disables the field rather than silently
 *  dropping a value the user typed for a different tool. */
export function modelSupported(tool: string): boolean {
  return tool === "claude" || tool === "codex";
}

/** "switch" (engine identity changed) vs "reload" (same tool — a deliberate,
 *  useful no-identity-change action: unstick a wedged engine, or pick up a
 *  CLI-side config/model edit without a full app restart). ONE discriminated
 *  value so the dialog's confirm-button label and the timeline marker's
 *  phrasing can never disagree about which one just happened. An empty
 *  `oldTool` (identity not resolved yet) never reads as a reload — there is
 *  nothing to "reload" until a real prior tool is known. */
export type SwitchKind = "switch" | "reload";

export function switchKindOf(oldTool: string, newTool: string): SwitchKind {
  return oldTool !== "" && oldTool === newTool ? "reload" : "switch";
}

/** The switch outcomes the backend names with a stable code instead of an
 *  English sentence (Rust: `SWITCH_MARKER_ERROR_CODE` /
 *  `SWITCH_HALF_APPLIED_ERROR_CODE` / `SWITCH_CLEANUP_ERROR_CODE`). It sends
 *  the CODE and logs the database detail, so this locale's copy comes from
 *  `src/i18n/*.ts`.
 *
 *  Only outcomes where "the switch failed" would be wrong or incomplete get
 *  one; an ordinary failed switch passes its own message through, which is why
 *  `switchErrorCodeOf` returning null is a normal result, not a gap. */
export const SWITCH_ERROR_I18N: Record<string, string> = {
  switch_marker_stamp_failed: "session.switchMarkerFailed",
  switch_half_applied: "session.switchHalfApplied",
  switch_cleanup_failed: "session.switchCleanupFailed",
};

/** The ONE discriminated value the dialog maps from — a code, or null when the
 *  rejection is an ordinary one to render verbatim. Deriving it once and
 *  looking the copy up beats a boolean matcher per outcome, which is how the
 *  set silently goes stale when a fourth is added.
 *
 *  Shape tolerance mirrors `isProcessQuotaDegradedError`: tauri commands
 *  currently reject with strings, while tests and future adapters may surface
 *  an `Error` or a `{ code }` object. */
export function switchErrorCodeOf(error: unknown): string | null {
  const codes = Object.keys(SWITCH_ERROR_I18N);
  const text = (() => {
    if (typeof error === "string") return error;
    if (error instanceof Error) return error.message;
    if (typeof error === "object" && error !== null && "code" in error) return String(error.code);
    return null;
  })();
  if (text === null) return null;
  return codes.find((code) => text.includes(code)) ?? null;
}
