// Build the shell command to resume a session in the user's own terminal, and
// the app deep link where one exists. weft drives native CLIs, so a session can
// always be picked back up outside weft (architecture §5.6).

function shq(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

/**
 * `cd <cwd> && <bin> resume <id>` for the given tool. `command` is the actual
 * binary to invoke (a configured alias, e.g. `cc-claude`); it falls back to the
 * tool identity so an un-aliased session is unchanged. The per-tool argument
 * shape always follows the identity.
 */
export function resumeCommand(
  tool: string,
  cwd: string,
  nativeId: string,
  command?: string,
): string {
  const bin = command?.trim() || tool;
  const at = `cd ${shq(cwd)} && `;
  switch (tool) {
    case "claude":
      return `${at}${bin} --resume ${nativeId}`;
    case "codex":
      return `${at}${bin} resume ${nativeId}`;
    case "opencode":
      return `${at}${bin} . --session ${nativeId}`;
    case "omp":
      return `${at}${bin} --resume ${nativeId}`;
    default:
      return at + bin;
  }
}

/** An app deep link to the session, where the tool offers one (Codex only). */
export function appLink(tool: string, nativeId: string): string | null {
  if (tool === "codex") return `codex://threads/${nativeId}`;
  return null;
}
