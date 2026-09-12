import * as React from "react"
import { createPortal } from "react-dom"

import App from "@/App"
import { DialogLayer } from "@/components/dialogs"
import { HostProvider, type WeftHost } from "@/host"
import { I18nProvider, languageFromLocale } from "@/i18n"
import { PortalProvider } from "@/portal"
import { WeftSessionProvider, useWeftSession } from "@/session"
import SidebarApp from "@/SidebarApp"
import { ToastStack, WeftWorkspaceProvider, useWeftWorkspace } from "@/workspace-store"

function OverlayDialogs() {
  const session = useWeftSession()
  const store = useWeftWorkspace()
  return (
    <DialogLayer
      state={session.dialog}
      onClose={session.closeDialog}
      onCreateWorkspace={store.createWorkspace}
      onCreateIssue={store.createIssue}
      onImportRepositories={store.importRepositories}
      onSendMessage={store.sendMessage}
    />
  )
}

function HostedShell({
  sidebarTarget,
  mainTarget,
  overlayTarget,
}: {
  sidebarTarget: HTMLElement
  mainTarget: HTMLElement
  overlayTarget: HTMLElement
}) {
  const session = useWeftSession()
  // Dialog writes live on the workspace store, so the main tree can leave
  // while a native thread is open. The board stays in the store; coming back
  // does not refetch.
  return (
    <>
      {createPortal(<SidebarApp />, sidebarTarget)}
      {session.hostView === "workspace" ? createPortal(<App />, mainTarget) : null}
      {createPortal(
        <>
          <OverlayDialogs />
          <ToastStack />
        </>,
        overlayTarget,
      )}
    </>
  )
}

function PreviewShell() {
  return (
    <div className="preview-shell">
      <div className="preview-sidebar">
        <SidebarApp />
      </div>
      <div className="preview-main">
        <App />
      </div>
      <OverlayDialogs />
      <ToastStack />
    </div>
  )
}

export function WeftApp({
  host,
  layout,
  sidebarTarget,
  mainTarget,
  overlayTarget,
}: {
  host: WeftHost
  layout: "preview" | "hosted"
  sidebarTarget?: HTMLElement
  mainTarget?: HTMLElement
  overlayTarget?: HTMLElement
}) {
  const [locale, setLocale] = React.useState(host.locale)

  React.useEffect(() => {
    const sync = () => setLocale(document.documentElement.lang || host.locale)
    const observer = new MutationObserver(sync)
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["lang"] })
    return () => observer.disconnect()
  }, [host])

  const lang = languageFromLocale(locale)
  const portalContainer = overlayTarget ?? null

  return (
    <HostProvider host={host}>
      <I18nProvider lang={lang}>
        <PortalProvider container={portalContainer}>
          <WeftSessionProvider host={host}>
            <WeftWorkspaceProvider>
              {layout === "hosted" && sidebarTarget && mainTarget && overlayTarget ? (
                <HostedShell
                  sidebarTarget={sidebarTarget}
                  mainTarget={mainTarget}
                  overlayTarget={overlayTarget}
                />
              ) : (
                <PreviewShell />
              )}
            </WeftWorkspaceProvider>
          </WeftSessionProvider>
        </PortalProvider>
      </I18nProvider>
    </HostProvider>
  )
}
