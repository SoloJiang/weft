# Onboarding Real Repository Add Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace fake first-run onboarding repository cards with a real local git repository picker and pending repository list.

**Architecture:** Keep the flow local to `FirstRunOnboarding.tsx`: pending repos are stored in component state until final submit, then persisted via existing `api.addRepoRef` after `api.createWorkspace`. User-facing text stays in `src/i18n/en.ts` and `src/i18n/zh.ts`. Tests cover pure helper behavior without adding a browser test framework.

**Tech Stack:** React 19, TypeScript, Tauri invoke API wrapper, existing Vite build.

---

## File Structure

- Modify: `src/components/FirstRunOnboarding.tsx`
  - Remove fake `REPOS`, `NODES`, and `EDGES` sample data only if no longer used.
  - Add pending repo state and handlers.
  - Replace repo step UI with real local picker list.
  - Persist pending repos during `enter()`.

- Modify: `src/i18n/en.ts`
  - Add onboarding repository picker strings.

- Modify: `src/i18n/zh.ts`
  - Add Chinese onboarding repository picker strings.

- Test: `src/components/FirstRunOnboarding.test.ts`
  - Add pure helper tests for basename, duplicate insertion, editing, removal, and submit payload ordering.

---

### Task 1: Extract testable onboarding repo helpers

**Files:**
- Create: `src/components/FirstRunOnboarding.test.ts`
- Modify: `src/components/FirstRunOnboarding.tsx`

- [ ] **Step 1: Write failing helper tests**

Create `src/components/FirstRunOnboarding.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import {
  addPendingRepo,
  basename,
  removePendingRepo,
  renamePendingRepo,
  repoSubmitName,
  type PendingRepo,
} from "./FirstRunOnboarding";

describe("onboarding repository helpers", () => {
  it("derives repo names from local paths", () => {
    expect(basename("/Users/me/code/weft/")).toBe("weft");
    expect(basename("relative/repo")).toBe("repo");
    expect(basename("/")).toBe("");
  });

  it("adds a picked repo once with a derived name", () => {
    const result = addPendingRepo([], "/Users/me/code/weft/");
    expect(result.added).toBe(true);
    expect(result.repos).toEqual([
      { id: "/Users/me/code/weft", name: "weft", path: "/Users/me/code/weft" },
    ]);
  });

  it("rejects duplicate paths", () => {
    const existing: PendingRepo[] = [
      { id: "/Users/me/code/weft", name: "weft", path: "/Users/me/code/weft" },
    ];
    const result = addPendingRepo(existing, "/Users/me/code/weft/");
    expect(result.added).toBe(false);
    expect(result.repos).toBe(existing);
  });

  it("renames and removes pending repos", () => {
    const existing: PendingRepo[] = [
      { id: "/repo/a", name: "a", path: "/repo/a" },
      { id: "/repo/b", name: "b", path: "/repo/b" },
    ];
    expect(renamePendingRepo(existing, "/repo/a", "api")[0].name).toBe("api");
    expect(removePendingRepo(existing, "/repo/a")).toEqual([
      { id: "/repo/b", name: "b", path: "/repo/b" },
    ]);
  });

  it("uses edited name, then basename, then repo fallback for submit", () => {
    expect(repoSubmitName({ id: "/repo/a", name: "api", path: "/repo/a" })).toBe("api");
    expect(repoSubmitName({ id: "/repo/a", name: "  ", path: "/repo/a" })).toBe("a");
    expect(repoSubmitName({ id: "/", name: "  ", path: "/" })).toBe("repo");
  });
});
```

- [ ] **Step 2: Run failing tests**

Run:

```bash
pnpm test -- --run src/components/FirstRunOnboarding.test.ts
```

Expected: FAIL because no test script or helpers exist. If the project has no test script, add the smallest Vitest setup in Step 3 before implementation and rerun until failures are from missing exports.

- [ ] **Step 3: Add minimal test script if needed**

If `pnpm test` fails because package.json has no test script, add:

```json
"test": "vitest"
```

No extra dependency should be needed if `vitest` is already in `devDependencies`. If not present, do not add a new dependency; instead skip component unit tests and rely on TypeScript build plus helper type checking. Record that in final verification.

- [ ] **Step 4: Add helper exports**

In `FirstRunOnboarding.tsx`, add:

```ts
export type PendingRepo = {
  id: string;
  name: string;
  path: string;
};

export const basename = (p: string) => p.trim().replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? "";

const normalizePath = (p: string) => p.trim().replace(/\/+$/, "");

export function addPendingRepo(current: PendingRepo[], pickedPath: string): { added: boolean; repos: PendingRepo[] } {
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
```

- [ ] **Step 5: Verify helper tests pass or document no test runner**

Run:

```bash
pnpm test -- --run src/components/FirstRunOnboarding.test.ts
```

Expected: PASS if Vitest is available.

---

### Task 2: Replace fake repo UI with real pending list

**Files:**
- Modify: `src/components/FirstRunOnboarding.tsx`
- Modify: `src/i18n/en.ts`
- Modify: `src/i18n/zh.ts`

- [ ] **Step 1: Add component state and picker handler**

In `FirstRunOnboarding`, add:

```ts
const [repos, setRepos] = useState<PendingRepo[]>([]);

async function pickRepo() {
  const picked = await api.pickFolder(t("dialog.addRepoTitle"));
  if (!picked) return;
  setRepos((current) => {
    const result = addPendingRepo(current, picked);
    if (!result.added) setErr(t("onboarding.repoDuplicate"));
    else setErr(null);
    return result.repos;
  });
}
```

Pass `repos`, `setRepos`, and `pickRepo` into `OnboardingStage`.

- [ ] **Step 2: Persist repos during final submit**

In `enter()` after `createWorkspace` and before `createThread`, add:

```ts
for (const repo of repos) {
  await api.addRepoRef(ws.id, repoSubmitName(repo), repo.path);
}
```

- [ ] **Step 3: Replace step 2 UI**

Replace the fake repo cards with:

```tsx
<div className="mt-5 flex flex-col gap-3">
  {repos.length === 0 ? (
    <div className="rounded-[var(--radius-md)] border border-dashed border-border bg-bg/60 px-3 py-4 text-[12px] leading-relaxed text-ink-faint">
      {t("onboarding.repoEmpty")}
    </div>
  ) : (
    repos.map((repo) => (
      <div key={repo.id} className="rounded-[var(--radius-md)] border border-border bg-bg/70 p-3">
        <div className="flex items-center gap-2">
          <GitBranch size={15} className="shrink-0 text-brand" />
          <input
            value={repo.name}
            aria-label={t("onboarding.repoNameLabel")}
            onChange={(e) => setRepos((current) => renamePendingRepo(current, repo.id, e.currentTarget.value))}
            className="min-w-0 flex-1 rounded-[var(--radius-sm)] border border-border bg-surface px-2 py-1 text-[12px] font-semibold text-ink outline-none focus:border-brand focus:ring-2 focus:ring-brand/25"
          />
          <button
            type="button"
            onClick={() => setRepos((current) => removePendingRepo(current, repo.id))}
            className="rounded-[var(--radius-sm)] px-2 py-1 text-[11px] text-ink-faint hover:bg-brand-ghost hover:text-ink"
          >
            {t("onboarding.removeRepo")}
          </button>
        </div>
        <div className="mt-2 truncate font-mono text-[11px] text-ink-faint" title={repo.path}>
          {repo.path}
        </div>
      </div>
    ))
  )}
  <button
    type="button"
    onClick={() => void pickRepo()}
    className="inline-flex h-9 items-center justify-center gap-2 rounded-[var(--radius-md)] border border-border bg-surface px-3 text-[12.5px] font-medium text-ink transition-colors hover:border-brand hover:bg-brand-ghost"
  >
    <Plus size={14} />
    {t("onboarding.addLocalRepo")}
  </button>
</div>
```

- [ ] **Step 4: Remove unused fake graph imports/data only if unused**

Do not remove `NODES`/`EDGES` if `OnboardingGraph` still uses them. Remove `REPOS` if no longer referenced.

- [ ] **Step 5: Add i18n keys**

English `onboarding` keys:

```ts
repoEmpty: "Add one or more existing git repositories. You can also skip this and add repositories later.",
addLocalRepo: "Add local repository",
repoDuplicate: "That repository is already in the list.",
repoNameLabel: "Repository name",
repoPathLabel: "Repository path",
removeRepo: "Remove",
```

Chinese `onboarding` keys:

```ts
repoEmpty: "添加一个或多个已有本地 Git 仓库。也可以跳过，稍后再添加。",
addLocalRepo: "添加本地仓库",
repoDuplicate: "这个仓库已经在列表里。",
repoNameLabel: "仓库名称",
repoPathLabel: "仓库路径",
removeRepo: "移除",
```

---

### Task 3: Verify onboarding implementation

**Files:**
- Modified frontend files

- [ ] **Step 1: Run unit tests if available**

Run:

```bash
pnpm test -- --run src/components/FirstRunOnboarding.test.ts
```

Expected: PASS if Vitest is available. If not available and not added, state that no frontend test runner exists.

- [ ] **Step 2: Run frontend build**

Run:

```bash
pnpm build
```

Expected: PASS.

- [ ] **Step 3: Check patch whitespace**

Run:

```bash
git diff --check
```

Expected: no output.
