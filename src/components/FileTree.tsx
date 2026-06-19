import * as Collapsible from "@radix-ui/react-collapsible";
import {
  ChevronRightIcon,
  FileIcon,
  FolderIcon,
  FolderOpenIcon,
} from "lucide-react";
import type { HTMLAttributes, ReactNode } from "react";
import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
} from "react";
import { cn } from "../lib/cn";

interface FileTreeContextType {
  expandedPaths: Set<string>;
  togglePath: (path: string) => void;
  selectedPath?: string;
  onSelect?: (path: string) => void;
}

const noop = () => {};

const FileTreeContext = createContext<FileTreeContextType>({
  expandedPaths: new Set(),
  togglePath: noop,
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
  defaultExpanded = new Set(),
  selectedPath,
  onSelect,
  onExpandedChange,
  className,
  children,
  ...props
}: FileTreeProps) => {
  const [internalExpanded, setInternalExpanded] = useState(defaultExpanded);
  const expandedPaths = controlledExpanded ?? internalExpanded;

  const togglePath = useCallback(
    (path: string) => {
      const newExpanded = new Set(expandedPaths);
      if (newExpanded.has(path)) {
        newExpanded.delete(path);
      } else {
        newExpanded.add(path);
      }
      setInternalExpanded(newExpanded);
      onExpandedChange?.(newExpanded);
    },
    [expandedPaths, onExpandedChange],
  );

  const contextValue = useMemo(
    () => ({ expandedPaths, onSelect, selectedPath, togglePath }),
    [expandedPaths, onSelect, selectedPath, togglePath],
  );

  return (
    <FileTreeContext.Provider value={contextValue}>
      <div
        className={cn(
          "rounded-[var(--radius-lg)] border border-border bg-bg font-mono text-[13px]",
          className,
        )}
        role="tree"
        {...props}
      >
        <div className="p-1.5">{children}</div>
      </div>
    </FileTreeContext.Provider>
  );
};

export type FileTreeIconProps = HTMLAttributes<HTMLSpanElement>;

export const FileTreeIcon = ({
  className,
  children,
  ...props
}: FileTreeIconProps) => (
  <span className={cn("shrink-0", className)} {...props}>
    {children}
  </span>
);

export type FileTreeNameProps = HTMLAttributes<HTMLSpanElement>;

export const FileTreeName = ({
  className,
  children,
  ...props
}: FileTreeNameProps) => (
  <span className={cn("truncate", className)} {...props}>
    {children}
  </span>
);

interface FileTreeFolderContextType {
  path: string;
  name: string;
  isExpanded: boolean;
}

const FileTreeFolderContext = createContext<FileTreeFolderContextType>({
  isExpanded: false,
  name: "",
  path: "",
});

export type FileTreeFolderProps = HTMLAttributes<HTMLDivElement> & {
  path: string;
  name: string;
};

export const FileTreeFolder = ({
  path,
  name,
  className,
  children,
  ...props
}: FileTreeFolderProps) => {
  const { expandedPaths, togglePath, selectedPath, onSelect } =
    useContext(FileTreeContext);
  const isExpanded = expandedPaths.has(path);
  const isSelected = selectedPath === path;

  const handleOpenChange = useCallback(() => {
    togglePath(path);
  }, [togglePath, path]);

  const handleSelect = useCallback(() => {
    onSelect?.(path);
  }, [onSelect, path]);

  const folderContextValue = useMemo(
    () => ({ isExpanded, name, path }),
    [isExpanded, name, path],
  );

  return (
    <FileTreeFolderContext.Provider value={folderContextValue}>
      <Collapsible.Root open={isExpanded} onOpenChange={handleOpenChange}>
        <div
          className={cn("outline-none focus-visible:ring-0", className)}
          role="treeitem"
          aria-expanded={isExpanded}
          tabIndex={-1}
          {...props}
        >
          <div
            className={cn(
              "flex w-full items-center gap-1 rounded-[var(--radius-md)] px-2 py-1 text-left transition-colors",
              isSelected
                ? "bg-brand-ghost text-brand"
                : "hover:bg-surface text-ink",
            )}
          >
            <Collapsible.Trigger asChild>
              <button
                type="button"
                className="grid h-4 w-4 shrink-0 place-items-center rounded text-ink-faint transition-transform hover:text-ink"
                aria-label={isExpanded ? "Collapse folder" : "Expand folder"}
              >
                <ChevronRightIcon
                  size={14}
                  className={cn(
                    "transition-transform duration-150 ease-out",
                    isExpanded && "rotate-90",
                  )}
                />
              </button>
            </Collapsible.Trigger>
            <button
              type="button"
              className="flex min-w-0 flex-1 cursor-pointer items-center gap-1.5 border-none bg-transparent p-0 text-left"
              onClick={handleSelect}
            >
              <FileTreeIcon>
                {isExpanded ? (
                  <FolderOpenIcon size={15} className="shrink-0 text-brand" />
                ) : (
                  <FolderIcon size={15} className="shrink-0 text-brand" />
                )}
              </FileTreeIcon>
              <FileTreeName>{name}</FileTreeName>
            </button>
          </div>
          <Collapsible.Content>
            <div className="ml-3.5 border-l border-border pl-2">
              {children}
            </div>
          </Collapsible.Content>
        </div>
      </Collapsible.Root>
    </FileTreeFolderContext.Provider>
  );
};

interface FileTreeFileContextType {
  path: string;
  name: string;
}

const FileTreeFileContext = createContext<FileTreeFileContextType>({
  name: "",
  path: "",
});

export type FileTreeFileProps = HTMLAttributes<HTMLDivElement> & {
  path: string;
  name: string;
  icon?: ReactNode;
};

export const FileTreeFile = ({
  path,
  name,
  icon,
  className,
  children,
  ...props
}: FileTreeFileProps) => {
  const { selectedPath, onSelect } = useContext(FileTreeContext);
  const isSelected = selectedPath === path;

  const handleClick = useCallback(() => {
    onSelect?.(path);
  }, [onSelect, path]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        onSelect?.(path);
      }
    },
    [onSelect, path],
  );

  const fileContextValue = useMemo(() => ({ name, path }), [name, path]);

  return (
    <FileTreeFileContext.Provider value={fileContextValue}>
      <div
        className={cn(
          "flex cursor-pointer items-center gap-1 rounded-[var(--radius-md)] px-2 py-1 transition-colors outline-none focus-visible:ring-0",
          isSelected
            ? "bg-brand-ghost text-brand"
            : "hover:bg-surface text-ink",
          className,
        )}
        onClick={handleClick}
        onKeyDown={handleKeyDown}
        role="treeitem"
        tabIndex={0}
        {...props}
      >
        {children ?? (
          <>
            <span className="h-4 w-4 shrink-0" />
            <FileTreeIcon>
              {icon ?? (
                <FileIcon size={14} className="shrink-0 text-ink-faint" />
              )}
            </FileTreeIcon>
            <FileTreeName>{name}</FileTreeName>
          </>
        )}
      </div>
    </FileTreeFileContext.Provider>
  );
};

export type FileTreeActionsProps = HTMLAttributes<HTMLDivElement>;

const stopPropagation = (e: React.SyntheticEvent) => e.stopPropagation();

export const FileTreeActions = ({
  className,
  children,
  ...props
}: FileTreeActionsProps) => (
  <div
    className={cn("ml-auto flex items-center gap-1", className)}
    onClick={stopPropagation}
    {...props}
  >
    {children}
  </div>
);

export type FileTreeNodeProps = {
  nodes: FileNode[];
};

export type FileNode = {
  path: string;
  name: string;
  kind: "file" | "directory";
  children?: FileNode[];
};

/** Render a list of FileNode objects as FileTreeFolder/FileTreeFile children. */
export function FileTreeNodes({ nodes }: FileTreeNodeProps) {
  return nodes.map((node) =>
    node.kind === "directory" ? (
      <FileTreeFolder key={node.path} path={node.path} name={node.name}>
        {node.children && node.children.length > 0 && (
          <FileTreeNodes nodes={node.children} />
        )}
      </FileTreeFolder>
    ) : (
      <FileTreeFile key={node.path} path={node.path} name={node.name} />
    ),
  );
}

