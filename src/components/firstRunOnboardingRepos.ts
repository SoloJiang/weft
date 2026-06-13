export type PendingRepo = {
  id: string;
  name: string;
  path: string;
};

// Handle both POSIX (`/`) and Windows (`\`) separators: the native folder picker
// returns backslash paths on Windows, where splitting on `/` alone would keep the
// whole path as the repo name and miss trailing-separator duplicates.
export const basename = (p: string) =>
  p.trim().replace(/[\\/]+$/, "").split(/[\\/]/).filter(Boolean).pop() ?? "";

const normalizePath = (p: string) => p.trim().replace(/[\\/]+$/, "");

export function addPendingRepo(
  current: PendingRepo[],
  pickedPath: string,
): { added: boolean; repos: PendingRepo[] } {
  const path = normalizePath(pickedPath);
  if (!path) return { added: false, repos: current };
  if (current.some((repo) => repo.path === path)) return { added: false, repos: current };
  return {
    added: true,
    repos: [...current, { id: path, path, name: basename(path) || "repo" }],
  };
}

export function renamePendingRepo(current: PendingRepo[], id: string, name: string): PendingRepo[] {
  return current.map((repo) => (repo.id === id ? { ...repo, name } : repo));
}

export function removePendingRepo(current: PendingRepo[], id: string): PendingRepo[] {
  return current.filter((repo) => repo.id !== id);
}

export function repoSubmitName(repo: PendingRepo): string {
  return repo.name.trim() || basename(repo.path) || "repo";
}
