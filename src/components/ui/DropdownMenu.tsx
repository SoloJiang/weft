import type { ComponentProps } from "react";
import * as RDM from "@radix-ui/react-dropdown-menu";
import { cn } from "../../lib/cn";

/**
 * Thin re-skin of @radix-ui/react-dropdown-menu with the project's design
 * tokens, matching the house pattern in ./Select.tsx (weft-pop popper, raised
 * surface, brand-ghost highlight). Only the parts in use are exported.
 */
export const DropdownMenu = RDM.Root;
export const DropdownMenuTrigger = RDM.Trigger;

export function DropdownMenuContent({
  className,
  align = "end",
  sideOffset = 6,
  ...props
}: ComponentProps<typeof RDM.Content>) {
  return (
    <RDM.Portal>
      <RDM.Content
        align={align}
        sideOffset={sideOffset}
        className={cn(
          "weft-pop z-[60] min-w-[180px] overflow-hidden rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)]",
          className,
        )}
        {...props}
      />
    </RDM.Portal>
  );
}

export function DropdownMenuItem({
  className,
  ...props
}: ComponentProps<typeof RDM.Item>) {
  return (
    <RDM.Item
      className={cn(
        "relative flex cursor-pointer select-none items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12.5px] text-ink-muted outline-none transition-colors",
        "data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink",
        "data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        className,
      )}
      {...props}
    />
  );
}
