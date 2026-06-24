import { useEffect, useState } from "react";
import { StoreProvider, useStore } from "./state/store";
import { WorkspaceNav } from "./nav/WorkspaceNav";
import { AppTopBar } from "./nav/AppTopBar";
import { ThreadBoard } from "./board/ThreadBoard";
import { WorkspaceHome } from "./board/WorkspaceHome";
import { NeedsYouView } from "./board/NeedsYouView";
import { WorkerConversation } from "./session/WorkerConversation";
import { DangerToast } from "./components/DangerToast";
import { Toasts } from "./components/Toast";
import { FileMenu, useCmdAffordance } from "./components/FileMenu";
import { CommandPalette } from "./components/CommandPalette";
import { NeedsDock } from "./components/NeedsDock";
import { FirstRunOnboarding } from "./components/FirstRunOnboarding";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { SettingsScreen } from "./nav/SettingsDialog";
import { useAppShortcuts } from "./state/shortcuts";
import { useSystemNotifications } from "./lib/notifications";

// Below this window width the nav rail can't coexist with an open worker side
// panel: rail (288) + diff panel min (360) + main min (360). When a diff/files
// panel is open under it, hide the rail to give the panel + main room — without
// touching the user's manual collapse choice (navCollapsed governs the rest).
const RAIL_PLUS_PANEL_MIN = 288 + 360 + 360;

function NavRailGate() {
  const { navCollapsed, activeSidePanel } = useStore();
  // Boolean (not raw width) so a resize only re-renders on a threshold cross.
  const [narrow, setNarrow] = useState(() => window.innerWidth < RAIL_PLUS_PANEL_MIN);
  useEffect(() => {
    const onResize = () => setNarrow(window.innerWidth < RAIL_PLUS_PANEL_MIN);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  if (navCollapsed) return null;
  if (activeSidePanel != null && narrow) return null;
  return <WorkspaceNav />;
}

function Main() {
  const { viewing, activeThreadId, showNeeds } = useStore();
  // Needs-you is the workspace-wide exception queue — it takes precedence over
  // whatever thread/board is open underneath, so it's reachable from anywhere.
  if (showNeeds) return <NeedsYouView />;
  if (viewing != null) return <WorkerConversation />;
  if (activeThreadId != null) return <ThreadBoard />;
  return <WorkspaceHome />;
}

function Shell() {
  const {
    activeWorkspaceId,
    viewing,
    activeThreadId,
    showNeeds,
    homeTab,
  } = useStore();
  useAppShortcuts();
  useSystemNotifications();
  useCmdAffordance();
  if (
    homeTab === "settings" &&
    !showNeeds &&
    viewing == null &&
    activeThreadId == null
  ) {
    return (
      <div className="h-screen w-screen overflow-hidden bg-bg text-ink">
        <SettingsScreen />
        <Toasts />
        <CommandPalette />
      </div>
    );
  }
  const showDock =
    activeWorkspaceId != null &&
    !showNeeds &&
    (viewing != null ||
      activeThreadId != null ||
      homeTab === "board");
  // Key the boundary by route so navigating away from a crashed screen clears it.
  const routeKey = `${showNeeds ? "needs" : ""}|${viewing ?? ""}|${activeThreadId ?? ""}|${homeTab}`;
  return (
    <div className="flex h-screen w-screen overflow-hidden bg-bg text-ink">
      <NavRailGate />
      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <AppTopBar />
        {showDock && <NeedsDock />}
        <ErrorBoundary key={routeKey}>
          <div className="flex min-h-0 min-w-0 flex-1 flex-col weft-screen-in">
            <Main />
          </div>
        </ErrorBoundary>
      </div>
      <DangerToast />
      <Toasts />
      <FileMenu />
      <CommandPalette />
      <FirstRunOnboarding />
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
