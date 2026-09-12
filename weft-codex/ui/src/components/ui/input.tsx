import * as React from "react"

import { cn } from "@/lib/utils"

/**
 * Structure and behaviour only — the chrome lives in index.css.
 *
 * The utilities that used to be here (`h-8 rounded-lg border-input
 * bg-transparent px-2.5 dark:bg-input/30` …) never rendered: Tailwind emits
 * them into a layer, and the unlayered `input` element rule outranks every
 * layered declaration. The one set that did survive was `focus-visible:ring-3`,
 * because it paints a box-shadow that nothing else claimed — which is how a
 * single field ended up with two concentric focus rings.
 */
function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-slot="input"
      className={cn("w-full", className)}
      {...props}
    />
  )
}

export { Input }
