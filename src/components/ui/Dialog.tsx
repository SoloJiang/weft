import * as RD from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export const Dialog = RD.Root;
export const DialogTrigger = RD.Trigger;

export function DialogContent({
  title,
  description,
  children,
  className,
}: {
  title: string;
  description?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <RD.Portal>
      <RD.Overlay className="weft-overlay fixed inset-0 z-50 bg-black/55 backdrop-blur-[1px]" />
      <RD.Content
        className={cn(
          "weft-pop fixed left-1/2 top-1/2 z-50 w-[min(440px,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2",
          "rounded-[var(--radius-lg)] border border-border bg-surface p-5 shadow-[0_8px_28px_-8px_rgba(0,0,0,0.6)]",
          className,
        )}
      >
        <div className="mb-4 flex items-start justify-between gap-4">
          <div className="flex flex-col gap-1">
            <RD.Title className="text-[15px] font-semibold tracking-tight text-ink">
              {title}
            </RD.Title>
            {description && (
              <RD.Description className="text-[12px] text-ink-faint">
                {description}
              </RD.Description>
            )}
          </div>
          <RD.Close
            className="-mr-1 -mt-1 grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
            aria-label="Close"
          >
            <X size={15} />
          </RD.Close>
        </div>
        {children}
      </RD.Content>
    </RD.Portal>
  );
}

/**
 * A bare, roomy dialog for hosting a whole panel (e.g. ScopeReview) over the
 * current view instead of navigating away — the content brings its own header
 * and footer, so this adds only the portal, overlay, sizing, and an sr-only
 * title for accessibility. Caps at 86vh and lets the child's own scroll region
 * handle overflow (the child chain must carry `min-h-0`).
 */
export function DialogPanel({
  title,
  children,
  className,
}: {
  title: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <RD.Portal>
      <RD.Overlay className="weft-overlay fixed inset-0 z-50 bg-black/55 backdrop-blur-[1px]" />
      <RD.Content
        aria-describedby={undefined}
        className={cn(
          "weft-pop fixed left-1/2 top-1/2 z-50 flex max-h-[86vh] w-[min(900px,calc(100vw-3rem))]",
          "-translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-[var(--radius-lg)]",
          "border border-border bg-bg shadow-[0_8px_28px_-8px_rgba(0,0,0,0.6)]",
          className,
        )}
      >
        <RD.Title className="sr-only">{title}</RD.Title>
        {children}
      </RD.Content>
    </RD.Portal>
  );
}

export const DialogClose = RD.Close;
