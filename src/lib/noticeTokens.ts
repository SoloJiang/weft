/** Stable machine tokens a bus notice may carry in place of prose.
 *
 *  Some notices are raised from detached backend tasks — a watchdog sweep, a
 *  force-reset timer — which have no locale to render with: `lang` reaches the
 *  backend per command invocation rather than being persisted. Those paths emit
 *  a token and the copy lives here, in the catalogs the webview already owns.
 *  Same shape `ConfirmationCard` uses for `acp.permission_required`.
 *
 *  Anything not listed is real prose from an agent and renders verbatim.
 *
 *  Each key is a CONTRACT with the Rust constant that emits it; the backend
 *  pins its own side (see `engine::ACP_FORCE_RESET_NOTICE`). If the two drift,
 *  the raw token shows up as the notice body.
 */
export const NOTICE_TOKENS: Record<string, string> = {
  "acp.force_reset_notice": "needs.acpForceResetNotice",
};

/** The i18n key for a notice body, or null when it is prose to show as-is. */
export function noticeTokenKey(text: string): string | null {
  return NOTICE_TOKENS[text] ?? null;
}
