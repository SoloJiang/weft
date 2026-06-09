import { StoreProvider, useStore } from "./state/store";
import { WorkspaceNav } from "./nav/WorkspaceNav";
import { ThreadBoard } from "./board/ThreadBoard";
import { WorkspaceHome } from "./board/WorkspaceHome";
import { SessionView } from "./session/SessionView";
import { ObserveView } from "./session/ObserveView";
import { DangerToast } from "./components/DangerToast";
import { Toasts } from "./components/Toast";
import { CommandPalette } from "./components/CommandPalette";
import { ErrorBoundary } from "./components/ErrorBoundary";

function Main() {
  const { activeSessionId, viewing, activeThreadId } = useStore();
  if (activeSessionId != null) return <SessionView />;
  if (viewing != null) return <ObserveView />;
  if (activeThreadId != null) return <ThreadBoard />;
  return <WorkspaceHome />;
}

function Shell() {
  const { navCollapsed, activeSessionId, viewing, activeThreadId } = useStore();
  // Key the boundary by route so navigating away from a crashed screen clears it.
  const routeKey = `${activeSessionId ?? ""}|${viewing ?? ""}|${activeThreadId ?? ""}`;
  return (
    <div className="flex h-screen w-screen overflow-hidden bg-bg text-ink">
      {!navCollapsed && <WorkspaceNav />}
      <ErrorBoundary key={routeKey}>
        <div className="flex min-w-0 flex-1 flex-col weft-screen-in">
          <Main />
        </div>
      </ErrorBoundary>
      <DangerToast />
      <Toasts />
      <CommandPalette />
    </div>
  );
}

export default function App() {
  return (
    <StoreProvider>
      <Shell />
    </StoreProvider>
  );
}
