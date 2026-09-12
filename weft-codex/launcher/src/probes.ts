import type { CdpSession } from "./cdp.js"

export type CompatibilityTier = "safe-mode" | "weft-mode"

export interface CapabilityProbe {
  id: string
  ok: boolean
  detail: string
  requiredFor: "base" | "additive" | "subtractive" | "optional"
  /** User-facing failure text; present only when `ok` is false. */
  reason?: string
}

export interface ProbeReport {
  tier: CompatibilityTier
  probes: CapabilityProbe[]
}

interface RuntimeEvaluateResult {
  result?: {
    value?: unknown
    description?: string
  }
  exceptionDetails?: unknown
}

interface RendererSnapshot {
  selectors: Record<string, boolean>
  tokens: Record<string, string>
  modeSwitcher: boolean
  /**
   * Whether the single scoped mode trigger carries a non-empty `id`.
   * `associatedModeMenu()` pairs the trigger to its popup via
   * `aria-labelledby === button.id`, and `ensureNativeCodexMode()` bails out
   * without it — so a trigger without an id passes the presence check while
   * Weft mode still falls back to safe mode. Measured non-empty on build 6321
   * (`radix-_r_3_`).
   */
  modeSwitcherId: boolean
  /**
   * Drag regions found inside a *usable* `main`, matching the runtime scope in
   * `ensureWorkspaceRoot`. A document-wide count over-reports: it stays green
   * even if Codex moves the titlebar out of `main`, where the runtime looks.
   */
  titlebarDragRegion: boolean
  /**
   * How many conversation rows the sidebar is currently showing.
   *
   * The thread anchors are *data*-dependent, not structure-dependent: a profile
   * with no conversations renders a perfectly healthy sidebar that simply has no
   * rows to carry them. Without this count we cannot tell "Codex renamed the
   * attribute" from "this user has not started a chat yet", and a new profile
   * would be locked out of Weft forever.
   */
  threadRowCount: number
  /**
   * Whether the mode row still exposes exactly one sibling container — the slot
   * the native search and activity buttons live in, and the one Weft mode puts
   * its own search and inbox entries into.
   *
   * Deliberately `optional`: losing it costs the *placement* of those two
   * entries, not the capability. The sidebar renders them in its own header
   * instead, so failing closed over this would trade a cosmetic regression for
   * a functional one (see §5.8 on sizing `requiredFor` to the real cost of the
   * anchor's absence).
   */
  headerActionSlot: boolean
  locale: string
}

const SELECTORS = {
  "renderer.root": "#root",
  "renderer.main": "main",
  "sidebar.scroll": "[data-app-action-sidebar-scroll]",
  "sidebar.section": "[data-app-action-sidebar-section]",
  "sidebar.heading": "[data-app-action-sidebar-section-heading]",
  "sidebar.projectCreate": "[data-app-action-sidebar-project-create]",
  "sidebar.threadRow": "[data-app-action-sidebar-thread-row]",
  "sidebar.threadRoute": "[data-app-action-sidebar-thread-id]",
  "sidebar.threadActive": "[data-app-action-sidebar-thread-active]",
} as const

/**
 * Shared, dependency-free source used by both the compatibility probe and the
 * document-start renderer agent. Codex can keep an inert route transition in
 * the DOM alongside the active route, so a bare `querySelector("main")` is not
 * a safe mount anchor.
 */
export const VISIBLE_MAIN_HELPERS_SOURCE = `
    function mainVisibleArea(element) {
      const rect = element.getBoundingClientRect();
      const viewportWidth = Math.max(document.documentElement.clientWidth, window.innerWidth || 0);
      const viewportHeight = Math.max(document.documentElement.clientHeight, window.innerHeight || 0);
      const width = Math.max(0, Math.min(rect.right, viewportWidth) - Math.max(rect.left, 0));
      const height = Math.max(0, Math.min(rect.bottom, viewportHeight) - Math.max(rect.top, 0));
      return width * height;
    }

    function usableMainCandidate(element) {
      if (!(element instanceof HTMLElement)) return false;
      if (element.closest('[inert], [aria-hidden="true"], [hidden]')) return false;
      const style = getComputedStyle(element);
      if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
      if (style.pointerEvents === "none") return false;
      return mainVisibleArea(element) > 0;
    }

    function visibleMainRoute() {
      const candidates = [...document.querySelectorAll("main")].filter(usableMainCandidate);
      candidates.sort((left, right) => mainVisibleArea(right) - mainVisibleArea(left));
      return candidates[0] || null;
    }
`

/**
 * User-facing failure text. Deliberately free of CSS selectors: these strings
 * surface in the launcher log and `doctor`, where a raw attribute name tells a
 * user nothing actionable. The selector still travels in `detail` for triage.
 */
const FAILURE_REASONS: Record<
  keyof typeof SELECTORS | "mode.switcher" | "host.locale" | "titlebar.dragRegion" | "sidebar.headerActionSlot",
  string
> = {
  "renderer.root": "Codex 应用外壳未就绪",
  "renderer.main": "找不到 Codex 主工作区，无法挂载 workspace",
  "sidebar.scroll": "找不到 Codex 侧边栏，无法挂载 Weft 区块",
  "sidebar.section": "Codex 侧边栏分区结构已变化",
  "sidebar.heading": "Codex 侧边栏分区标题结构已变化",
  "sidebar.projectCreate": "找不到 Codex 的新建项目入口",
  "sidebar.threadRow": "找不到 Codex 的会话行，逐会话增强不可用",
  "sidebar.threadRoute": "找不到 Codex 的会话标识，无法打开指定会话",
  "sidebar.threadActive": "无法识别当前打开的会话，Issue 归属解析不可用",
  "mode.switcher": "找不到 Codex 的模式切换入口，无法进入 Weft",
  "sidebar.headerActionSlot": "Codex 侧边栏头部结构已变化，搜索与收件箱入口改在 Weft 侧边栏内显示",
  "host.locale": "无法读取 Codex 的界面语言",
  "titlebar.dragRegion": "未检测到原生标题栏拖拽区域",
}

/**
 * Codex publishes `--color-token-*` as a thin alias layer over its `--vscode-*`
 * theme variables, and build 6662 dropped two of those aliases while keeping
 * the variables behind them. Reading the `--vscode-*` name is therefore both
 * the durable choice and a value-preserving one: measured on 6662,
 * `--vscode-button-foreground` is `#fafafa` and `--vscode-input-background` is
 * `rgba(251, 251, 251, 0.96)`, byte-identical to what the aliases reported on
 * 6321. `theme.sidebarSurface` has always been sourced this way. See
 * docs/compat/codex-builds.md §8.1.
 */
export const TOKEN_PROBES = {
  "theme.sidebarSurface": "--vscode-sideBar-background",
  "theme.mainSurface": "--color-token-main-surface-primary",
  "theme.dropdownSurface": "--color-token-dropdown-background",
  "theme.foreground": "--color-token-foreground",
  "theme.secondaryText": "--color-token-text-secondary",
  "theme.border": "--color-token-border",
  "theme.borderHeavy": "--color-token-border-heavy",
  "theme.primary": "--color-token-primary",
  "theme.buttonForeground": "--vscode-button-foreground",
  "theme.linkForeground": "--color-token-text-link-foreground",
  "theme.inputBackground": "--vscode-input-background",
  "theme.inputBorder": "--color-token-input-border",
  "theme.hoverBackground": "--color-token-list-hover-background",
  "theme.fontSans": "--font-sans",
  "theme.fontMono": "--font-mono",
  "theme.radiusLarge": "--radius-lg",
  "theme.radiusMedium": "--radius-md",
  "theme.radiusSmall": "--radius-sm",
} as const

/**
 * Everything `ui/src/index.css` reads off the host, probed values included.
 *
 * The four `--color-token-*` names at the end resolve to nothing on build 6662
 * — the same alias-layer contraction that took `button-foreground` and
 * `input-background` (docs/compat/codex-builds.md §8.1). They stay listed
 * because older builds still publish them and the stylesheet still tries them
 * first; each is followed by the `--vscode-*` name that survived, which is
 * what actually answers on 6662.
 */
export const ALLOWED_CODEX_TOKENS = [
  ...Object.values(TOKEN_PROBES),
  "--vscode-button-hoverBackground",
  "--vscode-focusBorder",
  "--vscode-font-family",
  "--vscode-editor-font-family",
  "--color-token-input-placeholder-foreground",
  "--vscode-input-placeholderForeground",
  "--color-token-charts-yellow",
  "--vscode-charts-yellow",
  "--color-token-charts-red",
  "--vscode-charts-red",
  "--color-token-charts-green",
  "--vscode-charts-green",
] as const

function selectorProbe(
  snapshot: RendererSnapshot,
  id: keyof typeof SELECTORS,
  requiredFor: CapabilityProbe["requiredFor"],
): CapabilityProbe {
  const ok = snapshot.selectors[id] === true
  if (ok) return { id, ok, detail: SELECTORS[id], requiredFor }
  return {
    id,
    ok,
    detail: `Missing ${SELECTORS[id]}`,
    requiredFor,
    reason: FAILURE_REASONS[id],
  }
}

/**
 * A probe for an anchor that only exists once the user has conversations.
 *
 * With no rows in the sidebar there is nothing to carry the attribute, so its
 * absence proves nothing — reporting a failure there would degrade every fresh
 * profile. We report "not applicable" instead, which keeps the tier intact and
 * still says plainly that the anchor went unverified.
 *
 * The trade-off is explicit: if Codex renames the row attribute itself, a
 * profile with no chats cannot tell. Any profile with at least one conversation
 * still catches it, and `scripts/upgrade-drill.mjs` covers the rename directly.
 */
function threadAnchorProbe(
  snapshot: RendererSnapshot,
  id: keyof typeof SELECTORS,
  requiredFor: CapabilityProbe["requiredFor"],
): CapabilityProbe {
  if (snapshot.threadRowCount === 0) {
    return {
      id,
      ok: true,
      detail: `Not applicable: the sidebar shows no conversations`,
      requiredFor,
    }
  }
  return selectorProbe(snapshot, id, requiredFor)
}

/**
 * The tokens Weft is actually gated on, per the 08-16 spec §8.3 / §8.4: core
 * surfaces, foreground, fonts.
 *
 * The rest are cosmetic. Every one of them is consumed through a fallback in
 * `ui/src/index.css`, and `applyRadiusTokens` already skips radii it cannot
 * read, so losing one costs fidelity to the host palette — not usability.
 * Gating on all eighteen is what let build 6662 lock the whole product out by
 * renaming two colour aliases; docs/compat/codex-builds.md §8 records the
 * incident and §5.8 the rule it broke — size `requiredFor` to what the
 * anchor's absence actually costs.
 */
const CORE_TOKEN_PROBES = new Set([
  "theme.sidebarSurface",
  "theme.mainSurface",
  "theme.dropdownSurface",
  "theme.foreground",
  "theme.fontSans",
  "theme.fontMono",
])

function tokenProbe(snapshot: RendererSnapshot, id: string, token: string): CapabilityProbe {
  const value = snapshot.tokens[token]?.trim() ?? ""
  return {
    id,
    ok: Boolean(value),
    detail: value || `Missing ${token}`,
    requiredFor: CORE_TOKEN_PROBES.has(id) ? "base" : "optional",
  }
}

/**
 * The mode switcher needs two things, and reporting them as one boolean hid a
 * real contradiction: a trigger without an `id` passed the presence check while
 * `ensureNativeCodexMode` still forced safe mode. Both conditions gate the same
 * capability, so they stay one probe — but `detail` names which half failed.
 */
function modeSwitcherProbe(snapshot: RendererSnapshot): CapabilityProbe {
  const id = "mode.switcher"
  const requiredFor = "base" as const
  if (!snapshot.modeSwitcher) {
    return {
      id,
      ok: false,
      detail: "Missing one semantic sidebar mode menu trigger",
      requiredFor,
      reason: FAILURE_REASONS["mode.switcher"],
    }
  }
  if (!snapshot.modeSwitcherId) {
    return {
      id,
      ok: false,
      detail: "Sidebar mode menu trigger has no id; cannot pair it to its menu",
      requiredFor,
      reason: FAILURE_REASONS["mode.switcher"],
    }
  }
  return { id, ok: true, detail: "Sidebar mode menu trigger", requiredFor }
}

export function classifyCompatibility(probes: CapabilityProbe[]): CompatibilityTier {
  if (probes.some((probe) => probe.requiredFor !== "optional" && !probe.ok)) return "safe-mode"
  return "weft-mode"
}

export function reportFromSnapshot(snapshot: RendererSnapshot): ProbeReport {
  const probes: CapabilityProbe[] = [
    selectorProbe(snapshot, "renderer.root", "base"),
    selectorProbe(snapshot, "renderer.main", "base"),
    selectorProbe(snapshot, "sidebar.scroll", "base"),
    selectorProbe(snapshot, "sidebar.section", "optional"),
    selectorProbe(snapshot, "sidebar.heading", "optional"),
    ...Object.entries(TOKEN_PROBES).map(([id, token]) => tokenProbe(snapshot, id, token)),
    // Thread anchors back deep-link and active-thread→Issue resolution. When
    // conversations exist, their loss cannot enter Weft. All four are verified
    // present on build 6321; see docs/compat/codex-builds.md §2.
    //
    // `sidebar.threadActive` was previously not probed at all, even though
    // activeThreadId() reads it and it is the sole input to Issue resolution.
    // Structural: the create-project affordance is leftover sidebar chrome and
    // is no longer an enter-Weft condition.
    selectorProbe(snapshot, "sidebar.projectCreate", "optional"),
    // Data-dependent: only assertable once conversations exist. See
    // threadAnchorProbe — gating the tier on these unconditionally locked
    // every fresh profile out of Weft.
    threadAnchorProbe(snapshot, "sidebar.threadRow", "base"),
    threadAnchorProbe(snapshot, "sidebar.threadRoute", "base"),
    threadAnchorProbe(snapshot, "sidebar.threadActive", "base"),
    modeSwitcherProbe(snapshot),
    {
      id: "host.locale",
      ok: Boolean(snapshot.locale.trim()),
      detail: snapshot.locale.trim() || "Document locale is empty",
      requiredFor: "base",
      ...(snapshot.locale.trim() ? {} : { reason: FAILURE_REASONS["host.locale"] }),
    },
    {
      id: "sidebar.headerActionSlot",
      ok: snapshot.headerActionSlot,
      detail: snapshot.headerActionSlot
        ? "Mode row exposes one action slot"
        : "Mode row has no single action slot to host the Weft entries",
      requiredFor: "optional",
      ...(snapshot.headerActionSlot ? {} : { reason: FAILURE_REASONS["sidebar.headerActionSlot"] }),
    },
    {
      id: "titlebar.dragRegion",
      ok: snapshot.titlebarDragRegion,
      detail: snapshot.titlebarDragRegion
        ? "Native titlebar drag region preserved"
        : "No native titlebar drag region detected",
      requiredFor: "base",
      ...(snapshot.titlebarDragRegion ? {} : { reason: FAILURE_REASONS["titlebar.dragRegion"] }),
    },
  ]
  return { tier: classifyCompatibility(probes), probes }
}

export function buildProbeExpression(): string {
  return `(() => {
    ${VISIBLE_MAIN_HELPERS_SOURCE}
    const selectors = ${JSON.stringify(SELECTORS)};
    const tokens = ${JSON.stringify(ALLOWED_CODEX_TOKENS)};
    const rootStyle = getComputedStyle(document.documentElement);
    const mainRoute = visibleMainRoute();
    const sidebar = document.querySelector(selectors["sidebar.scroll"]);
    const navigation = sidebar?.closest("nav");
    const modeButtons = navigation
      ? [...navigation.querySelectorAll('button[aria-haspopup="menu"][aria-expanded][data-state]')]
          .filter((button) => !sidebar.contains(button))
      : [];
    // Scope the drag-region check to the same route ensureWorkspaceRoot mounts
    // into. A document-wide count over-reports: it stays green even if Codex
    // moves the titlebar out of the live main, which is where the runtime looks.
    const titlebarDragRegion = Boolean(mainRoute) &&
      [...mainRoute.querySelectorAll("header, header *")].some((element) =>
        getComputedStyle(element).getPropertyValue("-webkit-app-region") === "drag"
      );
    return {
      selectors: Object.fromEntries(Object.entries(selectors).map(([id, selector]) => [
        id,
        id === "renderer.main" ? Boolean(mainRoute) : Boolean(document.querySelector(selector)),
      ])),
      tokens: Object.fromEntries(tokens.map((token) => [token, rootStyle.getPropertyValue(token)])),
      modeSwitcher: modeButtons.length === 1,
      modeSwitcherId: modeButtons.length === 1 && Boolean(modeButtons[0].id),
      threadRowCount: document.querySelectorAll(selectors["sidebar.threadRow"]).length,
      // Same shape test the renderer agent's actionSlot() uses, so the probe
      // and the runtime cannot disagree about whether the slot exists.
      headerActionSlot: modeButtons.length === 1 &&
        [...(modeButtons[0].parentElement?.children ?? [])]
          .filter((child) => child instanceof HTMLElement && child !== modeButtons[0]).length === 1,
      titlebarDragRegion,
      locale: document.documentElement.lang || navigator.language || "",
    };
  })()`
}

export async function probeRenderer(session: CdpSession): Promise<ProbeReport> {
  const expression = buildProbeExpression()
  const response = await session.send<RuntimeEvaluateResult>("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  })
  if (response.exceptionDetails) throw new Error("Renderer capability probe threw an exception")
  const value = response.result?.value
  if (!value || typeof value !== "object") {
    throw new Error(response.result?.description || "Renderer capability probe returned no value")
  }
  return reportFromSnapshot(value as RendererSnapshot)
}
