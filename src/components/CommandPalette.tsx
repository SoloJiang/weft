import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  CircleDot,
  CornerDownLeft,
  HelpCircle,
  LayoutDashboard,
  Network,
  PanelLeft,
  Plus,
  Search,
  Settings,
  SquarePen,
  SunMoon,
} from "lucide-react";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";
import { CreateThreadDialog, CreateWorkspaceDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

type Command = {
  key: string;
  group: string;
  label: string;
  icon: React.ReactNode;
  run: () => void;
  /** Keyboard shortcut hint (e.g. "⌘1"), shown right-aligned for discovery. */
  hint?: string;
};

/** Open the palette from anywhere (e.g. the rail's search trigger). */
export function openCommandPalette() {
  window.dispatchEvent(new Event("weft:open-palette"));
}

/**
 * ⌘K / Ctrl+K command palette — the silky cross-app jump (§ navigation unify).
 * One keystroke to reach any issue or workspace surface without hunting the
 * sidebar. Self-contained: a capture-phase window listener owns the hotkey (so
 * it beats any focused input), arrow/Enter drive selection.
 */
export function CommandPalette() {
  const { t } = useTranslation();
  const {
    threads,
    selectThread,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    openNeeds,
    navCollapsed,
    setNavCollapsed,
    activeWorkspaceId,
  } = useStore();
  const { toggle: toggleTheme } = useTheme();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  // The palette owns its dialogs (it's always mounted, unlike the rail which
  // unmounts when collapsed), so actions work regardless of sidebar state.
  const [dialog, setDialog] = useState<null | "ws" | "thread">(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const activeRef = useRef<HTMLButtonElement>(null);

  // Global hotkey in capture phase so it fires before a focused terminal grabs
  // the key. ⌘/Ctrl+K toggles; we own the ⌘ prefix (§4.3 key ownership).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        e.stopPropagation();
        setOpen((v) => !v);
      }
    };
    const onOpen = () => setOpen(true);
    window.addEventListener("keydown", onKey, { capture: true });
    window.addEventListener("weft:open-palette", onOpen);
    return () => {
      window.removeEventListener("keydown", onKey, { capture: true });
      window.removeEventListener("weft:open-palette", onOpen);
    };
  }, []);

  // Reset query/selection and focus the field whenever it opens.
  useEffect(() => {
    if (open) {
      setQuery("");
      setSelected(0);
      // focus after the element mounts
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  const commands = useMemo<Command[]>(() => {
    const issues: Command[] = threads.map((th) => ({
      key: `issue-${th.id}`,
      group: t("palette.issue"),
      label: th.title,
      icon: <CircleDot size={14} />,
      run: () => selectThread(th.id),
    }));
    const nav: Command[] = [
      {
        key: "nav-needs",
        group: t("palette.go"),
        label: t("needs.title"),
        icon: <HelpCircle size={14} />,
        run: () => openNeeds(),
      },
      {
        key: "nav-board",
        group: t("palette.go"),
        label: t("palette.board"),
        icon: <LayoutDashboard size={14} />,
        hint: "⌘1",
        run: () => {
          backToWorkspace();
          setHomeTab("board");
        },
      },
      {
        key: "nav-repos",
        group: t("palette.go"),
        label: t("palette.repos"),
        icon: <Network size={14} />,
        hint: "⌘2",
        run: () => openRepoMap(),
      },
      {
        key: "nav-sidebar",
        group: t("palette.go"),
        label: t("palette.toggleSidebar"),
        icon: <PanelLeft size={14} />,
        hint: "⌘\\",
        run: () => setNavCollapsed(!navCollapsed),
      },
    ];
    const actions: Command[] = [
      ...(activeWorkspaceId != null
        ? [
            {
              key: "act-issue",
              group: t("palette.action"),
              label: t("nav.newThread"),
              icon: <SquarePen size={14} />,
              run: () => setDialog("thread"),
            },
          ]
        : []),
      {
        key: "act-workspace",
        group: t("palette.action"),
        label: t("nav.newWorkspace"),
        icon: <Plus size={14} />,
        run: () => setDialog("ws"),
      },
      {
        key: "act-theme",
        group: t("palette.action"),
        label: t("palette.theme"),
        icon: <SunMoon size={14} />,
        run: () => toggleTheme(),
      },
      {
        key: "act-settings",
        group: t("palette.action"),
        label: t("settings.title"),
        icon: <Settings size={14} />,
        run: () => {
          backToWorkspace();
          setHomeTab("settings");
        },
      },
    ];
    return [...issues, ...nav, ...actions];
  }, [
    threads,
    selectThread,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    openNeeds,
    navCollapsed,
    setNavCollapsed,
    activeWorkspaceId,
    toggleTheme,
    t,
  ]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return commands;
    return commands.filter((c) => c.label.toLowerCase().includes(q));
  }, [commands, query]);

  // Keep the highlighted index inside the filtered range.
  const active = filtered.length ? Math.min(selected, filtered.length - 1) : 0;

  // Scroll the highlighted command into view when arrowing past the fold.
  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest" });
  }, [active, open]);

  function close() {
    setOpen(false);
  }

  function runAt(i: number) {
    const cmd = filtered[i];
    if (!cmd) return;
    close();
    cmd.run();
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelected((s) => (filtered.length ? (Math.min(s, filtered.length - 1) + 1) % filtered.length : 0));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelected((s) =>
        filtered.length ? (Math.min(s, filtered.length - 1) + filtered.length - 1) % filtered.length : 0,
      );
    } else if (e.key === "Enter") {
      e.preventDefault();
      runAt(active);
    }
  }

  // Group consecutive commands by their group label for section headers.
  let lastGroup = "";

  return (
    <>
      {open && (
        <div className="fixed inset-0 z-[90]">
          <div
            className="weft-overlay absolute inset-0 bg-black/55 backdrop-blur-[1px]"
            data-state="open"
            onClick={close}
          />
      <div className="absolute inset-x-0 top-[14vh] flex justify-center px-4">
        <div
          className="weft-pop flex max-h-[60vh] w-[min(560px,calc(100vw-2rem))] flex-col overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface shadow-[0_16px_48px_-12px_rgba(0,0,0,0.6)]"
          data-state="open"
        >
          <div className="flex items-center gap-2.5 border-b border-border px-3.5">
            <Search size={15} className="shrink-0 text-ink-faint" />
            <input
              ref={inputRef}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setSelected(0);
              }}
              onKeyDown={onKeyDown}
              placeholder={t("palette.placeholder")}
              className="h-11 flex-1 bg-transparent text-[13.5px] text-ink outline-none placeholder:text-ink-faint"
            />
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
            {filtered.length === 0 ? (
              <div className="px-3 py-6 text-center text-[12.5px] text-ink-faint">
                {t("palette.empty")}
              </div>
            ) : (
              filtered.map((c, i) => {
                const showHeader = c.group !== lastGroup;
                lastGroup = c.group;
                return (
                  <div key={c.key}>
                    {showHeader && (
                      <div className="px-2.5 pb-1 pt-2 text-[10.5px] font-medium uppercase tracking-wide text-ink-faint">
                        {c.group}
                      </div>
                    )}
                    <button
                      type="button"
                      ref={i === active ? activeRef : undefined}
                      onClick={() => runAt(i)}
                      onMouseMove={() => setSelected(i)}
                      className={cn(
                        "flex w-full items-center gap-2.5 rounded-[var(--radius-sm)] px-2.5 py-2 text-left text-[13px] outline-none transition-colors",
                        i === active
                          ? "bg-brand-ghost text-ink"
                          : "text-ink-muted hover:bg-brand-ghost/60",
                      )}
                    >
                      <span className="text-ink-faint">{c.icon}</span>
                      <span className="min-w-0 flex-1 truncate">{c.label}</span>
                      {c.hint && (
                        <kbd className="shrink-0 rounded border border-border bg-surface px-1.5 py-px font-mono text-[10px] text-ink-faint">
                          {c.hint}
                        </kbd>
                      )}
                      {i === active && (
                        <CornerDownLeft size={12} className="shrink-0 text-ink-faint" />
                      )}
                    </button>
                  </div>
                );
              })
            )}
          </div>
        </div>
      </div>
    </div>
      )}

      <CreateThreadDialog open={dialog === "thread"} onOpenChange={(o) => !o && setDialog(null)} />
      <CreateWorkspaceDialog open={dialog === "ws"} onOpenChange={(o) => !o && setDialog(null)} />
    </>
  );
}
