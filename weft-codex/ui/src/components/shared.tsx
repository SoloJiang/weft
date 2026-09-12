import * as React from "react"
import { MessageCircle } from "lucide-react"

import { Button, type buttonVariants } from "@/components/ui/button"
import { useI18n, type MessageKey } from "@/i18n"
import { currentHost } from "@/host"
import { cn } from "@/lib/utils"
import { useWeftSession } from "@/session"
import { codexThreadHref } from "@/lib/thread-open"
import type { VariantProps } from "class-variance-authority"
import type { Direction, Issue, RepoComponent } from "@/types"

interface AsyncButtonProps
  extends Omit<React.ComponentProps<typeof Button>, "onClick">,
    VariantProps<typeof buttonVariants> {
  label: string
  pendingLabel: string
  onAction: () => Promise<void>
  onError: (error: unknown) => void
  iconOnly?: boolean
}

export function AsyncButton({
  label,
  pendingLabel,
  onAction,
  onError,
  disabled,
  children,
  iconOnly = false,
  size,
  className,
  // A text button's visible label is often too terse to name it on its own in a
  // list ("Start lead" on every card). Let callers give the full phrase without
  // rendering it: before this, the explicit attribute below erased anything
  // spread in, so no caller could override the accessible name.
  "aria-label": ariaLabel,
  ...props
}: AsyncButtonProps) {
  const [pending, setPending] = React.useState(false)
  const resolvedLabel = pending ? pendingLabel : label

  const run = async () => {
    if (pending || disabled) return
    setPending(true)
    try {
      await onAction()
    } catch (error) {
      onError(error)
    } finally {
      setPending(false)
    }
  }

  return (
    <Button
      {...props}
      size={iconOnly ? (size ?? "icon-sm") : size}
      className={cn(className, iconOnly && "async-button-icon")}
      disabled={pending || disabled}
      aria-busy={pending}
      aria-label={ariaLabel ?? (iconOnly ? resolvedLabel : undefined)}
      title={iconOnly ? resolvedLabel : undefined}
      onClick={run}
    >
      {children}
      {iconOnly ? <span className="sr-only">{resolvedLabel}</span> : resolvedLabel}
    </Button>
  )
}

export { codexThreadHref, openCodexThread } from "@/lib/thread-open"

export function ThreadLink({
  threadId,
  onError,
  label,
  pendingLabel,
  className,
  iconOnly = false,
}: {
  threadId: string
  onError?: (error: unknown) => void
  label?: string
  pendingLabel?: string
  className?: string
  iconOnly?: boolean
}) {
  const { t } = useI18n()
  const session = useWeftSession()
  if (!threadId) return null
  const pending = session.openingThreadId === threadId
  const resolvedLabel = pending
    ? (pendingLabel ?? t("loading.openingThread"))
    : (label ?? t("dir.openThread"))
  return (
    <Button
      asChild
      variant="ghost"
      size={iconOnly ? "icon-sm" : "default"}
      className={cn("thread-link", iconOnly && "thread-link-icon", className)}
    >
      <a
        href={codexThreadHref(threadId)}
        aria-label={resolvedLabel}
        title={resolvedLabel}
        aria-busy={pending}
        aria-disabled={pending}
        onClick={(event) => {
          if (!currentHost()) return
          event.preventDefault()
          if (pending) return
          void session.openThread(threadId).catch(() => {
            onError?.(new Error(t("err.threadOpen")))
          })
        }}
      >
        <MessageCircle aria-hidden="true" />
        {iconOnly ? <span className="sr-only">{resolvedLabel}</span> : resolvedLabel}
      </a>
    </Button>
  )
}

/**
 * The one way to reach a conversation, whatever owns it.
 *
 * Whether the thread already exists is a mechanism detail: the user has a
 * single concept — go to this thing's chat — and starting the agent when it is
 * not running yet happens on the way there. Both branches take the same label
 * and the same icon, so consistency is structural rather than two look-alikes
 * kept in sync by hand.
 */
export function ChatLink({
  threadId,
  label,
  pendingLabel,
  onStart,
  onError,
  className,
  iconOnly = false,
}: {
  threadId: string
  label: string
  pendingLabel: string
  /** Called only when there is no thread yet: start it, then open it. */
  onStart: () => Promise<void>
  onError?: (error: unknown) => void
  className?: string
  iconOnly?: boolean
}) {
  if (threadId) {
    return (
      <ThreadLink
        threadId={threadId}
        onError={onError}
        label={label}
        pendingLabel={pendingLabel}
        className={className}
        iconOnly={iconOnly}
      />
    )
  }
  return (
    <AsyncButton
      variant="ghost"
      className={cn("thread-link", iconOnly && "thread-link-icon", className)}
      label={label}
      pendingLabel={pendingLabel}
      onAction={onStart}
      onError={onError ?? (() => {})}
      iconOnly={iconOnly}
    >
      <MessageCircle aria-hidden="true" />
    </AsyncButton>
  )
}

export function LeadChatLink({
  issue,
  onOpen,
  onError,
  className,
  iconOnly = false,
}: {
  issue: Pick<Issue, "id" | "title" | "lead_codex_thread_id">
  onOpen: (issueId: number) => Promise<void>
  onError?: (error: unknown) => void
  className?: string
  iconOnly?: boolean
}) {
  const { t } = useI18n()
  return (
    <ChatLink
      threadId={issue.lead_codex_thread_id}
      label={t("issue.openChat", { title: issue.title })}
      pendingLabel={t("loading.openingChat")}
      onStart={() => onOpen(issue.id)}
      onError={onError}
      className={className}
      iconOnly={iconOnly}
    />
  )
}

export function TaskChatLink({
  direction,
  onOpen,
  onError,
  className,
  iconOnly = false,
}: {
  direction: Pick<Direction, "id" | "name" | "codex_thread_id">
  onOpen: (directionId: number) => Promise<void>
  onError?: (error: unknown) => void
  className?: string
  iconOnly?: boolean
}) {
  const { t } = useI18n()
  return (
    <ChatLink
      threadId={direction.codex_thread_id}
      label={t("dir.openChat", { name: direction.name })}
      pendingLabel={t("loading.openingChat")}
      onStart={() => onOpen(direction.id)}
      onError={onError}
      className={className}
      iconOnly={iconOnly}
    />
  )
}

export function EmptyState({
  titleKey,
  bodyKey,
  actionKey,
  onAction,
}: {
  titleKey: MessageKey
  bodyKey: MessageKey
  actionKey?: MessageKey
  onAction?: () => void
}) {
  const { t } = useI18n()
  return (
    <section className="empty-state">
      <h2>{t(titleKey)}</h2>
      <p>{t(bodyKey)}</p>
      {actionKey && onAction ? <Button onClick={onAction}>{t(actionKey)}</Button> : null}
    </section>
  )
}

export function Field({
  label,
  htmlFor,
  error,
  children,
  className,
}: {
  label: string
  htmlFor: string
  error?: string
  children: React.ReactNode
  className?: string
}) {
  return (
    <div className={cn("form-field", className)}>
      <label htmlFor={htmlFor}>{label}</label>
      {children}
      {error ? <p className="field-error" role="alert">{error}</p> : null}
    </div>
  )
}

export function parseStack(value: string): string[] {
  try {
    const parsed: unknown = JSON.parse(value || "[]")
    if (!Array.isArray(parsed)) return []
    return parsed.filter((item): item is string => typeof item === "string")
  } catch {
    return []
  }
}

export function parseComponents(value: string): RepoComponent[] {
  try {
    const parsed: unknown = JSON.parse(value || "[]")
    if (!Array.isArray(parsed)) return []
    return parsed.flatMap((item) => {
      if (!item || typeof item !== "object") return []
      const candidate = item as Partial<RepoComponent>
      if (typeof candidate.name !== "string" || typeof candidate.path !== "string") return []
      return [{
        name: candidate.name,
        path: candidate.path,
        summary: typeof candidate.summary === "string" ? candidate.summary : "",
      }]
    })
  } catch {
    return []
  }
}
