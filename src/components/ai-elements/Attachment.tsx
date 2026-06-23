import type { ReactNode } from "react";
import { FileText, Image as ImageIcon } from "lucide-react";
import { cn } from "../../lib/cn";

type AttachmentKind = "file" | "image";

const iconByKind: Record<AttachmentKind, ReactNode> = {
  file: <FileText size={11} className="shrink-0" />,
  image: <ImageIcon size={11} className="shrink-0" />,
};

function basename(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}

export function Attachment({
  kind,
  label,
  src,
  alt,
  className,
}: {
  readonly kind: AttachmentKind;
  readonly label: string;
  readonly src?: string;
  readonly alt?: string;
  readonly className?: string;
}) {
  if (kind === "image" && src) {
    return (
      <figure
        className={cn(
          "overflow-hidden rounded-[var(--radius-md)] border border-border bg-bg",
          className,
        )}
      >
        <img
          src={src}
          alt={alt ?? label}
          width={112}
          height={84}
          className="aspect-[4/3] max-h-32 w-28 object-cover"
        />
        <figcaption className="flex max-w-28 items-center gap-1 px-2 py-1 font-mono text-[10.5px] text-ink-faint">
          {iconByKind.image}
          <span className="truncate">{label}</span>
        </figcaption>
      </figure>
    );
  }

  return (
    <span
      className={cn(
        "inline-flex max-w-52 items-center gap-1 rounded-full border border-border bg-bg px-2 py-0.5 font-mono text-[10.5px] text-ink-muted",
        className,
      )}
      title={label}
    >
      {iconByKind.file}
      <span className="truncate">{basename(label)}</span>
    </span>
  );
}
