import path from "node:path"

import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig, type Plugin } from "vite"

const normalizeGeneratedWhitespace: Plugin = {
  name: "normalize-generated-whitespace",
  enforce: "post" as const,
  generateBundle(_options, bundle) {
    for (const output of Object.values(bundle)) {
      if (output.type === "chunk") {
        output.code = output.code.replace(/^[\t ]+$/gm, "")
      }
    }
  },
}

export default defineConfig({
  base: "/web/",
  plugins: [react(), tailwindcss(), normalizeGeneratedWhitespace],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  build: {
    outDir: "../crates/daemon/web",
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:47810",
    },
  },
})
