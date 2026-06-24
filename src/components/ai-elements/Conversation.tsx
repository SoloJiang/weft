import { StickToBottom } from "use-stick-to-bottom";
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
      <div
        className={cn(
          // flex-1 on BOTH roles: the column needs a definite width, else its
          // nested-flex bubble + break-words text collapses to min-content (one
          // char per line) under WKWebView. items-end still right-aligns the user
          // bubble within the full-width column.
          "flex min-w-0 flex-1 flex-col",
          role === "user" ? "items-end" : "items-start",
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
