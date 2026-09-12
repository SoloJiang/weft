import * as React from "react"

const PortalContext = React.createContext<HTMLElement | null>(null)

export function PortalProvider({
  container,
  children,
}: {
  container: HTMLElement | null
  children: React.ReactNode
}) {
  return <PortalContext.Provider value={container}>{children}</PortalContext.Provider>
}

export function usePortalContainer(): HTMLElement | undefined {
  return React.useContext(PortalContext) ?? undefined
}
