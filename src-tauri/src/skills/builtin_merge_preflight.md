---
name: weft-preflight-merge
description: Use before merging any pull request or merging into a protected branch to rehearse the candidate against the latest target/base ref in an isolated temporary worktree, report conflicts and validation results, and leave remote merge operations to the user.
---

<!-- weft-builtin -->

# Merge semantic preflight

Use this protocol before approving or carrying out any pull request merge or protected-branch merge. It is a local merge rehearsal, not merge authority or backend automation.

## Safety invariants

- Never merge, reset, check out, or run validation in the original working tree. Use a uniquely named temporary detached worktree for the entire rehearsal.
- Never push, force-push, update a remote ref, call a GitHub merge API, or perform a remote merge automatically.
- Register cleanup before creating the temporary worktree. Cleanup must run on success, a conflict, validation failure, and interruption.

## Protocol

1. Resolve the refs before changing anything. Identify the repository's current target/base ref, its remote, and the candidate head ref or SHA. Fetch the latest relevant refs from that remote (including the target/base ref and the candidate ref when it is remote-backed), then resolve and record the post-fetch candidate head SHA and base SHA. Do not use a stale pre-fetch base SHA.

2. Create a unique temporary directory and detached worktree, for example under the system temporary directory with a timestamp, process id, and random suffix. Add it at the candidate head with `git worktree add --detach <temporary-worktree> <candidate-head-sha>`. Do not use the original working tree as the worktree or as the merge workspace.

3. In the temporary worktree, merge the latest base into the candidate head locally with a non-fast-forward, non-committing merge such as `git merge --no-commit --no-ff <base-sha>`. This keeps the rehearsal disposable while exercising the real merge semantics.

   - If the merge reports textual conflicts or otherwise fails, stop before validation. Capture `git status --short` and the affected paths, including `git diff --name-only --diff-filter=U` when available. Report the conflict status and paths, then clean up.
   - Do not resolve conflicts silently, choose a side automatically, or continue to validation after a conflicted merge.

4. Only after a clean merge, inspect the target repository's existing validation ladder: its contributor instructions, CI configuration, Makefile/task runner, and package scripts. Run the checks that repository already defines, in its documented order.

   - A Cargo project must include `cargo check` from the appropriate manifest/workspace, plus any other existing Rust checks required by that repository.
   - A JavaScript project must use its existing package-manager scripts from `package.json` or workspace configuration (for example, the repository's `pnpm` scripts); do not substitute a universal command list.
   - Do not imply that every repository supports the same commands. If a documented check is unavailable or a validation command fails, record that exact result and stop the ladder as the repository's policy requires.

5. Report the candidate head SHA and base SHA, whether the local merge was clean, every command run with its exit status and result, any conflict paths or validation failures, and any cleanup failure. Keep the report tied to the exact post-fetch SHAs.

6. Clean up on every exit path. Abort an in-progress merge in the temporary worktree when needed, remove the temporary worktree with `git worktree remove --force <temporary-worktree>`, remove its unique temporary directory, and verify that no temporary worktree remains. If interruption prevents complete cleanup, report the remaining path and finish cleanup before any later attempt. Never push, force-push, or perform a remote GitHub merge as part of this protocol.
