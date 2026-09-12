let apiOrigin = ""

export function configureApi(origin: string): void {
  apiOrigin = origin.replace(/\/$/, "")
}

export function apiUrl(path: string): string {
  if (!path.startsWith("/")) {
    throw new Error("API path must be absolute")
  }
  return `${apiOrigin}${path}`
}

export class ApiError extends Error {
  status: number
  /**
   * The parsed error payload, kept whole.
   *
   * Some endpoints answer with a typed `code` plus the fields that make the
   * failure actionable (a revision conflict carries both revisions). Reducing
   * that to a message would force callers back to string-matching, which is
   * exactly what the typed errors exist to replace.
   */
  body: Record<string, unknown>

  constructor(message: string, status: number, body: Record<string, unknown> = {}) {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.body = body
  }
}

export async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers = new Headers(options.headers)
  if (options.body && !headers.has("content-type")) {
    headers.set("content-type", "application/json")
  }

  const response = await fetch(apiUrl(path), { ...options, headers })
  const body = (await response.json().catch(() => ({}))) as Record<string, unknown>
  if (!response.ok) {
    const message = typeof body.error === "string" ? body.error : `HTTP ${response.status}`
    throw new ApiError(message, response.status, body)
  }
  return body as T
}

export function jsonRequest(method: "POST", body: unknown = {}): RequestInit {
  return { method, body: JSON.stringify(body) }
}

export function slugify(value: string): string {
  return (
    value
      .toLowerCase()
      .replace(/[^a-z0-9\u4e00-\u9fff]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "item"
  )
}
