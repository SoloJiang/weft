import { createRoot } from "react-dom/client"

import { configureApi } from "@/api"
import { createPreviewHost, installHost } from "@/host"
import { WeftApp } from "@/WeftApp"
import "@/index.css"

const root = document.getElementById("root")
if (!root) throw new Error("Missing #root")

const host = createPreviewHost()
installHost(host)
configureApi("")
createRoot(root).render(<WeftApp host={host} layout="preview" />)
