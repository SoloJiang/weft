import * as Collapsible from "@radix-ui/react-collapsible";
import { ChevronRightIcon, FileIcon, FolderIcon, FolderOpenIcon } from "lucide-react";
import type { HTMLAttributes, KeyboardEvent, ReactNode, SyntheticEvent } from "react";
import { createContext, useCallback, useContext, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { FileNode as AppFileNode } from "../lib/types";
import { cn } from "../lib/cn";

type FileTreeContextType = {
  readonly expandedPaths: Set<string>;
  readonly setPathExpanded: (path: string, expanded: boolean) => void;
  readonly selectedPath?: string;
  readonly onSelect?: (path: string) => void;
};

const noopExpand = (_path: string, _expanded: boolean) => {};
const FileTreeContext = createContext<FileTreeContextType>({
  expandedPaths: new Set<string>(),
  setPathExpanded: noopExpand,
});

export type FileTreeProps = Omit<HTMLAttributes<HTMLDivElement>, "onSelect"> & {
  expanded?: Set<string>;
  defaultExpanded?: Set<string>;
  selectedPath?: string;
  onSelect?: (path: string) => void;
  onExpandedChange?: (expanded: Set<string>) => void;
};

export const FileTree = ({
  expanded: controlledExpanded,
  defaultExpanded = new Set<string>(),
  selectedPath,
  onSelect,
  onExpandedChange,
  className,
  children,
  ...props
}: FileTreeProps) => {
  const { t } = useTranslation();
  const [internalExpanded, setInternalExpanded] = useState(() => new Set(defaultExpanded));
  const expandedPaths = controlledExpanded ?? internalExpanded;
  const isControlled = controlledExpanded !== undefined;
  const setPathExpanded = useCallback(
    (path: string, expanded: boolean) => {
      const nextExpanded = new Set(expandedPaths);
      if (expanded) {
        nextExpanded.add(path);
      } else {
        nextExpanded.delete(path);
      }
      if (!isControlled) setInternalExpanded(nextExpanded);
      onExpandedChange?.(nextExpanded);
    },
    [expandedPaths, isControlled, onExpandedChange],
  );
  const contextValue = useMemo(
    () => ({ expandedPaths, onSelect, selectedPath, setPathExpanded }),
    [expandedPaths, onSelect, selectedPath, setPathExpanded],
  );

  return (
    <FileTreeContext.Provider value={contextValue}>
      <div
        role="tree"
        aria-label={t("files.treeLabel")}
        className={cn("rounded-[var(--radius-lg)] border border-border bg-bg font-mono text-[13px] text-ink", className)}
        {...props}
      >
        <div className="p-1.5">{children}</div>
      </div>
    </FileTreeContext.Provider>
  );
};

export type FileTreeIconProps = HTMLAttributes<HTMLSpanElement>;

export const FileTreeIcon = ({ className, children, ...props }: FileTreeIconProps) => (
  <span className={cn("inline-flex h-4 w-4 shrink-0 items-center justify-center", className)} {...props}>
    {children}
  </span>
);

export type FileTreeNameProps = HTMLAttributes<HTMLSpanElement>;

export const FileTreeName = ({ className, children, ...props }: FileTreeNameProps) => (
  <span className={cn("min-w-0 flex-1 truncate", className)} {...props}>
    {children}
  </span>
);

export type FileTreeFolderProps = HTMLAttributes<HTMLDivElement> & {
  path: string;
  name: string;
};

export const FileTreeFolder = ({ path, name, className, children, ...props }: FileTreeFolderProps) => {
  const { expandedPaths, setPathExpanded, selectedPath, onSelect } = useContext(FileTreeContext);
  const isExpanded = expandedPaths.has(path);
  const isSelected = selectedPath === path;
  const setExpanded = useCallback(
    (expanded: boolean) => setPathExpanded(path, expanded),
    [path, setPathExpanded],
  );
  const handleSelect = useCallback(() => onSelect?.(path), [onSelect, path]);
  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key === "ArrowRight") {
        event.preventDefault();
        if (!isExpanded) setExpanded(true);
        return;
      }
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        if (isExpanded) setExpanded(false);
        return;
      }
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        handleSelect();
      }
    },
    [handleSelect, isExpanded, setExpanded],
  );

  return (
    <Collapsible.Root open={isExpanded} onOpenChange={setExpanded}>
      <div className={cn("outline-none", className)} {...props}>
        <div
          role="treeitem"
          aria-expanded={isExpanded}
          aria-selected={isSelected}
          data-selected={isSelected}
          tabIndex={0}
          onClick={handleSelect}
          onKeyDown={handleKeyDown}
          className={fileTreeRowClass}
        >
          <FolderTrigger expanded={isExpanded} />
          <FileTreeIcon>
            <FolderGlyph expanded={isExpanded} />
          </FileTreeIcon>
          <FileTreeName>{name}</FileTreeName>
        </div>
        <Collapsible.Content role="group">
          <div className="ml-3.5 border-l border-border pl-2">{children}</div>
        </Collapsible.Content>
      </div>
    </Collapsible.Root>
  );
};

export type FileTreeFileProps = HTMLAttributes<HTMLDivElement> & {
  path: string;
  name: string;
  icon?: ReactNode;
};

export const FileTreeFile = ({ path, name, icon, className, children, ...props }: FileTreeFileProps) => {
  const { selectedPath, onSelect } = useContext(FileTreeContext);
  const isSelected = selectedPath === path;
  const handleSelect = useCallback(() => onSelect?.(path), [onSelect, path]);
  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        handleSelect();
      }
    },
    [handleSelect],
  );

  return (
    <div
      role="treeitem"
      aria-selected={isSelected}
      data-selected={isSelected}
      tabIndex={0}
      className={cn(fileTreeRowClass, className)}
      onClick={handleSelect}
      onKeyDown={handleKeyDown}
      {...props}
    >
      {children ?? <FileTreeFileContents icon={icon} name={name} />}
    </div>
  );
};

export type FileTreeActionsProps = HTMLAttributes<HTMLDivElement>;

export const FileTreeActions = ({ className, children, ...props }: FileTreeActionsProps) => (
  <div className={cn("ml-auto flex items-center gap-1", className)} onClick={stopPropagation} onKeyDown={stopPropagation} {...props}>
    {children}
  </div>
);

export type FileTreeNodeProps = {
  nodes: FileNode[];
};

export type FileNode = AppFileNode;

export function FileTreeNodes({ nodes }: FileTreeNodeProps) {
  return nodes.map(renderFileTreeNode);
}

const fileTreeRowClass = cn(
  "flex w-full cursor-pointer items-center gap-1.5 rounded-[var(--radius-md)] px-2 py-1 text-left",
  "transition-colors duration-150 ease-[var(--ease-out-quint)] outline-none",
  "data-[selected=false]:text-ink data-[selected=false]:hover:bg-surface",
  "data-[selected=true]:bg-brand-ghost data-[selected=true]:text-brand",
  "focus-visible:bg-surface focus-visible:ring-2 focus-visible:ring-brand/30",
);

function renderFileTreeNode(node: FileNode) {
  if (node.kind === "directory") {
    return (
      <FileTreeFolder key={node.path} path={node.path} name={node.name}>
        <FileTreeNodes nodes={node.children ?? []} />
      </FileTreeFolder>
    );
  }
  return <FileTreeFile key={node.path} path={node.path} name={node.name} />;
}

function FolderTrigger({ expanded }: { expanded: boolean }) {
  const { t } = useTranslation();
  return (
    <Collapsible.Trigger asChild>
      <button
        type="button"
        className="grid h-4 w-4 shrink-0 place-items-center rounded-[var(--radius-sm)] text-ink-faint transition-[color,transform] duration-150 ease-[var(--ease-out-quint)] hover:text-ink"
        aria-label={expanded ? t("files.collapseFolder") : t("files.expandFolder")}
        onClick={stopPropagation}
      >
        <ChevronRightIcon
          size={14}
          className={cn(
            "transition-transform duration-150 ease-[var(--ease-out-quint)] motion-reduce:transition-none",
            expanded && "rotate-90",
          )}
        />
      </button>
    </Collapsible.Trigger>
  );
}

function FileTreeFileContents({ icon, name }: { icon?: ReactNode; name: string }) {
  return (
    <>
      <span className="h-4 w-4 shrink-0" />
      <FileTreeIcon>{icon ?? <FileIcon size={14} className="shrink-0 text-ink-faint" />}</FileTreeIcon>
      <FileTreeName>{name}</FileTreeName>
    </>
  );
}

function FolderGlyph({ expanded }: { expanded: boolean }) {
  if (expanded) return <FolderOpenIcon size={15} className="shrink-0 text-brand" />;
  return <FolderIcon size={15} className="shrink-0 text-brand" />;
}

function stopPropagation(event: SyntheticEvent) {
  event.stopPropagation();
}
