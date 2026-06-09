import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Activity,
  CircleDot,
  CornerDownLeft,
  LayoutDashboard,
  Network,
  PanelLeft,
  Search,
} from "lucide-react";
import { useStore } from "../state/store";
import { cn } from "../lib/cn";

type Command = {
  key: string;
  group: string;
  label: string;
  icon: React.ReactNode;
  run: () => void;
};

/**
 * ⌘K / Ctrl+K command palette — the silky cross-app jump (§ navigation unify).
 * One keystroke to reach any issue or workspace surface without hunting the
 * sidebar. Self-contained: a capture-phase window listener owns the hotkey (so
 * it beats xterm in a focused session), arrow/Enter drive selection.
 */
export function CommandPalette() {
  const { t } = useTranslation();
  const {
    threads,
    selectThread,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    navCollapsed,
    setNavCollapsed,
  } = useStore();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

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
    window.addEventListener("keydown", onKey, { capture: true });
    return () => window.removeEventListener("keydown", onKey, { capture: true });
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
        key: "nav-board",
        group: t("palette.go"),
        label: t("palette.board"),
        icon: <LayoutDashboard size={14} />,
        run: () => {
          backToWorkspace();
          setHomeTab("board");
        },
      },
      {
        key: "nav-activity",
        group: t("palette.go"),
        label: t("palette.activity"),
        icon: <Activity size={14} />,
        run: () => {
          backToWorkspace();
          setHomeTab("overview");
        },
      },
      {
        key: "nav-repos",
        group: t("palette.go"),
        label: t("palette.repos"),
        icon: <Network size={14} />,
        run: () => openRepoMap(),
      },
      {
        key: "nav-sidebar",
        group: t("palette.go"),
        label: t("palette.toggleSidebar"),
        icon: <PanelLeft size={14} />,
        run: () => setNavCollapsed(!navCollapsed),
      },
    ];
    return [...issues, ...nav];
  }, [
    threads,
    selectThread,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    navCollapsed,
    setNavCollapsed,
    t,
  ]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return commands;
    return commands.filter((c) => c.label.toLowerCase().includes(q));
  }, [commands, query]);

  // Keep the highlighted index inside the filtered range.
  const active = filtered.length ? Math.min(selected, filtered.length - 1) : 0;

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

  if (!open) return null;

  // Group consecutive commands by their group label for section headers.
  let lastGroup = "";

  return (
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
  );
}
