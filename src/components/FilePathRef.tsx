import { createContext, useContext, type ReactNode } from "react";
import { FileCode2, FileText } from "lucide-react";
import { cn } from "../lib/cn";
import { openFileMenu, openFileRef } from "../lib/fileLinks";
import { displayPath } from "../lib/filePathParsing";

/**
 * True when already inside an interactive link / file-ref. Descendant file refs
 * (e.g. an inline-code label inside `[`App.tsx`](src/App.tsx)`) then render inert
 * so the OUTER element owns the click — otherwise the inner ref would hijack the
 * gesture and act on the label instead of the href.
 */
export const InsideRefContext = createContext(false);

const CODE_CHIP = "rounded bg-raised px-1 py-0.5 font-mono text-[11.5px] text-ink";

/**
 * A file path mentioned in chat, rendered quiet (looks like ordinary text or an
 * inline-code chip). The ⌘/Ctrl-held affordance — underline + pointer on hover —
 * comes from `body[data-cmd]` CSS, so plain clicks never open anything by
 * accident. ⌘/Ctrl-click opens it; right-click opens the menu.
 */
export function FilePathRef({
  token,
  cwd,
  code,
  isUrl,
  compact,
  children,
}: {
  token: string;
  cwd?: string;
  /** Render with the inline-code chip styling (for `` `path` `` spans). */
  code?: boolean;
  /** Token came from a markdown link href (URI semantics) vs a literal path. */
  isUrl?: boolean;
  /** Fit inside narrow tool rows. */
  compact?: boolean;
  children: ReactNode;
}) {
  // Nested under a link/file-ref → stay inert; the outer element owns the gesture.
  if (useContext(InsideRefContext)) {
    return <span className={cn(code && CODE_CHIP)}>{children}</span>;
  }

  const open = () => void openFileRef(token, cwd, isUrl);
  const icon = fileIcon(token);

  return (
    <span
      role="button"
      tabIndex={0}
      className={cn(
        "weft-file-ref inline-flex min-w-0 max-w-full items-center gap-1 align-baseline",
        "rounded-[var(--radius-sm)] border border-border/80 bg-raised/70 px-1.5 py-0.5",
        "font-mono text-[11.5px] leading-none text-ink outline-none transition-colors",
        "hover:border-brand/55 hover:bg-brand-ghost hover:text-ink focus-visible:border-brand focus-visible:ring-2 focus-visible:ring-brand/25",
        compact && "max-w-[24ch]",
        code && CODE_CHIP,
      )}
      title={displayPath(token, isUrl)}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        open();
      }}
      onKeyDown={(e) => {
        if (e.key !== "Enter" && e.key !== " ") return;
        e.preventDefault();
        e.stopPropagation();
        open();
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
        openFileMenu({ x: e.clientX, y: e.clientY, token, cwd, isUrl });
      }}
    >
      <span className="shrink-0 text-brand/85">{icon}</span>
      <span className="min-w-0 truncate">
        <InsideRefContext.Provider value={true}>{children}</InsideRefContext.Provider>
      </span>
    </span>
  );
}

function fileIcon(token: string) {
  return /\.(?:tsx?|jsx?|mjs|cjs|rs|py|go|rb|java|kt|swift|c|cc|cpp|h|hpp|sh|zsh|fish|sql|vue|svelte|astro)(?::\d+(?::\d+)?)?$/i.test(token)
    ? <FileCode2 size={12} />
    : <FileText size={12} />;
}
