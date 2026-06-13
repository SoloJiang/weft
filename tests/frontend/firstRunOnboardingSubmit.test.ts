import test from "node:test";
import assert from "node:assert/strict";
import {
  submitOnboarding,
  InvalidReposError,
  type OnboardingApi,
} from "../../src/components/firstRunOnboardingSubmit.ts";
import type { PendingRepo } from "../../src/components/firstRunOnboardingRepos.ts";

function fakeApi(badPaths: string[] = []) {
  const bad = new Set(badPaths);
  const calls = { checkGitRepo: 0, createWorkspace: 0, addRepoRef: 0, createThread: 0 };
  const workspaces: { id: number; name: string }[] = [];
  let nextId = 1;
  const api: OnboardingApi & { calls: typeof calls; workspaces: typeof workspaces } = {
    calls,
    workspaces,
    checkGitRepo: async (path: string) => {
      calls.checkGitRepo++;
      return !bad.has(path);
    },
    createWorkspace: async (name: string) => {
      calls.createWorkspace++;
      const ws = { id: nextId++, name };
      workspaces.push(ws);
      return ws;
    },
    addRepoRef: async () => {
      calls.addRepoRef++;
      return {};
    },
    createThread: async () => {
      calls.createThread++;
      return {};
    },
  };
  return api;
}

const repo = (path: string): PendingRepo => ({ id: path, name: "", path });

test("rejects before persisting when a repo is not a git repository", async () => {
  const api = fakeApi(["/bad/repo"]);
  await assert.rejects(
    () =>
      submitOnboarding(api, {
        name: "ws",
        title: "fix login",
        issueKind: "bugfix",
        repos: [repo("/good/repo"), repo("/bad/repo")],
      }),
    (e: unknown) =>
      e instanceof InvalidReposError && e.invalidRepos.length === 1 && e.invalidRepos[0] === "/bad/repo",
  );
  // nothing was persisted
  assert.equal(api.calls.createWorkspace, 0);
  assert.equal(api.calls.addRepoRef, 0);
  assert.equal(api.calls.createThread, 0);
  assert.equal(api.workspaces.length, 0);
});

test("retrying with the same bad repo never orphans a workspace", async () => {
  const api = fakeApi(["/bad/repo"]);
  const input = { name: "ws", title: "t", issueKind: "bugfix", repos: [repo("/bad/repo")] };
  await assert.rejects(() => submitOnboarding(api, input));
  await assert.rejects(() => submitOnboarding(api, input));
  assert.equal(api.workspaces.length, 0);
});

test("happy path creates the workspace, adds each repo, then the issue", async () => {
  const api = fakeApi();
  const ws = await submitOnboarding(api, {
    name: "ws",
    title: "fix login",
    issueKind: "bugfix",
    repos: [repo("/a"), repo("/b")],
  });
  assert.equal(ws.id, 1);
  assert.equal(api.calls.createWorkspace, 1);
  assert.equal(api.calls.addRepoRef, 2);
  assert.equal(api.calls.createThread, 1);
  assert.equal(api.workspaces.length, 1);
});
