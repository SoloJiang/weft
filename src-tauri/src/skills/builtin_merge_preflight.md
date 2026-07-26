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
- Treat validation as code execution. For an external or otherwise untrusted candidate, do not run its validation commands in the normal agent environment. Require explicit human trust confirmation or use a credential-free isolated sandbox/container; otherwise report that validation was withheld and limit the preflight to non-executing inspection.

## Protocol

1. Resolve the refs before changing anything. Identify the target/base repository and ref, then separately identify the candidate head repository and ref or provider-specific PR ref. Fetch the latest base from the target remote. Fetch the candidate from its head remote or provider-specific PR ref, not by assuming its branch exists on the target remote. Resolve and record the post-fetch base SHA and candidate SHA, and for a pull request verify the fetched candidate SHA exactly matches the hosting provider's advertised head SHA. Do not use stale pre-fetch SHAs.

2. Create a unique temporary directory and detached worktree, for example under the system temporary directory with a timestamp, process id, and random suffix. Add it at the latest base SHA with `git worktree add --detach <temporary-worktree> <base-sha>`. Do not use the original working tree as the worktree or as the merge workspace.

3. In the temporary worktree, rehearse the real target-side operation: the base is the current side and the candidate is merged into it. Reproduce the repository's selected merge method exactly. For an ordinary merge, use a non-fast-forward, non-committing command such as `git merge --no-commit --no-ff <candidate-sha>`; if the selected method is squash or rebase, use its equivalent local rehearsal instead. This keeps the rehearsal disposable while preserving direction-sensitive merge-driver semantics.

   - If the merge reports textual conflicts or otherwise fails, stop before validation. Capture `git status --short` and the affected paths, including `git diff --name-only --diff-filter=U` when available. Report the conflict status and paths, then clean up.
   - Do not resolve conflicts silently, choose a side automatically, or continue to validation after a conflicted merge.

4. Only after a clean merge, decide whether validation may execute. For an external or otherwise untrusted candidate, an ordinary temporary worktree is not a sandbox: it can still access credentials, the home directory, network, and shared Git state. Obtain explicit human trust confirmation or use a credential-free isolated sandbox/container before running candidate-controlled commands. Without one of those conditions, report the clean/conflicted merge result and that validation was not run.

   When validation is authorized, inspect the target repository's existing validation ladder: its contributor instructions, CI configuration, Makefile/task runner, and package scripts. Run the checks that repository already defines, in its documented order.

   - A Cargo project must include `cargo check` from the appropriate manifest/workspace, plus any other existing Rust checks required by that repository.
   - A JavaScript project must use its existing package-manager scripts from `package.json` or workspace configuration (for example, the repository's `pnpm` scripts); do not substitute a universal command list.
   - Do not imply that every repository supports the same commands. If a documented check is unavailable or a validation command fails, record that exact result and stop the ladder as the repository's policy requires.

5. Report the candidate head SHA and base SHA, whether the local merge was clean, every command run with its exit status and result, any conflict paths or validation failures, and any cleanup failure. Keep the report tied to the exact post-fetch SHAs.

6. Clean up on every exit path. Abort an in-progress merge in the temporary worktree when needed, remove the temporary worktree with `git worktree remove --force <temporary-worktree>`, remove its unique temporary directory, and verify that no temporary worktree remains. If interruption prevents complete cleanup, report the remaining path and finish cleanup before any later attempt. Never push, force-push, or perform a remote GitHub merge as part of this protocol.
