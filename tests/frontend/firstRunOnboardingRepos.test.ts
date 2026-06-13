import test from "node:test";
import assert from "node:assert/strict";
import {
  addPendingRepo,
  basename,
  removePendingRepo,
  renamePendingRepo,
  repoSubmitName,
  type PendingRepo,
} from "../../src/components/firstRunOnboardingRepos.ts";

test("derives repo names from local paths", () => {
  assert.equal(basename("/Users/me/code/weft/"), "weft");
  assert.equal(basename("relative/repo"), "repo");
  assert.equal(basename("/"), "");
});

test("derives repo names from Windows backslash paths", () => {
  assert.equal(basename("C:\\work\\api"), "api");
  assert.equal(basename("C:\\work\\api\\"), "api"); // trailing separator
  assert.equal(repoSubmitName({ id: "C:\\work\\api", name: "  ", path: "C:\\work\\api" }), "api");
  // a trailing backslash is the same repo (dedup)
  const seeded = addPendingRepo([], "C:\\work\\api");
  const dup = addPendingRepo(seeded.repos, "C:\\work\\api\\");
  assert.equal(dup.added, false);
});

test("adds a picked repo once with a derived name", () => {
  const result = addPendingRepo([], "/Users/me/code/weft/");
  assert.equal(result.added, true);
  assert.deepEqual(result.repos, [
    { id: "/Users/me/code/weft", name: "weft", path: "/Users/me/code/weft" },
  ]);
});

test("rejects duplicate paths", () => {
  const existing: PendingRepo[] = [
    { id: "/Users/me/code/weft", name: "weft", path: "/Users/me/code/weft" },
  ];
  const result = addPendingRepo(existing, "/Users/me/code/weft/");
  assert.equal(result.added, false);
  assert.equal(result.repos, existing);
});

test("renames and removes pending repos", () => {
  const existing: PendingRepo[] = [
    { id: "/repo/a", name: "a", path: "/repo/a" },
    { id: "/repo/b", name: "b", path: "/repo/b" },
  ];
  assert.equal(renamePendingRepo(existing, "/repo/a", "api")[0].name, "api");
  assert.deepEqual(removePendingRepo(existing, "/repo/a"), [
    { id: "/repo/b", name: "b", path: "/repo/b" },
  ]);
});

test("uses edited name, then basename, then repo fallback for submit", () => {
  assert.equal(repoSubmitName({ id: "/repo/a", name: "api", path: "/repo/a" }), "api");
  assert.equal(repoSubmitName({ id: "/repo/a", name: "  ", path: "/repo/a" }), "a");
  assert.equal(repoSubmitName({ id: "/", name: "  ", path: "/" }), "repo");
});
