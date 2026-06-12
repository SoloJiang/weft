import * as RS from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "../../lib/cn";

export interface Option {
  value: string;
  label: string;
}

export function Select({
  value,
  onValueChange,
  options,
  ariaLabel,
  placeholder,
}: {
  value: string;
  onValueChange: (v: string) => void;
  options: Option[];
  ariaLabel?: string;
  placeholder?: string;
}) {
  return (
    <RS.Root value={value} onValueChange={onValueChange}>
      <RS.Trigger
        aria-label={ariaLabel}
        className={cn(
          "inline-flex h-8 w-full items-center justify-between gap-2 rounded-[var(--radius-md)]",
          "border border-border bg-bg px-2.5 text-[13px] text-ink",
          "transition-colors hover:border-border-strong",
          "focus-visible:border-brand focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/30",
          "data-[placeholder]:text-ink-faint",
        )}
      >
        <RS.Value placeholder={placeholder} />
        <RS.Icon>
          <ChevronDown size={14} className="text-ink-faint" />
        </RS.Icon>
      </RS.Trigger>
      <RS.Portal>
        <RS.Content
          position="popper"
          sideOffset={4}
          className="weft-pop z-[60] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)]"
        >
          <RS.Viewport>
            {options.map((o) => (
              <RS.Item
                key={o.value}
                value={o.value}
                className={cn(
                  "relative flex h-7 cursor-pointer select-none items-center rounded-[var(--radius-sm)] pl-7 pr-2 text-[13px] text-ink-muted",
                  "data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink data-[highlighted]:outline-none",
                  "data-[state=checked]:text-ink",
                )}
              >
                <RS.ItemIndicator className="absolute left-2 inline-flex items-center">
                  <Check size={13} className="text-brand" />
                </RS.ItemIndicator>
                <RS.ItemText>{o.label}</RS.ItemText>
              </RS.Item>
            ))}
          </RS.Viewport>
        </RS.Content>
      </RS.Portal>
    </RS.Root>
  );
}
