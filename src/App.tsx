import { StoreProvider, useStore } from "./state/store";
import { WorkspaceNav } from "./nav/WorkspaceNav";
import { ThreadBoard } from "./board/ThreadBoard";
import { WorkspaceHome } from "./board/WorkspaceHome";
import { SessionView } from "./session/SessionView";

function Main() {
  const { activeSessionId, activeThreadId } = useStore();
  if (activeSessionId != null) return <SessionView />;
  if (activeThreadId != null) return <ThreadBoard />;
  return <WorkspaceHome />;
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
