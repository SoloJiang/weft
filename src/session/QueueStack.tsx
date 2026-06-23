import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Reorder } from "motion/react";
import { GripVertical, Image as ImageIcon, Pencil, X } from "lucide-react";
import type { QueuedItem } from "../lib/types";
import { useImeComposition } from "../lib/useImeComposition";

export function QueueStack({
  items,
  onRemove,
  onEdit,
  onReorder,
}: {
  items: QueuedItem[];
  onRemove: (id: number) => void;
  onEdit: (id: number, text: string) => void;
  onReorder: (order: number[]) => void;
}) {
  const { t } = useTranslation();
  // Drag reorders the local copy for a smooth UI; the backend is told ONCE on drop
  // (not on every intermediate permutation), so out-of-order async calls can't land
  // the engine queue in the wrong order. Adopt the backend order when not dragging.
  const [order, setOrder] = useState(items);
  const draggingRef = useRef(false);
  useEffect(() => {
    if (!draggingRef.current) setOrder(items);
  }, [items]);

  if (items.length === 0) return null;
  return (
    <Reorder.Group
      axis="y"
      values={order}
      onReorder={setOrder}
      className="flex flex-col gap-1"
    >
      {order.map((it) => (
        <Reorder.Item
          key={it.id}
          value={it}
          onDragStart={() => {
            draggingRef.current = true;
          }}
          onDragEnd={() => {
            draggingRef.current = false;
            onReorder(order.map((i) => i.id));
          }}
          className="flex items-center gap-1.5 rounded-[var(--radius-md)] border border-border bg-bg px-2 py-1"
        >
          <GripVertical
            size={12}
            className="shrink-0 cursor-grab text-ink-faint"
            aria-label={t("lead.queueDrag")}
          />
          <QueueRowText item={it} onEdit={onEdit} />
          <button
            onClick={() => onRemove(it.id)}
            aria-label={t("lead.queueDelete")}
            className="shrink-0 text-ink-faint hover:text-ink"
          >
            <X size={12} />
          </button>
        </Reorder.Item>
      ))}
    </Reorder.Group>
  );
}

function QueueRowText({
  item,
  onEdit,
}: {
  item: QueuedItem;
  onEdit: (id: number, text: string) => void;
}) {
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [val, setVal] = useState(item.text);
  const { composition, isComposing } = useImeComposition();

  // An attachment-only queued send (e.g. a pasted image, no prose) has empty text;
  // show a badge so it isn't a blank, indistinguishable row.
  const hasText = item.text.trim().length > 0;
  // Inline edit is offered only for plain text rows: slash commands ({command,args}
  // shape) and attachment-bearing rows don't round-trip an edited text body into the
  // delivered transcript, so they stay delete/reorder-only.
  const editable =
    hasText &&
    item.images === 0 &&
    !item.has_attachments &&
    !item.text.trimStart().startsWith("/");

  if (!editing) {
    return (
      <>
        {hasText ? (
          <span className="min-w-0 flex-1 truncate text-[12px] text-ink">{item.text}</span>
        ) : (
          <span className="inline-flex min-w-0 flex-1 items-center gap-1 truncate text-[12px] text-ink-faint">
            <ImageIcon size={12} className="shrink-0" />
            {t("lead.queueAttachmentOnly", { count: Math.max(item.images, 1) })}
          </span>
        )}
        {editable && (
          <button
            onClick={() => {
              setVal(item.text);
              setEditing(true);
            }}
            aria-label={t("lead.queueEdit")}
            className="shrink-0 text-ink-faint hover:text-ink"
          >
            <Pencil size={11} />
          </button>
        )}
      </>
    );
  }

  const commit = () => {
    const v = val.trim();
    setEditing(false);
    if (v && v !== item.text) onEdit(item.id, v);
  };

  return (
    <input
      autoFocus
      value={val}
      onChange={(e) => setVal(e.target.value)}
      {...composition}
      onKeyDown={(e) => {
        if (e.key === "Enter" && !isComposing(e)) {
          e.preventDefault();
          commit();
        } else if (e.key === "Escape") {
          e.preventDefault();
          setEditing(false);
        }
      }}
      onBlur={commit}
      className="min-w-0 flex-1 rounded-[var(--radius-sm)] border border-border bg-surface px-1.5 py-0.5 text-[12px] text-ink outline-none"
    />
  );
}
