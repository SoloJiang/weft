import { createElement } from "react"
import { createRoot, type Root } from "react-dom/client"

import { configureApi } from "@/api"
import { WeftApp } from "@/WeftApp"
import { currentHost, installHost, type WeftHost } from "@/host"
import "@/index.css"

export interface MountWeftOptions {
  sidebar: ShadowRoot
  main: ShadowRoot
  overlay: ShadowRoot
  host: WeftHost
}

function mountNode(root: ShadowRoot, className: string): HTMLElement {
  const existing = root.querySelector<HTMLElement>(`:scope > .${className}`)
  if (existing) return existing
  const node = document.createElement("div")
  node.className = `weft-mount ${className}`
  root.append(node)
  return node
}

export function mountWeft(options: MountWeftOptions): () => void {
  installHost(options.host)
  configureApi(options.host.weftdOrigin)
  const sidebar = mountNode(options.sidebar, "weft-sidebar-mount")
  const main = mountNode(options.main, "weft-workspace-mount")
  const overlay = mountNode(options.overlay, "weft-overlay-mount")

  const reactHost = document.createElement("div")
  reactHost.className = "weft-react-host"
  overlay.append(reactHost)

  const root: Root = createRoot(reactHost)
  root.render(createElement(WeftApp, {
    host: options.host,
    layout: "hosted",
    sidebarTarget: sidebar,
    mainTarget: main,
    overlayTarget: overlay,
  }))

  return () => {
    root.unmount()
    reactHost.remove()
    if (currentHost() === options.host) installHost(null)
  }
}

declare global {
  interface Window {
    WeftCodex?: { mountWeft: typeof mountWeft }
  }
}

window.WeftCodex = { mountWeft }
