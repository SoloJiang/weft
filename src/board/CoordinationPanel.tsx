import { useMemo, useState } from "react";
import { motion, AnimatePresence } from "motion/react";
import { Megaphone, Radio, Send } from "lucide-react";
import { useStore } from "../state/store";
import type { Direction } from "../lib/types";
import { Button } from "../components/ui/Button";
import { Input } from "../components/ui/Input";
import { Select } from "../components/ui/Select";
import { cn } from "../lib/cn";

/** A right-rail panel: the thread's bus timeline + a human composer. */
export function CoordinationPanel({ directions }: { directions: Direction[] }) {
  const { messages, postHuman } = useStore();
  const [to, setTo] = useState<string>("*");
  const [text, setText] = useState("");

  const nameOf = useMemo(() => {
    const m: Record<string, string> = { you: "you", "*": "all" };
    for (const d of directions) m[String(d.id)] = d.name;
    return (key: string) => m[key] ?? key;
  }, [directions]);

  const options = useMemo(
    () => [
      { value: "*", label: "Broadcast · all directions" },
      ...directions.map((d) => ({ value: String(d.id), label: d.name })),
    ],
    [directions],
  );

  async function send() {
    if (!text.trim()) return;
    await postHuman(to === "*" ? null : to, text);
    setText("");
  }

  return (
    <aside className="flex w-80 shrink-0 flex-col border-l border-border bg-surface">
      <header className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <Radio size={13} className="text-brand" />
        <span className="text-[12px] font-semibold text-ink">Thread bus</span>
        <span className="ml-auto text-[11px] text-ink-faint">
          {messages.length} {messages.length === 1 ? "message" : "messages"}
        </span>
      </header>

      <div className="flex min-h-0 flex-1 flex-col-reverse overflow-y-auto px-3 py-2">
        {/* col-reverse keeps the newest pinned to the bottom */}
        <div className="flex flex-col gap-1.5">
          <AnimatePresence initial={false}>
            {messages.map((m, i) => (
              <motion.div
                key={`${m.ts}-${i}`}
                initial={{ opacity: 0, y: 4 }}
                animate={{ opacity: 1, y: 0 }}
                transition={{ duration: 0.16 }}
                className={cn(
                  "rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-1.5",
                  m.kind === "interface" && "border-approval/40",
                )}
              >
                <div className="flex items-center gap-1.5 text-[10px] text-ink-faint">
                  {m.kind === "interface" && (
                    <Megaphone size={10} className="text-approval" />
                  )}
                  <span className="font-medium text-ink-muted">{nameOf(m.from)}</span>
                  <span>→</span>
                  <span>{nameOf(m.to)}</span>
                </div>
                <p className="mt-0.5 whitespace-pre-wrap break-words text-[12px] text-ink">
                  {m.text}
                </p>
              </motion.div>
            ))}
          </AnimatePresence>
          {messages.length === 0 && (
            <p className="px-1 py-6 text-center text-[11px] leading-relaxed text-ink-faint">
              No messages yet. Directions post here via the bus; you can too.
            </p>
          )}
        </div>
      </div>

      <form
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
        className="flex flex-col gap-2 border-t border-border p-3"
      >
        <Select value={to} onValueChange={setTo} ariaLabel="Recipient" options={options} />
        <div className="flex gap-2">
          <Input
            placeholder="Message the thread…"
            value={text}
            onChange={(e) => setText(e.currentTarget.value)}
          />
          <Button type="submit" variant="primary" size="icon" disabled={!text.trim()}>
            <Send size={14} />
          </Button>
        </div>
      </form>
    </aside>
  );
}
