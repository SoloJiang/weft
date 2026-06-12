# Onboarding Real Repository Add Design

## Goal

Replace the fake repository step in first-run onboarding with a real, simple local-repository picker. First-time users should be able to create a workspace, attach one or more existing local git repositories, and create their first issue in one flow.

## Current State

`src/components/FirstRunOnboarding.tsx` renders a fake repository step:

- `REPOS` is a static list of sample repos.
- Step 2 only displays those sample repos.
- `enter()` creates a workspace and first issue, but never calls `api.addRepoRef`.

Real repository APIs already exist:

- `api.pickFolder(title)` opens the system folder picker.
- `api.addRepoRef(workspaceId, name, localGitPath)` registers an existing local repository.
- `AddRepoDialog` supports local/clone/new in normal workspace usage.

The onboarding path should not reuse the full `AddRepoDialog`. First-run setup should remain as simple as creating a workspace.

## Design

Use an embedded pending repository list in `FirstRunOnboarding`.

### User Flow

1. User enters workspace name.
2. User reaches the repository step.
3. Repository step shows an empty state and an `Add local repository` button.
4. Clicking the button opens the system folder picker.
5. Selected path is added to a pending list.
6. Repo name defaults to the selected path basename and can be edited inline.
7. User can remove an accidentally selected repo.
8. User can skip the repository step and create a workspace with zero repos.
9. On final Enter:
   - create workspace
   - add each pending repo to that workspace
   - create first issue
   - refresh workspaces
   - select the new workspace
   - dismiss onboarding

### Scope

First version supports only existing local repositories. Clone and new-repo creation stay in the normal `AddRepoDialog` after onboarding.

This keeps first-run setup short and avoids creating a workspace early just to make the existing dialog work.

## Component State

Add a local type in `FirstRunOnboarding.tsx`:

```ts
type PendingRepo = {
  id: string;
  name: string;
  path: string;
};
```

Add state:

```ts
const [repos, setRepos] = useState<PendingRepo[]>([]);
```

`id` should be deterministic enough for UI state, for example the selected path. Paths are unique in the pending list.

## Repository Selection

Add a helper:

```ts
async function pickRepo() {
  const path = await api.pickFolder(t("dialog.addRepoTitle"));
  if (!path) return;
  const normalized = path.trim();
  if (!normalized) return;
  if (repos.some((repo) => repo.path === normalized)) {
    setErr(t("onboarding.repoDuplicate"));
    return;
  }
  setErr(null);
  setRepos((current) => [
    ...current,
    { id: normalized, path: normalized, name: basename(normalized) || "repo" },
  ]);
}
```

`basename` should match the existing local path behavior in `nav/dialogs.tsx`: trim trailing slashes, split by `/`, use the final segment.

Do not pre-validate whether the selected folder is a git repository in the frontend. The backend already owns repository validation and error reporting.

## Final Submit Behavior

Update `enter()`:

```ts
const ws = await api.createWorkspace(name);
for (const repo of repos) {
  await api.addRepoRef(ws.id, repo.name.trim() || basename(repo.path) || "repo", repo.path);
}
await api.createThread(ws.id, title, issueKind);
await refreshWorkspaces();
await selectWorkspace(ws.id);
dismiss();
```

If any `addRepoRef` call fails, show the error and do not dismiss onboarding. The workspace may already exist; do not add rollback in this change. Rollback would be more destructive and requires a separate confirmation design.

## UI

Replace static fake repo cards with real pending repo cards.

Empty state copy:

- English: `Add one or more existing git repositories. You can also skip this and add repositories later.`
- Chinese: `添加一个或多个已有本地 Git 仓库。也可以跳过，稍后再添加。`

Controls:

- Primary action in the repo step: `Add local repository` / `添加本地仓库`
- Each pending repo card:
  - editable repo name input
  - read-only path line
  - remove button

Do not add clone/new controls to onboarding.

## Validation

Final Enter remains disabled until workspace name, first issue title, and issue kind are present. Repositories are optional.

Repository name fallback order when submitting:

1. edited repo name
2. basename(path)
3. `repo`

Duplicate selected paths are blocked in the pending list and surface a non-fatal inline error.

## i18n

Update both `src/i18n/en.ts` and `src/i18n/zh.ts`.

Required new keys under `onboarding`:

- `repoEmpty`
- `addLocalRepo`
- `repoDuplicate`
- `repoNameLabel`
- `repoPathLabel`
- `removeRepo`

Existing `onboarding.addReposTitle`, `onboarding.addReposBody`, and `onboarding.moreRepos` can be rewritten or removed if no longer used.

## Acceptance Criteria

- First-run onboarding no longer renders the static sample repo list.
- User can select a local folder in the repository step.
- Selected repository appears in the pending list with editable name and path display.
- Selecting the same folder twice does not add a duplicate and shows an inline message.
- User can remove a pending repository.
- User can finish onboarding with zero repositories.
- User can finish onboarding with one or more repositories; each repository is registered through `api.addRepoRef` after workspace creation.
- If repository registration fails, onboarding remains open and shows the error.
- `pnpm build` passes.

## Non-Goals

- Do not add clone repository onboarding.
- Do not add create-new-repository onboarding.
- Do not pre-create the workspace before the final Enter action.
- Do not rollback a workspace if repository registration fails.
- Do not change backend repository validation.
