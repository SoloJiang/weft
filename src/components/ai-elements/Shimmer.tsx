import { cn } from "../../lib/cn";

export function Shimmer({ className }: { readonly className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-block h-3 w-20 overflow-hidden rounded-full bg-raised align-middle",
        className,
      )}
    >
      <span className="weft-shimmer block h-full w-1/2 bg-gradient-to-r from-transparent via-brand/25 to-transparent" />
    </span>
  );
}
