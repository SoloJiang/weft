import { ALLOWED_CODEX_TOKENS, VISIBLE_MAIN_HELPERS_SOURCE } from "./probes.js"

export type HostMode = "work" | "codex" | "weft"

const SIDEBAR_ROOT_ID = "weft-codex-sidebar-root"
const WORKSPACE_ROOT_ID = "weft-codex-workspace-root"
const OVERLAY_ROOT_ID = "weft-codex-overlay-root"
const NATIVE_CHECK_ATTR = "data-weft-codex-native-mode-check"
const HEADER_ACTION_ATTR = "data-weft-codex-header-action"
const HEADER_BADGE_ATTR = "data-weft-codex-header-badge"

export interface RendererAgentConfig {
  webBaseUrl: string
  bindingName: string
  initialMode: HostMode
  cspBypass: boolean
}

function isLoopback(hostname: string): boolean {
  return hostname === "127.0.0.1" || hostname === "localhost" || hostname === "[::1]"
}

export function validateRendererAgentConfig(config: RendererAgentConfig): RendererAgentConfig {
  const webUrl = new URL(config.webBaseUrl)
  if (webUrl.protocol !== "http:" || !isLoopback(webUrl.hostname)) {
    throw new Error("Renderer web URL must use loopback HTTP")
  }
  if (webUrl.username || webUrl.password) throw new Error("Renderer web URL cannot contain credentials")
  if (!/^[a-zA-Z_$][a-zA-Z0-9_$]{0,63}$/.test(config.bindingName)) {
    throw new Error("Renderer binding name is invalid")
  }
  return {
    ...config,
    webBaseUrl: webUrl.href,
  }
}

function serializeForScript(value: unknown): string {
  return JSON.stringify(value)
    .replaceAll("<", "\\u003c")
    .replaceAll("\u2028", "\\u2028")
    .replaceAll("\u2029", "\\u2029")
}

/**
 * Build the document-start renderer agent. The script has no imports and no
 * dependency on Codex's React tree. It mounts three shadow roots and calls
 * `WeftCodex.mountWeft`.
 */
export function buildRendererAgentSource(input: RendererAgentConfig): string {
  const config = validateRendererAgentConfig(input)
  const serializedConfig = serializeForScript(config)
  const serializedTokens = serializeForScript(ALLOWED_CODEX_TOKENS)

  return `;(() => {
    "use strict";

    const GLOBAL_KEY = "__weftCodexAgentV1";
    const config = ${serializedConfig};
    const allowedTokens = ${serializedTokens};
    const SIDEBAR_ROOT_ID = "weft-codex-sidebar-root";
    const WORKSPACE_ROOT_ID = "weft-codex-workspace-root";
    const OVERLAY_ROOT_ID = "weft-codex-overlay-root";
    const STYLE_ID = "weft-codex-host-style";
    const MODE_ITEM_ATTR = "data-weft-codex-mode-item";
    const NATIVE_CHECK_ATTR = "data-weft-codex-native-mode-check";
    const HEADER_ACTION_ATTR = "data-weft-codex-header-action";
    const HEADER_BADGE_ATTR = "data-weft-codex-header-badge";
    const THREAD_OPEN_RETRY_DELAYS = [0, 80, 160, 320, 640, 1000, 1800];
    const previous = window[GLOBAL_KEY];
    if (previous && typeof previous.dispose === "function") previous.dispose();

    const state = {
      mode: config.initialMode,
      nativeMode: config.initialMode === "work" ? "work" : "codex",
      view: "workspace",
      cspBypass: Boolean(config.cspBypass),
      sidebarRoot: null,
      workspaceRoot: null,
      overlayRoot: null,
      sidebarShadow: null,
      workspaceShadow: null,
      overlayShadow: null,
      unmount: null,
      uiReady: false,
      loadingWeft: false,
      weftCss: "",
      modeButton: null,
      savedModeButton: null,
      headerActionsMounted: false,
      inboxCount: 0,
      usedLengths: new Map(),
      commandHandlers: new Set(),
      viewHandlers: new Set(),
      notifiedThreadId: undefined,
      pendingPick: new Map(),
      mutationObserver: null,
      resizeObserver: null,
      observedPanels: new WeakSet(),
      mediaQuery: null,
      mountTimer: 0,
      disposed: false,
      started: false,
      clickListener: null,
      mediaListener: null,
    };

    function notifyHost(type, payload = {}) {
      const binding = window[config.bindingName];
      if (typeof binding !== "function") return;
      try {
        binding(JSON.stringify({ version: 1, type, ...payload }));
      } catch {
        // The launcher may be reconnecting.
      }
    }

    function weftdOrigin() {
      return new URL(config.webBaseUrl).origin;
    }

    function weftAsset(name) {
      return weftdOrigin() + "/web/" + name;
    }

    function modeButton() {
      const sidebar = document.querySelector("[data-app-action-sidebar-scroll]");
      const navigation = sidebar && sidebar.closest("nav");
      if (!navigation) return null;
      const candidates = [...navigation.querySelectorAll('button[aria-haspopup="menu"][aria-expanded][data-state]')]
        .filter((button) => !sidebar.contains(button));
      return candidates.length === 1 ? candidates[0] : null;
    }

    function modeLabelNode(button) {
      if (!(button instanceof HTMLElement)) return null;
      return button.querySelector("span");
    }

    function inferNativeMode(button) {
      if (!(button instanceof HTMLElement)) return "work";
      const saved = state.savedModeButton;
      const label = saved && saved.button === button
        ? saved.text
        : (button.textContent || button.getAttribute("aria-label") || "");
      return /codex/i.test(label) ? "codex" : "work";
    }

    function saveModeButton(button) {
      if (!(button instanceof HTMLElement)) return;
      if (state.savedModeButton && state.savedModeButton.button === button) return;
      const label = modeLabelNode(button);
      state.savedModeButton = {
        button,
        label,
        text: label ? label.textContent || "" : "",
        ariaLabel: button.getAttribute("aria-label"),
      };
      state.nativeMode = inferNativeMode(button);
    }

    function restoreModeButton() {
      const saved = state.savedModeButton;
      if (!saved) return;
      if (saved.button.isConnected) {
        if (saved.label && saved.label.isConnected) saved.label.textContent = saved.text;
        if (saved.ariaLabel === null) saved.button.removeAttribute("aria-label");
        else saved.button.setAttribute("aria-label", saved.ariaLabel);
      }
      state.savedModeButton = null;
    }

    function applyNativeModeButtonLabel(labelText) {
      const button = state.modeButton;
      if (!(button instanceof HTMLElement) || !labelText) return;
      const label = modeLabelNode(button);
      const previousText = label ? label.textContent || "" : "";
      if (label) label.textContent = labelText;
      const aria = button.getAttribute("aria-label");
      if (aria && previousText && aria.includes(previousText)) {
        button.setAttribute("aria-label", aria.replace(previousText, labelText));
      }
    }

    function applyWeftModeButton(button) {
      if (!(button instanceof HTMLElement)) return;
      saveModeButton(button);
      const label = modeLabelNode(button);
      if (label && label.textContent !== "Weft") label.textContent = "Weft";
      const locale = document.documentElement.lang || navigator.language || "en";
      const aria = locale.toLowerCase().startsWith("zh")
        ? "切换模式，当前模式：Weft"
        : "Switch mode, current mode: Weft";
      if (button.getAttribute("aria-label") !== aria) button.setAttribute("aria-label", aria);
    }

    function actionSlot(button) {
      if (!(button instanceof HTMLElement)) return null;
      const modeRow = button.parentElement;
      if (!(modeRow instanceof HTMLElement)) return null;
      const siblings = [...modeRow.children]
        .filter((child) => child instanceof HTMLElement && child !== button);
      return siblings.length === 1 ? siblings[0] : null;
    }

    function markModeHeader(button) {
      if (!(button instanceof HTMLElement)) return;
      const modeRow = button.parentElement;
      const header = modeRow && modeRow.parentElement;
      if (!(modeRow instanceof HTMLElement) || !(header instanceof HTMLElement)) return;
      modeRow.dataset.weftCodexModeHeader = "";
      for (const child of header.children) {
        if (!(child instanceof HTMLElement) || child === modeRow) continue;
        child.dataset.weftCodexNativeHeaderAction = "";
      }
      const slot = actionSlot(button);
      if (!(slot instanceof HTMLElement)) return;
      for (const child of slot.children) {
        if (!(child instanceof HTMLElement)) continue;
        if (child.hasAttribute(HEADER_ACTION_ATTR)) continue;
        child.dataset.weftCodexNativeHeaderAction = "";
      }
    }

    function localeIsChinese() {
      const locale = document.documentElement.lang || navigator.language || "en";
      return locale.toLowerCase().startsWith("zh");
    }

    const HEADER_ACTIONS = [
      {
        key: "search",
        command: "search.open",
        path: "M11 3a8 8 0 1 0 0 16 8 8 0 0 0 0-16ZM21 21l-4.35-4.35",
        label: { zh: "搜索 workspace", en: "Search workspace" },
      },
      {
        key: "inbox",
        command: "inbox.open",
        path: "M22 12h-6l-2 3h-4l-2-3H2M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11Z",
        label: { zh: "收件箱", en: "Inbox" },
      },
    ];

    function headerActionIcon(path) {
      const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      svg.setAttribute("width", "17");
      svg.setAttribute("height", "17");
      svg.setAttribute("viewBox", "0 0 24 24");
      svg.setAttribute("fill", "none");
      svg.setAttribute("stroke", "currentColor");
      svg.setAttribute("stroke-width", "2");
      svg.setAttribute("stroke-linecap", "round");
      svg.setAttribute("stroke-linejoin", "round");
      svg.setAttribute("aria-hidden", "true");
      svg.classList.add("icon-xs", "shrink-0");
      const node = document.createElementNS("http://www.w3.org/2000/svg", "path");
      node.setAttribute("d", path);
      svg.append(node);
      return svg;
    }

    function headerActionTemplate(slot, button) {
      const native = [...slot.querySelectorAll("button")]
        .find((candidate) => !candidate.hasAttribute(HEADER_ACTION_ATTR));
      if (native instanceof HTMLElement) return native;
      return button instanceof HTMLElement ? button : null;
    }

    function inboxLabel(base, count) {
      if (count <= 0) return base;
      if (localeIsChinese()) return base + "，" + count + " 项待处理";
      return base + ", " + count + " needing attention";
    }

    function applyInboxBadge() {
      const button = document.querySelector('[' + HEADER_ACTION_ATTR + '="inbox"]');
      if (!(button instanceof HTMLElement)) return;
      const action = HEADER_ACTIONS.find((candidate) => candidate.key === "inbox");
      const base = localeIsChinese() ? action.label.zh : action.label.en;
      button.setAttribute("aria-label", inboxLabel(base, state.inboxCount));
      let badge = button.querySelector("[" + HEADER_BADGE_ATTR + "]");
      if (state.inboxCount <= 0) {
        if (badge) badge.remove();
        return;
      }
      if (!(badge instanceof HTMLElement)) {
        badge = document.createElement("span");
        badge.setAttribute(HEADER_BADGE_ATTR, "");
        badge.setAttribute("aria-hidden", "true");
        button.append(badge);
      }
      const text = state.inboxCount > 99 ? "99+" : String(state.inboxCount);
      if (badge.textContent !== text) badge.textContent = text;
    }

    function dispatchHostCommand(command) {
      for (const handler of state.commandHandlers) {
        try { handler(command); } catch { /* React handler */ }
      }
    }

    function notifyView() {
      const threadId = activeThreadId();
      state.notifiedThreadId = threadId;
      for (const handler of state.viewHandlers) {
        try { handler(state.view, threadId); } catch { /* React handler */ }
      }
    }

    function createHeaderAction(template, action) {
      const clone = template.cloneNode(true);
      if (!(clone instanceof HTMLElement)) return null;
      clone.setAttribute(HEADER_ACTION_ATTR, action.key);
      clone.removeAttribute("data-weft-codex-native-header-action");
      clone.removeAttribute("aria-haspopup");
      clone.removeAttribute("aria-expanded");
      clone.removeAttribute("data-state");
      clone.removeAttribute("id");
      for (const identified of clone.querySelectorAll("[id]")) identified.removeAttribute("id");
      for (const svg of clone.querySelectorAll("svg")) svg.remove();
      for (const text of [...clone.childNodes]) {
        if (text.nodeType === Node.TEXT_NODE) text.remove();
      }
      for (const span of [...clone.querySelectorAll("span")]) {
        if (!span.hasAttribute(HEADER_BADGE_ATTR)) span.remove();
      }
      clone.setAttribute("type", "button");
      clone.setAttribute("aria-label", localeIsChinese() ? action.label.zh : action.label.en);
      clone.append(headerActionIcon(action.path));
      clone.addEventListener("click", (event) => {
        event.preventDefault();
        event.stopPropagation();
        dispatchHostCommand(action.command);
      });
      return clone;
    }

    function ensureHeaderActions(button) {
      const slot = actionSlot(button);
      if (!(slot instanceof HTMLElement)) {
        removeHeaderActions();
        state.headerActionsMounted = false;
        return;
      }
      const template = headerActionTemplate(slot, button);
      if (!(template instanceof HTMLElement)) {
        removeHeaderActions();
        state.headerActionsMounted = false;
        return;
      }
      for (const action of HEADER_ACTIONS) {
        const selector = '[' + HEADER_ACTION_ATTR + '="' + action.key + '"]';
        const existing = slot.querySelector(":scope > " + selector);
        if (existing instanceof HTMLElement) continue;
        for (const orphan of document.querySelectorAll(selector)) orphan.remove();
        const created = createHeaderAction(template, action);
        if (created) slot.append(created);
      }
      state.headerActionsMounted = true;
      applyInboxBadge();
    }

    function removeHeaderActions() {
      for (const node of document.querySelectorAll("[" + HEADER_ACTION_ATTR + "]")) node.remove();
    }

    function setInboxCount(value) {
      const count = Number.isFinite(value) && value > 0 ? Math.floor(value) : 0;
      if (state.inboxCount === count) return;
      state.inboxCount = count;
      applyInboxBadge();
    }

    function setDocumentState() {
      const root = document.documentElement;
      const values = {
        weftCodexMode: state.mode,
        weftCodexView: state.view,
        weftCodexTier: "weft-mode",
        weftCodexHeaderActions: state.headerActionsMounted ? "native" : "fallback",
        weftCodexCspBypass: state.cspBypass ? "true" : "false",
      };
      for (const [key, value] of Object.entries(values)) {
        if (root.dataset[key] !== value) root.dataset[key] = value;
      }
    }

    function setMode(nextMode, persist = true) {
      if (nextMode !== "work" && nextMode !== "codex" && nextMode !== "weft") return;
      const changed = state.mode !== nextMode;
      if (nextMode === "weft") {
        state.view = "workspace";
        state.mode = "weft";
        if (state.modeButton) applyWeftModeButton(state.modeButton);
      } else {
        unmountWeft();
        state.mode = nextMode;
        state.nativeMode = nextMode;
        state.view = "workspace";
        restoreModeButton();
      }
      setDocumentState();
      syncModeMenus();
      notifyView();
      if (changed && persist) notifyHost("mode.changed", { mode: nextMode });
      if (nextMode === "weft") void ensureWeftMounted();
    }

    function setView(nextView) {
      if (nextView !== "workspace" && nextView !== "thread") return;
      const threadId = activeThreadId();
      // Same view is not a no-op: opening another chat while already in
      // thread view must still tell React which row is active.
      if (state.view === nextView && state.notifiedThreadId === threadId) return;
      state.view = nextView;
      setDocumentState();
      notifyView();
    }

    function syncThreadView() {
      if (state.mode !== "weft" || state.view !== "thread") return;
      const threadId = activeThreadId();
      if (threadId === state.notifiedThreadId) return;
      notifyView();
    }

    function ensureShadow(root) {
      if (root.shadowRoot) return root.shadowRoot;
      return root.attachShadow({ mode: "open" });
    }

    function decorateHost(root) {
      if (!(root instanceof HTMLElement)) return;
      root.dataset.hostTheme = resolvedTheme();
      applyRadiusTokens(root);
    }

    function ensureSidebarRoot() {
      const sidebar = document.querySelector("[data-app-action-sidebar-scroll]");
      if (!(sidebar instanceof HTMLElement)) return false;
      let root = state.sidebarRoot;
      if (!(root instanceof HTMLElement)) {
        root = document.createElement("div");
        root.id = SIDEBAR_ROOT_ID;
        root.dataset.weftCodexHostSurface = "sidebar";
        state.sidebarRoot = root;
        state.sidebarShadow = ensureShadow(root);
      }
      if (root.parentElement !== sidebar) sidebar.append(root);
      decorateHost(root);
      return true;
    }

    ${VISIBLE_MAIN_HELPERS_SOURCE}

    /**
     * How much of the visible main Codex's own panels are occupying.
     *
     * Both the side panel and the bottom panel are rendered on demand and then
     * *stay* in the DOM, collapsing to an inline \`width: 0\` / \`height: 0\` box
     * when closed. Measuring the box is therefore the entire test — a closed
     * panel reads 0 without needing a separate open/closed signal, and a panel
     * Codex has never opened is simply absent and also reads 0. That is the
     * fail-open direction we want: when we cannot measure, we claim nothing.
     *
     * The panel is observed on the way past because it *animates*. A single
     * measurement taken when the inline style flips would freeze the workspace
     * at a mid-transition size; feeding the panel to the resize observer keeps
     * the geometry following it to rest, and picks up resizer drags for free.
     */
    function nativePanelSize(mainRoute, area, axis) {
      const panel = mainRoute.querySelector('[data-app-shell-focus-area="' + area + '"]');
      if (!(panel instanceof HTMLElement)) return 0;
      if (state.resizeObserver && !state.observedPanels.has(panel)) {
        state.observedPanels.add(panel);
        state.resizeObserver.observe(panel);
      }
      const rect = panel.getBoundingClientRect();
      return Math.max(0, Math.round(axis === "width" ? rect.width : rect.height));
    }

    function ensureWorkspaceRoot() {
      const mainRoute = visibleMainRoute();
      if (!(mainRoute instanceof HTMLElement)) return false;
      let root = state.workspaceRoot;
      if (!(root instanceof HTMLElement)) {
        root = document.createElement("div");
        root.id = WORKSPACE_ROOT_ID;
        root.dataset.weftCodexHostSurface = "workspace";
        state.workspaceRoot = root;
        state.workspaceShadow = ensureShadow(root);
      }
      if (root.parentElement !== mainRoute) mainRoute.append(root);
      const mainRect = mainRoute.getBoundingClientRect();
      const dragRegions = [...mainRoute.querySelectorAll("header, header *")].filter((element) =>
        getComputedStyle(element).getPropertyValue("-webkit-app-region") === "drag"
      );
      let top = 0;
      for (const region of dragRegions) {
        const rect = region.getBoundingClientRect();
        top = Math.max(top, rect.bottom - mainRect.top);
      }
      const topValue = Math.max(0, Math.round(top)) + "px";
      if (root.style.top !== topValue) root.style.top = topValue;
      const rightValue = nativePanelSize(mainRoute, "right-panel", "width") + "px";
      if (root.style.right !== rightValue) root.style.right = rightValue;
      const bottomValue = nativePanelSize(mainRoute, "bottom-panel", "height") + "px";
      if (root.style.bottom !== bottomValue) root.style.bottom = bottomValue;
      decorateHost(root);
      return true;
    }

    function ensureOverlayRoot() {
      let root = state.overlayRoot;
      if (!(root instanceof HTMLElement)) {
        root = document.createElement("div");
        root.id = OVERLAY_ROOT_ID;
        root.dataset.weftCodexHostSurface = "overlay";
        document.documentElement.append(root);
        state.overlayRoot = root;
        state.overlayShadow = ensureShadow(root);
      }
      if (!root.isConnected) document.documentElement.append(root);
      decorateHost(root);
      return true;
    }

    function adoptCss(shadow, cssText) {
      let style = shadow.querySelector("style[data-weft-codex-css]");
      if (!(style instanceof HTMLStyleElement)) {
        style = document.createElement("style");
        style.dataset.weftCodexCss = "";
        shadow.prepend(style);
      }
      if (style.textContent !== cssText) style.textContent = cssText;
    }

    function usedLength(value, rootFontSize) {
      if (!value || value.includes("%")) return value;
      const key = rootFontSize + "|" + value;
      const cached = state.usedLengths.get(key);
      if (cached !== undefined) return cached;
      const probe = document.createElement("div");
      probe.style.cssText = "position:absolute;visibility:hidden;pointer-events:none";
      probe.style.borderTopLeftRadius = value;
      document.documentElement.append(probe);
      const used = getComputedStyle(probe).borderTopLeftRadius || value;
      probe.remove();
      state.usedLengths.set(key, used);
      return used;
    }

    function applyRadiusTokens(root) {
      const style = getComputedStyle(document.documentElement);
      const rootFontSize = style.fontSize;
      for (const token of allowedTokens) {
        if (!token.startsWith("--radius")) continue;
        const value = style.getPropertyValue(token).trim();
        if (!value) continue;
        const px = usedLength(value, rootFontSize);
        root.style.setProperty(token, px);
        root.style.setProperty(token.replace("--radius-", "--r-"), px);
      }
    }

    function resolvedTheme() {
      const root = document.documentElement;
      if (root.classList.contains("electron-light")) return "light";
      if (root.classList.contains("electron-dark")) return "dark";
      const scheme = getComputedStyle(root).colorScheme.toLowerCase();
      return scheme.includes("light") && !scheme.includes("dark") ? "light" : "dark";
    }

    function loadScript(src) {
      const existing = document.querySelector('script[data-weft-codex-bundle="true"]');
      if (existing && window.WeftCodex && typeof window.WeftCodex.mountWeft === "function") {
        return Promise.resolve();
      }
      if (existing) existing.remove();
      return new Promise((resolve, reject) => {
        const script = document.createElement("script");
        script.src = src;
        script.dataset.weftCodexBundle = "true";
        script.onload = () => resolve();
        script.onerror = () => {
          script.remove();
          reject(new Error("Failed to load Weft bundle"));
        };
        document.documentElement.append(script);
      });
    }

    function createHost() {
      return {
        get locale() { return document.documentElement.lang || ""; },
        get view() { return state.view; },
        get threadId() { return activeThreadId(); },
        get weftdOrigin() { return weftdOrigin(); },
        get headerActions() { return state.headerActionsMounted ? "native" : "fallback"; },
        openThread(threadId) {
          return openNativeThread(threadId).then((ok) => {
            if (ok) return;
            notifyHost("thread.open.missing", { threadId });
            throw new Error("Thread is not in the Codex sidebar yet");
          });
        },
        showWorkspace() { setView("workspace"); },
        pickRepositories() {
          return new Promise((resolve, reject) => {
            const requestId = typeof crypto.randomUUID === "function"
              ? crypto.randomUUID()
              : String(Date.now());
            state.pendingPick.set(requestId, { resolve, reject });
            notifyHost("repositories.pick", { requestId });
          });
        },
        setInboxCount(count) { setInboxCount(count); },
        onCommand(handler) {
          state.commandHandlers.add(handler);
          return () => state.commandHandlers.delete(handler);
        },
        onView(handler) {
          state.viewHandlers.add(handler);
          handler(state.view, activeThreadId());
          return () => state.viewHandlers.delete(handler);
        },
      };
    }

    function unmountWeft() {
      if (typeof state.unmount === "function") {
        try { state.unmount(); } catch { /* already gone */ }
      }
      state.unmount = null;
      state.uiReady = false;
    }

    async function ensureWeftMounted() {
      if (state.disposed || state.mode !== "weft") return false;
      if (state.unmount) return true;
      if (!state.sidebarShadow || !state.workspaceShadow || !state.overlayShadow) return false;
      if (state.loadingWeft) return false;
      state.loadingWeft = true;
      try {
        if (!state.weftCss) {
          const response = await fetch(weftAsset("weft.css"));
          if (!response.ok) throw new Error("weft css " + response.status);
          state.weftCss = await response.text();
        }
        for (const root of [state.sidebarShadow, state.workspaceShadow, state.overlayShadow]) {
          adoptCss(root, state.weftCss);
        }
        await loadScript(weftAsset("weft.js"));
        const api = window.WeftCodex;
        if (!api || typeof api.mountWeft !== "function") throw new Error("WeftCodex.mountWeft missing");
        state.unmount = api.mountWeft({
          sidebar: state.sidebarShadow,
          main: state.workspaceShadow,
          overlay: state.overlayShadow,
          host: createHost(),
        });
        state.uiReady = true;
        notifyHost("ui.ready");
        return true;
      } catch (error) {
        notifyHost("ui.error", { error: String(error && error.message ? error.message : error) });
        return false;
      } finally {
        state.loadingWeft = false;
      }
    }

    function installStyles() {
      if (document.getElementById(STYLE_ID)) return;
      const style = document.createElement("style");
      style.id = STYLE_ID;
      style.textContent = \`
        #${SIDEBAR_ROOT_ID} {
          display: none;
          min-width: 0;
          min-height: 0;
          width: 100%;
        }
        #${WORKSPACE_ROOT_ID} {
          position: absolute;
          left: 0;
          right: 0;
          bottom: 0;
          z-index: 20;
          display: none;
          min-width: 0;
          min-height: 0;
          overflow: hidden;
          background: var(--color-token-main-surface-primary);
        }
        #${OVERLAY_ROOT_ID} {
          position: fixed;
          inset: 0;
          z-index: var(--weft-layer-modal, 10000);
          pointer-events: none;
        }
        html[data-weft-codex-tier="weft-mode"][data-weft-codex-mode="weft"] [data-app-action-sidebar-scroll] {
          gap: 0 !important;
          overflow: hidden !important;
          background: transparent !important;
        }
        html[data-weft-codex-tier="weft-mode"][data-weft-codex-mode="weft"] #${SIDEBAR_ROOT_ID} {
          display: flex;
          flex: 1 1 auto;
          height: 100%;
          background: transparent !important;
        }
        html[data-weft-codex-tier="weft-mode"][data-weft-codex-mode="weft"] [data-weft-codex-native-header-action] {
          display: none !important;
        }
        /* The header title belongs to the thread that is on stage, and on the
           workspace stage there is none — leaving it up hangs the last Lead's
           first line above the board. Hidden rather than removed: the header
           itself is the window drag band, and collapsing the box would take
           that band down with it. */
        html[data-weft-codex-tier="weft-mode"][data-weft-codex-mode="weft"][data-weft-codex-view="workspace"] [data-testid="app-shell-header-context-menu-surface"] {
          visibility: hidden;
        }
        html[data-weft-codex-tier="weft-mode"][data-weft-codex-mode="weft"] [data-app-action-sidebar-scroll] > :not(#${SIDEBAR_ROOT_ID}) {
          display: none !important;
        }
        html[data-weft-codex-mode="weft"][data-weft-codex-view="workspace"] #${WORKSPACE_ROOT_ID} {
          display: block;
        }
        html[data-weft-codex-mode="weft"] [${NATIVE_CHECK_ATTR}] {
          display: none !important;
        }
        html:not([data-weft-codex-tier="weft-mode"]) [${HEADER_ACTION_ATTR}],
        html:not([data-weft-codex-mode="weft"]) [${HEADER_ACTION_ATTR}] {
          display: none !important;
        }
        [${HEADER_ACTION_ATTR}] {
          position: relative;
        }
        [${HEADER_BADGE_ATTR}] {
          position: absolute;
          top: -1px;
          inset-inline-end: -1px;
          min-width: 14px;
          height: 14px;
          padding: 0 3px;
          border-radius: 7px;
          background: var(--color-token-primary);
          color: var(--color-token-button-foreground, var(--vscode-button-foreground, #fff));
          font-size: 9px;
          font-weight: 600;
          line-height: 14px;
          text-align: center;
          pointer-events: none;
        }
      \`;
      (document.head || document.documentElement).append(style);
    }

    function checkIcon() {
      const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      svg.dataset.weftCodexCheck = "";
      svg.setAttribute("width", "17");
      svg.setAttribute("height", "17");
      svg.setAttribute("viewBox", "0 0 17 17");
      svg.setAttribute("fill", "none");
      svg.setAttribute("aria-hidden", "true");
      svg.classList.add("icon-xs", "shrink-0", "opacity-75");
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", "M13.7 4.1a.7.7 0 0 1 .2 1l-6.2 8a.7.7 0 0 1-1 .1L3 9.8a.7.7 0 1 1 1-1l3.1 2.8 5.6-7.3a.7.7 0 0 1 1-.2Z");
      path.setAttribute("fill", "currentColor");
      svg.append(path);
      return svg;
    }

    function associatedModeMenu(menu) {
      if (!(menu instanceof HTMLElement) || menu.getAttribute("role") !== "menu") return false;
      const button = state.modeButton;
      if (!(button instanceof HTMLElement)) return false;
      return Boolean(button.id) && menu.getAttribute("aria-labelledby") === button.id;
    }

    function activateWeftMenuItem(event) {
      event.preventDefault();
      event.stopPropagation();
      if (typeof event.stopImmediatePropagation === "function") event.stopImmediatePropagation();
      const menu = event.currentTarget instanceof Element
        ? event.currentTarget.closest('[role="menu"]')
        : null;
      const nativeCodex = menu
        ? [...menu.querySelectorAll('[role="menuitem"]:not([' + MODE_ITEM_ATTR + '])')]
            .find((item) => /^Codex/i.test(item.textContent || ""))
        : null;
      // The live button is the source of truth. nativeMode used to start as
      // "codex" even when the host was still on ChatGPT, so the first Weft
      // click skipped the native switch and mounted on the wrong base.
      const onCodex = state.modeButton instanceof HTMLElement
        && inferNativeMode(state.modeButton) === "codex";
      if (nativeCodex instanceof HTMLElement && !onCodex) nativeCodex.click();
      state.nativeMode = "codex";
      setMode("weft");
      if (state.modeButton instanceof HTMLElement) state.modeButton.focus();
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true }));
    }

    function prepareMenuKeyboard(menu, item) {
      if (menu.dataset.weftCodexKeyboard === "true") return;
      menu.dataset.weftCodexKeyboard = "true";
      menu.addEventListener("keydown", (event) => {
        const nativeItems = [...menu.querySelectorAll('[role="menuitem"]:not([' + MODE_ITEM_ATTR + '])')];
        const lastNative = nativeItems[nativeItems.length - 1];
        if (event.key === "ArrowDown" && document.activeElement === lastNative) {
          event.preventDefault();
          item.focus();
          return;
        }
        if (event.key === "ArrowUp" && document.activeElement === item && lastNative instanceof HTMLElement) {
          event.preventDefault();
          lastNative.focus();
          return;
        }
        if ((event.key === "Enter" || event.key === " ") && document.activeElement === item) {
          activateWeftMenuItem(event);
        }
      }, true);
    }

    function createWeftMenuItem(menu) {
      const template = menu.querySelector('[role="menuitem"]');
      if (!(template instanceof HTMLElement)) return null;
      const item = template.cloneNode(true);
      if (!(item instanceof HTMLElement)) return null;
      item.setAttribute(MODE_ITEM_ATTR, "");
      item.setAttribute("role", "menuitem");
      item.setAttribute("tabindex", "-1");
      item.removeAttribute("aria-labelledby");
      for (const identified of item.querySelectorAll("[id]")) identified.removeAttribute("id");
      const leafSpans = [...item.querySelectorAll("span")].filter((span) => !span.querySelector("span"));
      if (leafSpans[0]) leafSpans[0].textContent = "Weft";
      const locale = (document.documentElement.lang || navigator.language || "en").toLowerCase();
      if (leafSpans[1]) {
        leafSpans[1].textContent = locale.startsWith("zh")
          ? "跨仓库推进 issue"
          : "Plan and ship across repos";
      }
      for (const svg of item.querySelectorAll("svg")) svg.remove();
      const content = item.firstElementChild;
      if (state.mode === "weft" && content) content.append(checkIcon());
      item.addEventListener("click", activateWeftMenuItem, true);
      item.addEventListener("pointerdown", (event) => event.stopPropagation(), true);
      return item;
    }

    function syncModeMenus() {
      for (const menu of document.querySelectorAll('[role="menu"]')) {
        if (!associatedModeMenu(menu)) continue;
        for (const nativeItem of menu.querySelectorAll('[role="menuitem"]:not([' + MODE_ITEM_ATTR + '])')) {
          const directIcon = nativeItem.firstElementChild && nativeItem.firstElementChild.querySelector(":scope > svg");
          if (directIcon) directIcon.setAttribute(NATIVE_CHECK_ATTR, "");
        }
        let item = menu.querySelector("[" + MODE_ITEM_ATTR + "]");
        if (!(item instanceof HTMLElement)) {
          item = createWeftMenuItem(menu);
          if (item) menu.append(item);
        }
        if (!(item instanceof HTMLElement)) continue;
        const content = item.firstElementChild;
        if (content) {
          const existingCheck = content.querySelector(":scope > svg[data-weft-codex-check]");
          if (state.mode === "weft" && !existingCheck) content.append(checkIcon());
          if (state.mode !== "weft" && existingCheck) existingCheck.remove();
        }
        prepareMenuKeyboard(menu, item);
      }
    }

    function activeThreadId() {
      const active = document.querySelector('[data-app-action-sidebar-thread-active="true"][data-app-action-sidebar-thread-id]');
      const value = active && active.getAttribute("data-app-action-sidebar-thread-id");
      if (!value) return undefined;
      const separator = value.indexOf(":");
      return separator >= 0 ? value.slice(separator + 1) : value;
    }

    function nativeThreadRow(threadId) {
      const rows = [...document.querySelectorAll("[data-app-action-sidebar-thread-id]")];
      return rows.find((candidate) => {
        const value = candidate.getAttribute("data-app-action-sidebar-thread-id") || "";
        return value === threadId || value.endsWith(":" + threadId);
      });
    }

    async function openNativeThread(threadId) {
      for (const delay of THREAD_OPEN_RETRY_DELAYS) {
        if (delay) await new Promise((resolve) => window.setTimeout(resolve, delay));
        if (state.disposed) return false;
        const row = nativeThreadRow(threadId);
        if (!(row instanceof HTMLElement)) continue;
        row.click();
        setView("thread");
        return true;
      }
      return false;
    }

    function onDocumentClick(event) {
      const target = event.target instanceof Element ? event.target.closest('[role="menuitem"]') : null;
      if (!(target instanceof HTMLElement) || target.hasAttribute(MODE_ITEM_ATTR)) return;
      const menu = target.closest('[role="menu"]');
      if (!associatedModeMenu(menu)) return;
      const text = target.textContent || "";
      const leafLabel = [...target.querySelectorAll("span")]
        .find((span) => !span.querySelector("span") && (span.textContent || "").trim());
      const selectedLabel = (leafLabel && leafLabel.textContent || "").trim();
      const nextMode = /codex/i.test(text) ? "codex" : "work";
      setMode(nextMode);
      applyNativeModeButtonLabel(selectedLabel || (nextMode === "codex" ? "Codex" : "ChatGPT"));
    }

    function mount() {
      if (state.disposed || !document.documentElement) return;
      installStyles();
      ensureSidebarRoot();
      ensureWorkspaceRoot();
      ensureOverlayRoot();
      const nextModeButton = modeButton();
      if (state.modeButton !== nextModeButton) {
        restoreModeButton();
        state.modeButton = nextModeButton;
      }
      if (state.mode === "weft") {
        if (nextModeButton) applyWeftModeButton(nextModeButton);
      } else if (nextModeButton) {
        const inferred = inferNativeMode(nextModeButton);
        state.mode = inferred;
        state.nativeMode = inferred;
      }
      if (nextModeButton) {
        markModeHeader(nextModeButton);
        ensureHeaderActions(nextModeButton);
      } else {
        removeHeaderActions();
        state.headerActionsMounted = false;
      }
      setDocumentState();
      syncModeMenus();
      syncThreadView();
      if (state.mode === "weft") void ensureWeftMounted();
    }

    function scheduleMount() {
      if (state.mountTimer || state.disposed) return;
      state.mountTimer = window.setTimeout(() => {
        state.mountTimer = 0;
        mount();
      }, 0);
    }

    function start() {
      if (state.started || state.disposed || !document.documentElement) return;
      state.started = true;
      state.clickListener = onDocumentClick;
      document.addEventListener("click", state.clickListener, true);
      state.mutationObserver = new MutationObserver(() => scheduleMount());
      state.mutationObserver.observe(document.documentElement, {
        attributes: true,
        attributeFilter: ["class", "lang", "style", "inert", "aria-hidden", "hidden"],
        childList: true,
        subtree: true,
      });
      state.resizeObserver = new ResizeObserver(() => scheduleMount());
      state.resizeObserver.observe(document.documentElement);
      state.mediaQuery = matchMedia("(prefers-color-scheme: dark)");
      state.mediaListener = () => scheduleMount();
      state.mediaQuery.addEventListener("change", state.mediaListener);
      mount();
      notifyHost("agent.ready");
    }

    function reloadUi() {
      unmountWeft();
      state.weftCss = "";
      const bundle = document.querySelector('script[data-weft-codex-bundle="true"]');
      if (bundle) bundle.remove();
      void ensureWeftMounted();
    }

    function setCspBypass(enabled) {
      state.cspBypass = Boolean(enabled);
      setDocumentState();
      reloadUi();
    }

    function deliverActionResult(requestId, response) {
      const pending = state.pendingPick.get(requestId);
      if (!pending) return false;
      state.pendingPick.delete(requestId);
      if (response && response.ok === true) {
        const paths = response.result && Array.isArray(response.result.paths) ? response.result.paths : [];
        pending.resolve(paths);
      } else {
        pending.reject(new Error(response && response.error ? response.error : "picker failed"));
      }
      return true;
    }

    function status() {
      return {
        version: 1,
        mode: state.mode,
        view: state.view,
        tier: "weft-mode",
        cspBypass: state.cspBypass,
        uiMounted: Boolean(
          state.sidebarRoot && state.sidebarRoot.isConnected
          && state.workspaceRoot && state.workspaceRoot.isConnected
          && state.overlayRoot && state.overlayRoot.isConnected
        ),
        uiReady: Boolean(state.uiReady),
        nativeModeSwitcher: Boolean(state.modeButton),
        headerActions: state.headerActionsMounted ? "native" : "fallback",
        inboxCount: state.inboxCount,
      };
    }

    function dispose() {
      if (state.disposed) return;
      state.disposed = true;
      if (state.mountTimer) window.clearTimeout(state.mountTimer);
      if (state.mutationObserver) state.mutationObserver.disconnect();
      if (state.resizeObserver) state.resizeObserver.disconnect();
      if (state.mediaQuery && state.mediaListener) state.mediaQuery.removeEventListener("change", state.mediaListener);
      if (state.clickListener) document.removeEventListener("click", state.clickListener, true);
      unmountWeft();
      state.pendingPick.clear();
      state.commandHandlers.clear();
      state.viewHandlers.clear();
      restoreModeButton();
      removeHeaderActions();
      if (state.sidebarRoot) state.sidebarRoot.remove();
      if (state.workspaceRoot) state.workspaceRoot.remove();
      if (state.overlayRoot) state.overlayRoot.remove();
      const style = document.getElementById(STYLE_ID);
      if (style) style.remove();
      const bundle = document.querySelector('script[data-weft-codex-bundle="true"]');
      if (bundle) bundle.remove();
      const root = document.documentElement;
      delete root.dataset.weftCodexMode;
      delete root.dataset.weftCodexView;
      delete root.dataset.weftCodexTier;
      delete root.dataset.weftCodexHeaderActions;
      delete root.dataset.weftCodexCspBypass;
      for (const element of document.querySelectorAll("[data-weft-codex-mode-header]")) {
        element.removeAttribute("data-weft-codex-mode-header");
      }
      for (const element of document.querySelectorAll("[data-weft-codex-native-header-action]")) {
        element.removeAttribute("data-weft-codex-native-header-action");
      }
    }

    window[GLOBAL_KEY] = {
      version: 1,
      status,
      setMode,
      setCspBypass,
      reloadFrames: reloadUi,
      deliverActionResult,
      dispose,
    };
    if (document.documentElement) start();
    else document.addEventListener("readystatechange", start, { once: true });
  })();`
}
