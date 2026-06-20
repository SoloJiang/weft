import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, FolderTree, RefreshCw } from "lucide-react";
import { api } from "../lib/api";
import type { FileNode, FileTree } from "../lib/types";
import { FileTree as FileTreeRoot, FileTreeNodes } from "../components/FileTree";

const POLL_MS = 10000;

/**
 * Browse the worktree's directory structure as a collapsible file tree.
 * Selecting a file opens it in the OS default app; folders expand/collapse.
 */
export function FileTreeView({
  cwd,
  open = true,
}: {
  cwd: string;
  open?: boolean;
}) {
  const { t } = useTranslation();
  const [tree, setTree] = useState<FileTree | null>(null);
  const [selectedPath, setSelectedPath] = useState<string | undefined>();
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    let inFlight = false;
    const tick = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const next = await api.listWorktreeFiles(cwd);
        if (alive) {
          setTree(next);
          setLoaded(true);
          setError(null);
        }
      } catch (e) {
        if (alive) {
          setError(String(e));
          setLoaded(true);
        }
      } finally {
        inFlight = false;
      }
    };
    setTree(null);
    setLoaded(false);
    setError(null);
    void tick();
    const h = setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [cwd, open]);

  const nodeByPath = useMemo(() => buildNodeMap(tree?.nodes ?? []), [tree]);

  const handleSelect = async (path: string) => {
    setSelectedPath(path);
    const node = nodeByPath.get(path);
    if (node?.kind === "file") {
      try {
        await api.openFile(path);
      } catch {
        /* openFile already surfaces OS errors; ignore here */
      }
    }
  };

  const refresh = async () => {
    setLoaded(false);
    try {
      const next = await api.listWorktreeFiles(cwd);
      setTree(next);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoaded(true);
    }
  };

  const isEmpty = loaded && tree && tree.nodes.length === 0 && !error;

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto">
      <div className="sticky top-0 z-10 flex items-center justify-between border-b border-border bg-bg/95 px-4 py-2.5 backdrop-blur">
        <div className="flex items-center gap-2 text-[12px] font-semibold text-ink">
          <FolderTree size={13} className="text-ink-faint" />
          {t("files.tab")}
          {tree && tree.total > 0 && (
            <span className="text-[11px] font-normal text-ink-faint">
              {tree.total.toLocaleString()}
            </span>
          )}
        </div>
        <button
          onClick={() => void refresh()}
          title={t("files.refresh")}
          aria-label={t("files.refresh")}
          className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-surface hover:text-ink"
        >
          <RefreshCw size={12} />
        </button>
      </div>

      {tree?.truncated && (
        <div className="flex items-center gap-2 border-b border-border bg-waiting/10 px-4 py-2 text-[11px] text-waiting">
          <AlertTriangle size={12} className="shrink-0" />
          {t("files.truncated")}
        </div>
      )}

      {!loaded ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center">
          <p className="text-[12px] leading-relaxed text-ink-faint">
            {t("files.loading")}
          </p>
        </div>
      ) : error ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center">
          <p className="text-[12px] leading-relaxed text-danger">{error}</p>
        </div>
      ) : isEmpty ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center">
          <p className="text-[12px] leading-relaxed text-ink-faint">
            {t("files.empty")}
          </p>
        </div>
      ) : (
        <div className="p-3">
          <FileTreeRoot
            selectedPath={selectedPath}
            onSelect={handleSelect}
            className="border-none bg-transparent"
          >
            <FileTreeNodes nodes={tree?.nodes ?? []} />
          </FileTreeRoot>
        </div>
      )}
    </div>
  );
}

function buildNodeMap(nodes: FileNode[]): Map<string, FileNode> {
  const map = new Map<string, FileNode>();
  const walk = (list: FileNode[]) => {
    for (const node of list) {
      map.set(node.path, node);
      if (node.children) walk(node.children);
    }
  };
  walk(nodes);
  return map;
}
