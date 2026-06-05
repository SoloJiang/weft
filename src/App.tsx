import { Layers } from "lucide-react";
import { StoreProvider, useStore } from "./state/store";
import { WorkspaceNav } from "./nav/WorkspaceNav";
import { ThreadBoard } from "./board/ThreadBoard";
import { SessionView } from "./session/SessionView";

function Main() {
  const { activeSessionId, activeThreadId } = useStore();
  if (activeSessionId != null) return <SessionView />;
  if (activeThreadId != null) return <ThreadBoard />;
  return <NoThread />;
}

function NoThread() {
  return (
    <section className="flex min-w-0 flex-1 flex-col items-center justify-center bg-bg px-6 text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Layers size={22} className="text-ink-faint" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">Pick a thread</h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">
        A thread is a work line. Open one to see its directions — the parallel,
        per-repo work lines you run agents in — laid out as a board.
      </p>
    </section>
  );
}

export default function App() {
  return (
    <StoreProvider>
      <div className="flex h-screen w-screen overflow-hidden bg-bg text-ink">
        <WorkspaceNav />
        <Main />
      </div>
    </StoreProvider>
  );
}
