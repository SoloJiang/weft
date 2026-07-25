// Build a non-destructive route back into a native session. Weft drives native
// CLIs, but Codex has a session deep link while other tools resume in a terminal.

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
  const rawBin = command?.trim() || tool;
  const bin = shq(rawBin);
  const at = `cd ${shq(cwd)} && `;
  switch (tool) {
    case "claude":
      return `${at}${bin} --resume ${shq(nativeId)}`;
    case "codex":
      return `${at}${bin} resume ${shq(nativeId)}`;
    case "opencode":
      return `${at}${bin} . --session ${shq(nativeId)}`;
    case "omp":
      return `${at}${bin} --resume ${shq(nativeId)}`;
    default:
      return at + bin;
  }
}

/** An app deep link to the session, where the tool offers one (Codex only). */
export function appLink(tool: string, nativeId: string): string | null {
  if (tool === "codex") return `codex://threads/${nativeId}`;
  return null;
}

/**
 * The one safe, user-facing way to re-enter a native session.
 *
 * Codex opens its target thread through the app link. Every other coding agent
 * gets its exact terminal resume command. Neither destination stops Weft's
 * engine; lifecycle controls stay separate from session navigation.
 */
export type NativeSessionResumeTarget =
  | { kind: "copy-terminal-command"; command: string }
  | { kind: "open-codex"; url: string };

export function nativeSessionResumeTarget(
  tool: string,
  cwd: string,
  nativeId: string,
  command?: string,
): NativeSessionResumeTarget {
  const url = appLink(tool, nativeId);
  if (url) return { kind: "open-codex", url };
  return {
    kind: "copy-terminal-command",
    command: resumeCommand(tool, cwd, nativeId, command),
  };
}
