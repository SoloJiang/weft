import path from "node:path"

import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  define: {
    "process.env.NODE_ENV": JSON.stringify("production"),
  },
  build: {
    outDir: "../crates/daemon/web",
    emptyOutDir: false,
    lib: {
      entry: path.resolve(import.meta.dirname, "src/embed.ts"),
      name: "WeftCodex",
      formats: ["iife"],
      fileName: () => "weft.js",
      cssFileName: "weft",
    },
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        inlineDynamicImports: true,
        assetFileNames: (asset) => {
          const names = "names" in asset && Array.isArray(asset.names)
            ? asset.names
            : [asset.name]
          return names.some((name) => typeof name === "string" && name.endsWith(".css"))
            ? "weft.css"
            : "assets/[name][extname]"
        },
      },
    },
  },
})
