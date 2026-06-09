import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { api } from "../lib/api";
import { resumeCommand } from "../lib/resume";

/**
 * The issue console — a real chat, not a projection of the CLI's log. Messages
 * live in weft's own store, replies stream token-by-token over the lead-chat
 * event, and structured cards sit inline in the timeline. The engine survives
 * restarts (resume) so history is always here and the composer always works.
 */
export function LeadTab({ onReview }: { onReview: () => void }) {
  const {
    activeThreadId,
    leadMessages,
    leadTurn,
    leadSlash,
    loadLeadChat,
    sendLeadChat,
    interruptLead,
    setReviewingProposal,
  } = useStore();
  const { t } = useTranslation();

  useEffect(() => {
    if (activeThreadId != null) void loadLeadChat(activeThreadId);
  }, [activeThreadId, loadLeadChat]);

  if (activeThreadId == null) return null;
  // The lead's own timeline: worker chat rows carry a session_id, skip them.
  const msgs = (leadMessages[activeThreadId] ?? []).filter((m) => m.session_id == null);
  const turn = leadTurn[activeThreadId] ?? { state: "stopped" as const, queued: 0 };

  return (
    <div className="flex min-h-0 flex-1 flex-col bg-bg">
      <ChatTimeline
        messages={msgs}
        busy={turn.state === "busy"}
        onReviewProposal={() => {
          setReviewingProposal(true);
          onReview();
        }}
      />
      <ChatComposer
        slashCommands={leadSlash[activeThreadId] ?? []}
        busy={turn.state === "busy"}
        stopped={turn.state === "stopped"}
        queued={turn.queued}
        stoppedHint={t("lead.engineStopped")}
        onSend={(text) => void sendLeadChat(activeThreadId, text)}
        onStop={() => void interruptLead(activeThreadId)}
        onTakeOver={async () => {
          const st = await api.leadState(activeThreadId);
          if (!st.native_id) return false;
          await api.leadStop(activeThreadId);
          await navigator.clipboard.writeText(
            resumeCommand("claude", st.cwd, st.native_id),
          );
          return true;
        }}
      />
    </div>
  );
}
