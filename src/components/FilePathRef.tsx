import type { ReactNode } from "react";
import { cn } from "../lib/cn";
import { openFileMenu, openFileRef } from "../lib/fileLinks";

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
  children,
}: {
  token: string;
  cwd?: string;
  /** Render with the inline-code chip styling (for `` `path` `` spans). */
  code?: boolean;
  /** Token came from a markdown link href (URI semantics) vs a literal path. */
  isUrl?: boolean;
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "weft-file-ref",
        code && "rounded bg-raised px-1 py-0.5 font-mono text-[11.5px] text-ink",
      )}
      title={token}
      onClick={(e) => {
        if (!(e.metaKey || e.ctrlKey)) return;
        e.preventDefault();
        e.stopPropagation();
        void openFileRef(token, cwd, isUrl);
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
        openFileMenu({ x: e.clientX, y: e.clientY, token, cwd, isUrl });
      }}
    >
      {children}
    </span>
  );
}
