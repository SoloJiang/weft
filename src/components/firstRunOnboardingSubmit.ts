// Explicit .ts extension (allowImportingTsExtensions) so this module also loads
// under `node --test`, which resolves ESM specifiers without extension guessing.
import { repoSubmitName, type PendingRepo } from "./firstRunOnboardingRepos.ts";

/** The slice of `api` the onboarding submit needs. Keeps the helper pure and
 *  unit-testable without React or Tauri. */
export type OnboardingApi = {
  checkGitRepo: (path: string) => Promise<boolean>;
  createWorkspace: (name: string) => Promise<{ id: number }>;
  addRepoRef: (workspaceId: number, name: string, localGitPath: string) => Promise<unknown>;
  createThread: (workspaceId: number, title: string, kind: string) => Promise<unknown>;
};

export type OnboardingInput = {
  name: string;
  title: string;
  issueKind: string;
  repos: PendingRepo[];
};

/** Raised when one or more picked folders are not git repositories. Carries the
 *  offending paths so the UI can name them; thrown BEFORE any persistence. */
export class InvalidReposError extends Error {
  invalidRepos: string[];
  constructor(invalidRepos: string[]) {
    super(`not git repositories: ${invalidRepos.join(", ")}`);
    this.name = "InvalidReposError";
    this.invalidRepos = invalidRepos;
  }
}

/**
 * Persist the first-run onboarding result. Validates every repo path FIRST, so a
 * non-git folder fails before `createWorkspace` runs — otherwise a rejected repo
 * import orphans the just-created workspace, and each retry creates another.
 * Returns the created workspace.
 */
export async function submitOnboarding(
  api: OnboardingApi,
  { name, title, issueKind, repos }: OnboardingInput,
): Promise<{ id: number }> {
  const invalid: string[] = [];
  for (const repo of repos) {
    if (!(await api.checkGitRepo(repo.path))) invalid.push(repo.path);
  }
  if (invalid.length > 0) throw new InvalidReposError(invalid);

  const ws = await api.createWorkspace(name);
  for (const repo of repos) {
    await api.addRepoRef(ws.id, repoSubmitName(repo), repo.path);
  }
  await api.createThread(ws.id, title, issueKind);
  return ws;
}
