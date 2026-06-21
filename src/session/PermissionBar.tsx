import { useTranslation } from "react-i18next";
import { ShieldQuestion } from "lucide-react";
import { useStore } from "../state/store";
import type { PermissionAsk } from "../lib/types";
import { Button } from "../components/ui/Button";
import { toolFullName } from "../components/ToolIcon";
import type { ToolUIPart } from "ai";
import type { ComponentProps } from "react";
import { createContext, useContext, useMemo } from "react";
import { cn } from "../lib/cn";

// AI SDK Elements-style Confirmation primitives, Weft-skinned and inlined
// here so the component is self-contained.

type ToolUIPartApproval =
  | { id: string; approved?: never; reason?: never }
  | { id: string; approved: boolean; reason?: string }
  | { id: string; approved: true; reason?: string }
  | { id: string; approved: false; reason?: string }
  | undefined;

interface ConfirmationContextValue {
  approval: ToolUIPartApproval;
  state: ToolUIPart["state"];
}

const ConfirmationContext = createContext<ConfirmationContextValue | null>(
  null,
);

const useConfirmation = () => {
  const context = useContext(ConfirmationContext);
  if (!context) {
    throw new Error("Confirmation components must be used within Confirmation");
  }
  return context;
};

function Alert({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert"
      role="alert"
      className={cn(
        "relative grid w-full gap-1 rounded-none border border-border px-3 py-2 text-left text-[12.5px] bg-waiting/10 text-ink",
        className,
      )}
      {...props}
    />
  );
}

function AlertDescription({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-description"
      className={cn("text-[12.5px] text-ink-muted", className)}
      {...props}
    />
  );
}

type ConfirmationProps = ComponentProps<typeof Alert> & {
  approval?: ToolUIPartApproval;
  state: ToolUIPart["state"];
};

function Confirmation({
  className,
  approval,
  state,
  ...props
}: ConfirmationProps) {
  const contextValue = useMemo(() => ({ approval, state }), [approval, state]);

  if (!approval || state === "input-streaming" || state === "input-available") {
    return null;
  }

  return (
    <ConfirmationContext.Provider value={contextValue}>
      <Alert className={cn("flex flex-col gap-2", className)} {...props} />
    </ConfirmationContext.Provider>
  );
}

function ConfirmationTitle({
  className,
  ...props
}: ComponentProps<typeof AlertDescription>) {
  return <AlertDescription className={cn("inline", className)} {...props} />;
}

function ConfirmationActions({
  className,
  ...props
}: ComponentProps<"div">) {
  const { state } = useConfirmation();
  if (state !== "approval-requested") return null;
  return (
    <div
      className={cn("flex items-center justify-end gap-2 self-end", className)}
      {...props}
    />
  );
}

function ConfirmationAction(props: ComponentProps<typeof Button>) {
  return <Button type="button" {...props} />;
}

/**
 * Approvals at the conversation: when this session's agent is blocked on a
 * tool permission (Ask Bridge), answer it right here —
 * the conversation is the console, no detour through Needs-you required.
 */
export function PermissionBar({ asks }: { asks: PermissionAsk[] }) {
  const { answerPermission } = useStore();
  const { t } = useTranslation();
  if (asks.length === 0) return null;
  const ask = asks[0];
  return (
    <Confirmation
      approval={{ id: String(ask.id) }}
      state="approval-requested"
      className="flex-row flex-wrap items-center gap-2 rounded-none border-x-0 border-t-0 border-b border-waiting/40 bg-waiting/10 px-3 py-2 text-[12.5px]"
    >
      <ShieldQuestion size={14} className="shrink-0 text-waiting" />
      <ConfirmationTitle className="min-w-0 flex-1 truncate text-ink-muted">
        <span className="text-ink">{toolFullName(ask.tool)}</span> {t("needs.wantsPermission")}
        {ask.summary && <span className="ml-1.5 font-mono text-[11.5px]">{ask.summary}</span>}
      </ConfirmationTitle>
      <ConfirmationActions className="shrink-0">
        <ConfirmationAction size="sm" variant="primary" onClick={() => void answerPermission(ask.id, "allow")}>
          {t("common.allow")}
        </ConfirmationAction>
        <ConfirmationAction
          size="sm"
          variant="default"
          title={t("needs.alwaysTitle")}
          onClick={() => void answerPermission(ask.id, "always")}
        >
          {t("needs.always")}
        </ConfirmationAction>
        <ConfirmationAction
          size="sm"
          variant="default"
          title={t("needs.fullAccessTitle")}
          onClick={() => void answerPermission(ask.id, "full")}
        >
          {t("needs.fullAccess")}
        </ConfirmationAction>
        <ConfirmationAction size="sm" variant="danger" onClick={() => void answerPermission(ask.id, "deny")}>
          {t("common.deny")}
        </ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
}
