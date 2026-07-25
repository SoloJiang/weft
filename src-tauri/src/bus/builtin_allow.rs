//! The Ask Bridge's closed allowlist for an ENGINE'S OWN BUILT-IN tools —
//! the sibling of `server::AUTO_APPROVED_INTERNAL_TOOLS` (which covers weft's
//! own injected MCP tools).
//!
//! # Why this exists
//!
//! `inject::inject_ask_hook` installs a PreToolUse hook with a WILDCARD matcher
//! (`"*"` for claude, `".*"` for codex), so EVERY tool call reaches
//! `server::handle_ask` — including read-only builtins (`Read`, `Grep`,
//! `Glob`). Each one blocks on a human click in Needs-you, and a routine
//! file-reading turn issues dozens of them: dogfooding issue #96 froze a lead
//! for 23 minutes on nothing but reads.
//!
//! # Why the fix is here and NOT in the matcher
//!
//! The obvious fix — narrow the hook's matcher so safe tools never fire it —
//! fails in the DANGEROUS direction. A matcher is a positive filter: a tool
//! name the pattern doesn't match is not "asked about later", it is NEVER SEEN,
//! so it runs ungated. Expressing "everything except these safe ones" as a
//! matcher regex therefore makes every tool name the pattern-author didn't
//! anticipate — a newly shipped builtin, a renamed one, an MCP tool whose name
//! happens to dodge the pattern — silently ungated. That is a denylist wearing
//! a matcher's clothes, and its failure mode is exactly the one this change
//! must not have.
//!
//! So the matcher STAYS a wildcard (the hook keeps seeing everything) and the
//! narrowing happens HERE, on a closed allowlist: a name that isn't listed
//! surfaces the Needs-you card exactly as it does today. Every failure — an
//! unknown name, a drifted name, a path that can't be resolved, a DB error —
//! lands on "ask the human", never on "allow".
//!
//! # The bar for an entry
//!
//! A PreToolUse `allow` decision BYPASSES the CLI's own permission check rather
//! than deferring to it, so an entry here is weft OVERRIDING the engine, and
//! the shape of the rule has to be one the engine itself would recognize.
//! claude's tools reference ("Tools reference",
//! `code.claude.com/docs/en/tools-reference`) draws the line precisely:
//! `Read`/`Grep`/`Glob` don't prompt for paths INSIDE the working directory and
//! its additional directories, and DO prompt outside them. `ReadOnlyPath`
//! adopts that same rule — read-only tools, scoped to the session's own
//! directories — with weft's DB supplying the directory set (`session_roots`).
//!
//! That set is NOT always identical to the engine's own cwd, and the difference
//! is deliberate: a lead's cwd is an almost-empty scratch dir
//! (`<weft_home>/leads/<thread>`) while the repos it plans across live
//! elsewhere, so claude alone would prompt for every one of those reads. weft
//! knows what that lead's project actually is and says so, rather than weft
//! being loose about it — the set stays closed, weft-owned, and built only from
//! directories the user explicitly registered.
//!
//! Three independent conditions must ALL hold before a call is waved through
//! (`server::handle_ask` applies them in that order); each one can only
//! SUBTRACT auto-approvals:
//!
//! 1. the engine + tool name is in `SAFE_BUILTINS` (this file), and
//! 2. `ask::classify_risk` independently rates the call `ReadOnly`, and
//! 3. every path in its arguments is inside the session's own directories.
//!
//! Condition 2 deserves a note: `classify_risk` documents itself as a UX
//! heuristic for card triage, NOT a security boundary. That stays true — it is
//! used here only as a VETO (a non-`ReadOnly` verdict forces the card), never
//! as a reason to allow. The gate is the allowlist; the heuristic only takes
//! things off it. Its practical job is catching a credential-shaped file that
//! lives INSIDE the working directory (a repo's own `.env`, `.npmrc`,
//! `.git-credentials`), which containment alone would happily allow.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// How much of a safe builtin's INPUT still has to be checked before its call
/// can skip the human.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafeScope {
    /// The tool has no target to point anywhere: its capability is fixed and
    /// confined to the agent's own bookkeeping, so no argument can turn it into
    /// something else. Approved on the name alone.
    NoTarget,
    /// The tool reads whatever its arguments point at, so the ARGUMENTS decide
    /// whether the call is safe. Approved only when `classify_risk` rates it
    /// `ReadOnly` AND every path it names is inside the session's own
    /// directories (`paths_contained`).
    ReadOnlyPath,
}

/// The EXACT `(engine, tool_name, scope)` triples the Ask Bridge auto-approves
/// without a human click. Closed allowlist: anything absent surfaces the card.
///
/// Matching is EXACT and CASE-SENSITIVE, and keyed by engine on purpose:
///
/// - Engine-keyed because a bare tool name is not globally unique. opencode
///   flattens MCP tool names to `<server>_<tool>` with a SINGLE underscore, so
///   an opencode MCP server named `update` exposing a tool named `plan` reports
///   the name `update_plan` — indistinguishable from codex's builtin of that
///   name. Keying by engine makes that collision unreachable instead of
///   relying on the name to be unambiguous. (This is the same hazard
///   `server::split_internal_tool` documents for the MCP allowlist.)
/// - Case-sensitive because opencode's builtins are the lowercase spellings of
///   claude's (`read`, `grep`, `glob`); an ASCII-insensitive compare would
///   silently extend claude's entries to an engine whose tool vocabulary and
///   argument shapes were never audited here.
///
/// Notable DELIBERATE omissions:
///
/// - `Bash` / `apply_patch` / `Edit` / `Write` / `NotebookEdit` / `WebFetch` /
///   `WebSearch` — write, execute, or leave the machine. Arbitrary shell is
///   arbitrary regardless of how read-only the command text looks; weft has a
///   read-only-command classifier (`ask::READ_ONLY_COMMAND_WORDS`) but it is a
///   display heuristic, and promoting it into a gate that skips the human is a
///   separate, deliberate product decision — not something to slip in here.
/// - `Skill` — claude's own tools reference marks it permission-REQUIRED.
/// - `Agent` / `Task` — spawns a subagent. Its children each hit this bridge on
///   their own, but weft's session accounting doesn't model subagents, so the
///   spawn itself stays visible.
/// - `ExitPlanMode`, `LSP`, `ReadMcpResourceTool` — either permission-required
///   upstream or a capability whose full surface isn't pinned down here.
///   Unverified means gated.
/// - Every opencode builtin — its vocabulary is not verified in this repo, and
///   guessing at names is precisely the unsafe direction. opencode sessions
///   keep asking for everything, as today.
///
/// Sources for the claude names: claude's tools reference (the "Permission
/// required" column) cross-checked against the tool names appearing in real
/// weft-launched claude transcripts. For codex: its hooks documentation, which
/// states PreToolUse fires for `Bash`, `apply_patch`, MCP tools, and local
/// function tools such as `update_plan` — codex has no read-only file builtin
/// to list, because its reads go through the shell, which stays gated.
const SAFE_BUILTINS: &[(&str, &str, SafeScope)] = &[
    // ── claude ──────────────────────────────────────────────────────────────
    // Reads a file's contents. Path-scoped: `Read` is exactly the tool that
    // could otherwise walk out of the worktree into `~/.ssh`.
    ("claude", "Read", SafeScope::ReadOnlyPath),
    // Lists paths matching a glob. Returns names, not contents — still
    // path-scoped, since a listing outside the working dir is a leak too.
    ("claude", "Glob", SafeScope::ReadOnlyPath),
    // Searches file CONTENTS; same exposure as Read, same scoping.
    ("claude", "Grep", SafeScope::ReadOnlyPath),
    // Legacy notebook reader (superseded by `Read` in claude 2.x, still present
    // in older CLIs weft may be pointed at). Read-only by construction.
    ("claude", "NotebookRead", SafeScope::ReadOnlyPath),
    // The agent's own in-session checklist. No filesystem, no network, no
    // target: the input is the todo list itself.
    ("claude", "TodoWrite", SafeScope::NoTarget),
    // Loads deferred TOOL SCHEMAS into context. Returns declarations only, and
    // any tool it surfaces still hits this bridge when actually called.
    ("claude", "ToolSearch", SafeScope::NoTarget),
    // ── codex ───────────────────────────────────────────────────────────────
    // codex's analog of TodoWrite: the turn's plan steps. Same reasoning.
    ("codex", "update_plan", SafeScope::NoTarget),
];

/// Argument keys that NAME A TARGET for the path-scoped builtins, across the
/// engines' spelling conventions (claude `file_path`, opencode `filePath`,
/// `path` for Glob/Grep roots, `notebook_path` for the legacy notebook
/// reader). When one of these is present it MUST be a string, absolute, and
/// contained — a relative or `~`-prefixed value is refused rather than guessed
/// at, because resolving it would mean trusting a base directory the agent
/// could be wrong about.
const TARGET_KEYS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "notebookPath",
];

/// The scope at which `(engine, tool_name)` may skip the human, or `None` when
/// it may not — the single lookup `server::handle_ask` consults.
pub fn safe_scope(engine: &str, tool_name: &str) -> Option<SafeScope> {
    SAFE_BUILTINS
        .iter()
        .find(|(e, t, _)| *e == engine && *t == tool_name)
        .map(|(_, _, scope)| *scope)
}

/// Whether `path` resolves INSIDE one of `roots`.
///
/// Both sides are canonicalized, so this is containment of REAL locations, not
/// of the strings naming them: a symlink planted inside a worktree that points
/// at `~/.ssh` resolves outside every root and is refused, and macOS's
/// `/tmp` → `/private/tmp` aliasing doesn't produce a spurious miss. `roots`
/// arrives pre-canonicalized from `server::session_roots` (canonicalizing it
/// per call would re-stat every root on every ask).
///
/// `Path::starts_with` compares whole COMPONENTS, so `/repo-evil` is correctly
/// not inside `/repo`.
///
/// A path that can't be canonicalized — most often one that doesn't exist yet —
/// is NOT contained. Deliberate: containment can't be established for a
/// location that isn't there, and the honest answer to "can't establish it" is
/// the card. A read of a nonexistent file was going to fail in the engine
/// anyway.
fn contained(path: &str, roots: &[PathBuf]) -> bool {
    let p = Path::new(path);
    if !p.is_absolute() {
        return false;
    }
    let Ok(real) = std::fs::canonicalize(p) else {
        return false;
    };
    roots.iter().any(|r| real.starts_with(r))
}

/// Every absolute-path-shaped string anywhere in `input` is inside `roots`, and
/// every `TARGET_KEYS` entry present is a contained absolute path.
///
/// Two rules, because each covers what the other can't:
///
/// 1. A `TARGET_KEYS` key that IS present must be a string AND absolute AND
///    contained. This is what refuses `{"file_path": 42}` and
///    `{"file_path": "~/.ssh/id_rsa"}` — values rule 2 would skip because
///    neither is an absolute path string. A target key that is ABSENT is fine:
///    `Grep`/`Glob` then default to the engine's own cwd, which weft set to a
///    root when it spawned the session — a fact that only holds once the
///    session RESOLVED, hence the empty-`roots` refusal below.
/// 2. Recursively, ANY string value that looks like an absolute path must be
///    contained. This is the one that doesn't depend on knowing the tool's
///    schema: an argument key that isn't in `TARGET_KEYS` — one added by a
///    later CLI release, or a nested option object — still can't point out of
///    the session's directories.
///
/// KNOWN, ACCEPTED over-refusals from rule 2, both of which cost a click and
/// never an unwanted approval:
///
/// - A non-path string that merely STARTS with `/` — a `Grep` pattern like
///   `^/api/v1`, a URL path fragment — reads as an out-of-root path. The
///   alternative, deciding which leading-slash strings are "really" paths, is a
///   guess in the direction this module refuses to guess in.
/// - An ABSOLUTE glob pattern (`/wt/**/*.rs`) can't be canonicalized, so it
///   isn't contained even when it points inside a root. The common shape
///   claude actually emits — a relative `pattern` plus an absolute `path` — is
///   unaffected.
///
/// An empty `roots` — a session weft could NOT resolve (stale direction,
/// cross-thread route, deleted worktree; see `session_roots`) — refuses
/// everything, before the arguments are even looked at.
///
/// That check has to come first rather than falling out of the path rules,
/// because the targetless form has no path to fail on. `Grep {"pattern":
/// "TODO"}` names nothing and searches the ENGINE'S CWD, and rule 1 waves it
/// through on the strength of "the cwd is one of our roots" — which is exactly
/// the fact an empty `roots` says we could not establish. Scanning the
/// arguments would find nothing to reject and approve a read from an
/// unverified directory, turning the fail-closed identity check in
/// `session_roots` into a no-op for precisely the routes it exists to catch.
/// (Caught in review by Codex on PR #146; this function's own test had
/// asserted the permissive behavior as correct.)
pub fn paths_contained(input: Option<&Value>, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let Some(v) = input else {
        // No arguments at all: nothing points anywhere. `Read`/`Grep` without
        // arguments is malformed and the engine will reject it on its own.
        return true;
    };
    if let Some(obj) = v.as_object() {
        for key in TARGET_KEYS {
            let Some(target) = obj.get(*key) else {
                continue;
            };
            match target.as_str() {
                Some(s) if contained(s, roots) => {}
                _ => return false,
            }
        }
    }
    every_absolute_path_contained(v, roots)
}

/// Rule 2 of `paths_contained`, walked over the whole argument value.
fn every_absolute_path_contained(v: &Value, roots: &[PathBuf]) -> bool {
    match v {
        Value::String(s) => !Path::new(s).is_absolute() || contained(s, roots),
        Value::Array(items) => items
            .iter()
            .all(|i| every_absolute_path_contained(i, roots)),
        Value::Object(map) => map
            .values()
            .all(|i| every_absolute_path_contained(i, roots)),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A temp dir that cleans itself up, so a failing assert can't leak a tree.
    struct TempTree(PathBuf);
    impl TempTree {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "weft-allow-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempTree(std::fs::canonicalize(&p).unwrap())
        }
        fn file(&self, rel: &str) -> String {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, b"x").unwrap();
            p.to_string_lossy().to_string()
        }
        fn roots(&self) -> Vec<PathBuf> {
            vec![self.0.clone()]
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn safe_scope_is_exact_and_engine_keyed() {
        assert_eq!(
            safe_scope("claude", "Read"),
            Some(SafeScope::ReadOnlyPath),
            "the storm this whole module exists for"
        );
        assert_eq!(safe_scope("claude", "TodoWrite"), Some(SafeScope::NoTarget));
        assert_eq!(
            safe_scope("codex", "update_plan"),
            Some(SafeScope::NoTarget)
        );
        // claude's entries must NOT leak to another engine: opencode flattens
        // MCP names as `<server>_<tool>`, so a server `update` + tool `plan`
        // reports exactly `update_plan`.
        assert_eq!(safe_scope("opencode", "update_plan"), None);
        assert_eq!(safe_scope("codex", "Read"), None);
        assert_eq!(safe_scope("opencode", "Read"), None);
        // Case-sensitive: opencode's `read` is a DIFFERENT tool from claude's.
        assert_eq!(safe_scope("claude", "read"), None);
        assert_eq!(safe_scope("claude", "READ"), None);
        assert_eq!(safe_scope("opencode", "read"), None);
    }

    #[test]
    fn dangerous_builtins_are_never_allowlisted() {
        // The half of each CLI's vocabulary that writes, executes, or leaves
        // the machine. If any of these ever answers Some(..), the bridge has
        // stopped gating the thing it exists to gate.
        for name in [
            "Bash",
            "BashOutput",
            "KillShell",
            "Write",
            "Edit",
            "MultiEdit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Agent",
            "Task",
            "Skill",
            "SlashCommand",
            "ExitPlanMode",
            "EnterWorktree",
            "LSP",
            "ReadMcpResourceTool",
            "ListMcpResourcesTool",
            "PowerShell",
            "Artifact",
            "apply_patch",
            "exec_command",
            "shell",
            "write_stdin",
            "spawn_agent",
            "view_image",
            "mcp__weft_bus__bus_post",
            "mcp__anything__anything",
        ] {
            for engine in ["claude", "codex", "opencode"] {
                assert_eq!(
                    safe_scope(engine, name),
                    None,
                    "{engine}/{name} must still surface the card"
                );
            }
        }
    }

    #[test]
    fn read_inside_a_root_is_contained() {
        let t = TempTree::new("inside");
        let f = t.file("src/main.rs");
        assert!(paths_contained(
            Some(&json!({ "file_path": f })),
            &t.roots()
        ));
    }

    #[test]
    fn read_outside_every_root_is_refused() {
        let t = TempTree::new("outside");
        let other = TempTree::new("outside-other");
        let f = other.file("secrets.txt");
        assert!(!paths_contained(
            Some(&json!({ "file_path": f })),
            &t.roots()
        ));
        // ...including the classic absolute targets.
        assert!(!paths_contained(
            Some(&json!({ "file_path": "/etc/hosts" })),
            &t.roots()
        ));
    }

    #[test]
    fn sibling_root_prefix_is_not_containment() {
        // `/x/repo-evil` must not count as inside `/x/repo` just because the
        // string starts with it. Component-wise `starts_with` is what saves us.
        let base = TempTree::new("prefix");
        let root = base.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let evil = base.0.join("repo-evil");
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(evil.join("f.txt"), b"x").unwrap();
        assert!(!paths_contained(
            Some(&json!({ "file_path": evil.join("f.txt").to_string_lossy() })),
            &[root]
        ));
    }

    #[test]
    fn dotdot_traversal_out_of_a_root_is_refused() {
        let base = TempTree::new("dotdot");
        let root = base.0.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        let outside = base.file("outside.txt");
        let traversal = root.join("..").join("outside.txt");
        assert!(std::fs::metadata(&traversal).is_ok(), "target exists");
        assert!(
            !paths_contained(
                Some(&json!({ "file_path": traversal.to_string_lossy() })),
                &[root]
            ),
            "canonicalization must collapse `..` before the prefix check, got {outside}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_a_root_is_refused() {
        // The containment property has to hold for REAL locations: a repo (or
        // an agent, via an earlier approved write) can plant a symlink inside
        // the worktree that points anywhere.
        let base = TempTree::new("symlink");
        let root = base.0.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        let secret = base.file("id_rsa");
        let link = root.join("innocent.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(
            !paths_contained(
                Some(&json!({ "file_path": link.to_string_lossy() })),
                &[root.clone()]
            ),
            "a symlink out of the root must not be auto-approved"
        );
        // ...while a symlink that stays inside is still fine.
        let inner = root.join("real.txt");
        std::fs::write(&inner, b"x").unwrap();
        let inner_link = root.join("alias.txt");
        std::os::unix::fs::symlink(&inner, &inner_link).unwrap();
        assert!(paths_contained(
            Some(&json!({ "file_path": inner_link.to_string_lossy() })),
            &[root]
        ));
    }

    #[test]
    fn relative_and_tilde_targets_are_refused() {
        let t = TempTree::new("relative");
        t.file("src/main.rs");
        // Relative: weft would have to guess the base directory to resolve it.
        assert!(!paths_contained(
            Some(&json!({ "file_path": "src/main.rs" })),
            &t.roots()
        ));
        // `~` is not expanded by `Path`, so it is not absolute — rule 1 must
        // still refuse it rather than letting rule 2 skip past it.
        assert!(!paths_contained(
            Some(&json!({ "file_path": "~/.ssh/id_rsa" })),
            &t.roots()
        ));
    }

    #[test]
    fn non_string_target_is_refused() {
        let t = TempTree::new("nonstring");
        for bad in [json!(42), json!(null), json!(["/a", "/b"]), json!({})] {
            assert!(
                !paths_contained(Some(&json!({ "file_path": bad })), &t.roots()),
                "a target key that isn't a string must fail closed"
            );
        }
    }

    #[test]
    fn missing_target_key_is_fine_but_stray_absolute_paths_are_not() {
        let t = TempTree::new("missing");
        // Grep/Glob without `path` search the session's own cwd — a root.
        assert!(paths_contained(
            Some(&json!({ "pattern": "TODO", "output_mode": "content" })),
            &t.roots()
        ));
        // But an absolute path under ANY key — including one this module
        // doesn't know — must still be contained (rule 2).
        assert!(!paths_contained(
            Some(&json!({ "pattern": "TODO", "some_future_key": "/etc/passwd" })),
            &t.roots()
        ));
        // ...and nested inside arrays/objects, too.
        assert!(!paths_contained(
            Some(&json!({ "opts": { "extra_dirs": ["/etc"] } })),
            &t.roots()
        ));
    }

    #[test]
    fn multiple_roots_each_admit_their_own_files() {
        // A direction can own one worktree per repo; a file in ANY of them is
        // inside the session.
        let a = TempTree::new("multi-a");
        let b = TempTree::new("multi-b");
        let outside = TempTree::new("multi-c");
        let roots = vec![a.0.clone(), b.0.clone()];
        for f in [a.file("x.rs"), b.file("y.rs")] {
            assert!(paths_contained(Some(&json!({ "file_path": f })), &roots));
        }
        assert!(!paths_contained(
            Some(&json!({ "file_path": outside.file("z.rs") })),
            &roots
        ));
    }

    /// An unresolvable session (stale direction, cross-thread route, deleted
    /// worktree) auto-approves NOTHING path-scoped — including the forms that
    /// name no path at all.
    ///
    /// The targetless case is the one that matters and the one this test
    /// originally got WRONG: it asserted that `Grep {"pattern":"x"}` was fine
    /// with no roots, reasoning that a call naming nothing has nothing to
    /// contain. But it does have a target — the engine's cwd — and "the cwd is
    /// one of our roots" is exactly what an empty `roots` means we could not
    /// establish. Scanning arguments finds nothing to reject, so the identity
    /// check in `session_roots` silently became a no-op for the very routes it
    /// guards. Codex caught it in review on PR #146.
    #[test]
    fn empty_roots_auto_approve_nothing_at_all() {
        let t = TempTree::new("noroots");
        let f = t.file("a.rs");
        assert!(
            !paths_contained(Some(&json!({ "file_path": f })), &[]),
            "an unresolvable session must not auto-approve a path-scoped read"
        );
        for targetless in [
            json!({ "pattern": "x" }),
            json!({ "pattern": "x", "output_mode": "content" }),
            json!({}),
        ] {
            assert!(
                !paths_contained(Some(&targetless), &[]),
                "a cwd-defaulting call must not be approved against an \
                 unverified cwd: {targetless}"
            );
        }
        assert!(!paths_contained(None, &[]));
    }

    /// The mirror of the above, so the empty-roots refusal can't be "fixed" by
    /// something that also breaks the ordinary cwd-defaulting call: with a
    /// RESOLVED session, naming no path is still fine.
    #[test]
    fn targetless_call_is_fine_once_the_session_resolves() {
        let t = TempTree::new("targetless-ok");
        assert!(paths_contained(
            Some(&json!({ "pattern": "TODO" })),
            &t.roots()
        ));
        assert!(paths_contained(None, &t.roots()));
    }

    #[test]
    fn nonexistent_path_is_refused() {
        let t = TempTree::new("missing-file");
        let ghost = t.0.join("never-written.rs");
        assert!(
            !paths_contained(
                Some(&json!({ "file_path": ghost.to_string_lossy() })),
                &t.roots()
            ),
            "containment can't be established for a path that isn't there"
        );
    }
}
