import { StickToBottom } from "use-stick-to-bottom";
import { Sparkles } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export function Message({
  role,
  children,
}: {
  readonly role: "user" | "assistant";
  readonly children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "group flex w-full gap-2.5",
        role === "user" ? "flex-row-reverse" : "flex-row",
      )}
    >
      {role === "assistant" && (
        <span className="mt-0.5 grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
          <Sparkles size={14} />
        </span>
      )}
      <div
        className={cn(
          "flex min-w-0 flex-col",
          role === "user" ? "items-end" : "flex-1 items-start",
        )}
      >
        {children}
      </div>
    </div>
  );
}

export function Messages({ children }: { readonly children: ReactNode }) {
  return (
    <StickToBottom.Content className="flex flex-col pt-4 pb-4">
      {children}
    </StickToBottom.Content>
  );
}

export function Conversation({ children }: { readonly children: ReactNode }) {
  return (
    <StickToBottom
      className="relative flex min-h-0 flex-1 flex-col"
      initial="smooth"
      resize="smooth"
    >
      {children}
    </StickToBottom>
  );
}
