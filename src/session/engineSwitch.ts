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

/** Stable code `switch_lead_tool`/`switch_worker_tool` reject with when their
 *  durable transaction does not commit (Rust: `SWITCH_FAILED_ERROR_CODE`). The
 *  backend sends the CODE and logs the database detail, so this locale's copy
 *  comes from `src/i18n/*.ts` rather than raw SQLite text.
 *
 *  Same shape as `isProcessQuotaDegradedError`: tauri commands reject with
 *  strings today, while tests and future adapters may surface an `Error` or a
 *  `{ code }` object. Anything else falls through to the caller's generic
 *  handling, so an unrelated failure is never explained away as this one.
 *
 *  Two codes, keyed on what the user is LEFT with rather than on how the write
 *  failed: switching an idle or never-opened surface interrupts nothing, so
 *  claiming a turn was cut short would be as wrong as claiming nothing changed
 *  after a real teardown.
 *
 *  Longest match wins, since `switch_failed` is a prefix of
 *  `switch_failed_interrupted` and these are matched by substring. */
export const SWITCH_ERROR_I18N: Record<string, string> = {
  switch_failed_interrupted: "session.switchFailedInterrupted",
  switch_failed: "session.switchFailed",
};

/** The ONE discriminated value the dialog maps from — a code, or null when the
 *  rejection is an ordinary one to render verbatim. */
export function switchErrorCodeOf(error: unknown): string | null {
  const text = (() => {
    if (typeof error === "string") return error;
    if (error instanceof Error) return error.message;
    if (typeof error === "object" && error !== null && "code" in error) return String(error.code);
    return null;
  })();
  if (text === null) return null;
  return (
    Object.keys(SWITCH_ERROR_I18N)
      .sort((a, b) => b.length - a.length)
      .find((code) => text.includes(code)) ?? null
  );
}
