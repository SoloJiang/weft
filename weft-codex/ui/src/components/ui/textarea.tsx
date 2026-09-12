import * as React from "react"

import { cn } from "@/lib/utils"

/** Chrome lives in index.css; see the note on Input for why. */
function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn("w-full", className)}
      {...props}
    />
  )
}

export { Textarea }
