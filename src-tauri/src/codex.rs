//! Codex folder-trust + stable hook integration.
//!
//! Codex prompts "Do you trust this folder?" on first run in an untrusted repo
//! and blocks there, which stalls an unattended weft worker. Codex trust is keyed
//! by the *git repository root* (a worktree resolves to its main repo), stored in
//! ~/.codex/config.toml as `[projects."<root>"] trust_level = "trusted"`. We
//! pre-accept exactly that — a startup gate, not a per-action permission.
//!
//! Codex also requires hook-source trust. We do NOT pass
//! `--dangerously-bypass-hook-trust`; instead we install one stable
//! Weft-managed global hook in an existing Codex config and route only Weft
//! worktrees through it via a local `.weft-codex-ask-url` file. The config edit
//! is structural (via `toml_edit`): Weft appends its entry into the existing
//! `hooks.PreToolUse` array — resolving the dotted (`hooks.PreToolUse = [..]`)
//! and table (`[hooks]` + `PreToolUse = [..]`) forms to the same logical path —
//! or creates that array if absent. Codex runs matchers in sequence, so the Ask
//! Bridge stays active alongside any user hooks. A config that can't be parsed
//! is never overwritten, and no config is fabricated if Codex was never set up.

use std::path::{Path, PathBuf};

pub fn ensure_codex_trusted(cwd: &Path) {
    let Some(home) = codex_home() else {
        return;
    };
    let Some(root) = repo_root(cwd) else {
        return;
    };
    ensure_codex_trusted_in(&home.join(".codex").join("config.toml"), &root);
}

pub fn ensure_codex_hook() {
    let Some(home) = codex_home() else {
        return;
    };
    // One stable hook path across build profiles: `~/.codex/config.toml` is the
    // user's single global Codex config (shared by dev and release), and
    // `ensure_codex_hook_in` dedupes by exact command string. A profile-specific
    // path would append a second matching entry and double every permission ask,
    // so the hook script is intentionally NOT isolated per profile.
    ensure_codex_hook_in(
        &home.join(".codex").join("config.toml"),
        &home.join(".weft").join("weft-codex-hook.sh"),
    );
}

/// The user's home directory, where Codex keeps `~/.codex/config.toml`. Resolved
/// platform-awarely: on a Windows GUI launch `HOME` is unset (the profile lives at
/// `%USERPROFILE%`), so a bare `env::var("HOME")` would skip hook install there.
/// `dirs::home_dir()` already consults the right per-platform source.
fn codex_home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// The git repository root Codex trusts (a worktree → its main repo root).
fn repo_root(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git").env("PATH", crate::detect::tool_path())
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let gitdir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = PathBuf::from(&gitdir); // e.g. /repo/.git
    Some(p.parent()?.to_string_lossy().to_string())
}

fn ensure_codex_trusted_in(cfg: &Path, root: &str) {
    let Ok(text) = std::fs::read_to_string(cfg) else {
        return; // Codex not set up — don't fabricate a config.
    };
    let key = format!(
        "[projects.\"{}\"]",
        root.replace('\\', "\\\\").replace('"', "\\\"")
    );
    if text.contains(&key) {
        return; // already trusted
    }
    let mut next = text;
    if !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!("\n{key}\ntrust_level = \"trusted\"\n"));
    write_atomic(cfg, next.as_bytes());
}

/// The `bash <helper>` command Weft writes into Codex's `hooks.PreToolUse`. The
/// helper path is SHELL-quoted (single quotes, with embedded `'` escaped as
/// `'\''`) because this string is handed to a shell to run: an unquoted path with
/// a space or metacharacter (e.g. `C:\Users\Jane Doe\…`) would be word-split and
/// the Ask Bridge would never start. This quoting is for the shell only — the
/// TOML-string escaping of the whole command value is still `toml_edit`'s job, so
/// nothing is pre-escaped for TOML here (that would double-escape backslashes on
/// Windows paths). Centralized so production and the dedup check share one source.
fn codex_hook_command(helper: &Path) -> String {
    format!(
        "bash '{}'",
        helper.to_string_lossy().replace('\'', "'\\''")
    )
}

/// True if a PreToolUse entry's nested `hooks` already names `command`. The entry
/// is table-like (an inline table from a value-array, or a `[[..]]` table), and
/// its `hooks` may itself be a value-array of inline tables (`hooks = [{..}]`) or
/// an array-of-tables (`[[hooks.PreToolUse.hooks]]`) — both are scanned. Compares
/// the PARSED (unescaped) command so it stays correct on Windows paths.
fn entry_has_command(entry: &dyn toml_edit::TableLike, command: &str) -> bool {
    let Some(hooks) = entry.get("hooks") else {
        return false;
    };
    // Value-array form: hooks = [{ type = "command", command = ".." }, ..]
    if let Some(arr) = hooks.as_array() {
        if arr.iter().any(|v| {
            v.as_inline_table()
                .and_then(|t| t.get("command"))
                .and_then(|c| c.as_str())
                == Some(command)
        }) {
            return true;
        }
    }
    // Array-of-tables form: [[hooks.PreToolUse.hooks]] type=".." command=".."
    if let Some(aot) = hooks.as_array_of_tables() {
        if aot
            .iter()
            .any(|t| t.get("command").and_then(|c| c.as_str()) == Some(command))
        {
            return true;
        }
    }
    false
}

/// The single inner command hook `{ type = "command", command = "<command>",
/// timeout = 3650 }`, built from toml_edit values so escaping is correct.
fn weft_inner_command_hook(command: &str) -> toml_edit::InlineTable {
    let mut inner = toml_edit::InlineTable::new();
    inner.insert("type", toml_edit::Value::from("command"));
    inner.insert("command", toml_edit::Value::from(command));
    inner.insert("timeout", toml_edit::Value::from(3650));
    inner
}

/// Weft's full PreToolUse entry as an inline table, for the value-array forms:
/// `{ matcher = ".*", hooks = [{ type = "command", command = "<command>", timeout = 3650 }] }`.
///
/// The `.*` matcher is deliberate and must stay total: a matcher is a positive
/// filter, so any tool name it fails to match is not "asked about later" — the
/// hook never sees it and it runs UNGATED. Skipping the human for safe
/// read-only tools is decided in `bus::builtin_allow`, on a closed allowlist
/// where an unrecognized name falls through to the Needs-you card instead.
fn weft_entry_inline(command: &str) -> toml_edit::InlineTable {
    let mut hooks_arr = toml_edit::Array::new();
    hooks_arr.push(toml_edit::Value::InlineTable(weft_inner_command_hook(command)));
    let mut entry = toml_edit::InlineTable::new();
    entry.insert("matcher", toml_edit::Value::from(".*"));
    entry.insert("hooks", toml_edit::Value::Array(hooks_arr));
    entry
}

/// Prefix every non-empty line of `text` with `prefix`. The shared hook tail is
/// authored flush-left (claude's script has no nesting); Codex's copy is spliced
/// inside a `while` loop, so it gets re-indented rather than being duplicated at
/// a second indent level.
fn indent_lines(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_codex_hook_in(cfg: &Path, helper: &Path) {
    let Ok(text) = std::fs::read_to_string(cfg) else {
        return; // Codex not set up — don't fabricate a config.
    };
    if let Some(parent) = helper.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // `__DECIDE_OR_DENY__` is spliced (indented) with the shared fail-closed tail
    // claude's per-worktree hook also uses — one source for "pass weft's decision
    // through, otherwise deny explicitly".
    let helper_template = r#"#!/usr/bin/env bash
dir="${PWD:-.}"
while :; do
  route="$dir/.weft-codex-ask-url"
  if [ -f "$route" ]; then
    url="$(cat "$route" 2>/dev/null)"
    # SECURITY: a repo can plant a .weft-codex-ask-url, so only Weft's local endpoint
    # (always http://127.0.0.1:<port>) is trusted. PARSE the real host — strip scheme,
    # path, and any userinfo — rather than glob-matching the raw string: a glob on
    # "http://127.0.0.1:*" is defeated by http://127.0.0.1:80@attacker.example, whose
    # actual host is attacker.example. Non-http or non-loopback → exit without posting.
    # These two guards stay PASS-THROUGH (exit 0, no output), NOT deny: the route file
    # is attacker-controlled here, so denying would let any repo brick Codex sessions
    # by planting one. Weft is not involved in that case; Codex's own approval flow
    # stays authoritative, exactly as in a plain Codex session.
    case "$url" in
      http://*) ;;
      *) exit 0 ;;
    esac
    rest="${url#http://}"
    authority="${rest%%/*}"      # drop path/query
    hostport="${authority##*@}"  # drop userinfo (user:pass@)
    host="${hostport%%:*}"       # drop :port
    case "$host" in
      127.0.0.1|localhost) ;;
      *) exit 0 ;;
    esac
    resp="$(curl -sf -m 3600 -X POST "$url" -H 'Content-Type: application/json' --data-binary @- 2>/dev/null)"
    rc=$?
__DECIDE_OR_DENY__
  fi
  # No route file anywhere up the tree: this is NOT a Weft worktree. The hook is
  # global (~/.codex/config.toml), so it also runs for the user's own hand-started
  # Codex sessions — those must stay untouched (exit 0, no output), never denied.
  [ "$dir" = "/" ] && exit 0
  next="$(dirname "$dir")"
  [ "$next" = "$dir" ] && exit 0
  dir="$next"
done
"#;
    let helper_body = helper_template.replace(
        "__DECIDE_OR_DENY__",
        &indent_lines(crate::bus::inject::HOOK_DECIDE_OR_DENY, "    "),
    );
    if std::fs::write(helper, helper_body.as_bytes()).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(helper, std::fs::Permissions::from_mode(0o755));
    }

    let command = codex_hook_command(helper);

    // Migrate any legacy standalone Weft-managed block out of `text` FIRST, then
    // edit structurally. Codex runs `hooks.PreToolUse` matchers in sequence, so
    // Weft coexists with a user's own entries by appending into the SAME array —
    // a second top-level `hooks.PreToolUse` would be a TOML duplicate-key error.
    let base = if text.contains("# BEGIN WEFT MANAGED CODEX HOOK") {
        remove_managed_block(&text)
    } else {
        text.clone()
    };

    // Never overwrite a config we can't parse: a manual save mid-edit, or a form
    // toml_edit rejects, must be left exactly as the user left it.
    let Ok(mut doc) = base.parse::<toml_edit::DocumentMut>() else {
        return;
    };

    // Resolve / create the `hooks.PreToolUse` collection. Indexing `hooks` covers
    // the dotted form (`hooks.PreToolUse = [..]`, stored as an implicit table), the
    // table form (`[hooks]` then `PreToolUse = [..]`), and an inline `hooks = {..}`
    // — all via `as_table_like_mut`. The value itself can be a value-array of inline
    // tables OR the documented `[[hooks.PreToolUse]]` array-of-tables; we handle
    // both, appending Weft's entry so the Ask Bridge runs alongside user hooks.
    // All accessors are non-panicking (no `doc[..]`, which expects/panics).
    let root = doc.as_table_mut();
    let hooks_item = root
        .entry("hooks")
        .or_insert_with(toml_edit::table); // a `[hooks]` table if absent
    let Some(hooks) = hooks_item.as_table_like_mut() else {
        return; // `hooks` exists but isn't a table/inline-table — leave it be.
    };

    match hooks.get_mut("PreToolUse") {
        // Array-of-tables form: `[[hooks.PreToolUse]]`. Dedup over the existing
        // `[[..]]` entries, then append a new table whose `hooks` is a value-array
        // of one inline command hook (valid inside an array-of-tables entry).
        Some(item) if item.is_array_of_tables() => {
            let Some(aot) = item.as_array_of_tables_mut() else {
                return;
            };
            if aot.iter().any(|t| entry_has_command(t, &command)) {
                return;
            }
            let mut hooks_arr = toml_edit::Array::new();
            hooks_arr.push(toml_edit::Value::InlineTable(weft_inner_command_hook(&command)));
            let mut entry = toml_edit::Table::new();
            entry.insert("matcher", toml_edit::value(".*"));
            entry.insert(
                "hooks",
                toml_edit::Item::Value(toml_edit::Value::Array(hooks_arr)),
            );
            aot.push(entry);
        }
        // Value-array form (dotted or table inline array). Dedup over elements, then
        // append Weft's entry as an inline table.
        Some(item) => {
            let Some(array) = item.as_array_mut() else {
                return; // present but neither array nor array-of-tables — don't corrupt it.
            };
            if array
                .iter()
                .filter_map(|el| el.as_inline_table())
                .any(|t| entry_has_command(t, &command))
            {
                return;
            }
            array.push(toml_edit::Value::InlineTable(weft_entry_inline(&command)));
        }
        // Absent: create a value-array holding Weft's entry, INSIDE the hooks table
        // (never a conflicting top-level dotted `hooks.PreToolUse` key).
        None => {
            let mut array = toml_edit::Array::new();
            array.push(toml_edit::Value::InlineTable(weft_entry_inline(&command)));
            hooks.insert(
                "PreToolUse",
                toml_edit::Item::Value(toml_edit::Value::Array(array)),
            );
        }
    }

    // Untouched formatting/comments are preserved by toml_edit. Only write when the
    // serialized document actually differs from the file we read.
    let next = doc.to_string();
    if next != text {
        write_atomic(cfg, next.as_bytes());
    }
}

fn remove_managed_block(text: &str) -> String {
    const BEGIN: &str = "# BEGIN WEFT MANAGED CODEX HOOK";
    const END: &str = "# END WEFT MANAGED CODEX HOOK";
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == BEGIN {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim() == END {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("toml.weft-tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_when_absent_preserving_existing() {
        let base = std::env::temp_dir().join(format!("weft-codex-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        std::fs::write(
            &cfg,
            "# my config\nmodel = \"gpt-5\"\n\n[projects.\"/existing\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();

        ensure_codex_trusted_in(&cfg, "/private/tmp/weft-d-web");
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("# my config")); // preserved
        assert!(after.contains("[projects.\"/existing\"]")); // preserved
        assert!(after.contains("[projects.\"/private/tmp/weft-d-web\"]"));
        assert!(after.matches("trust_level = \"trusted\"").count() == 2);

        // idempotent
        ensure_codex_trusted_in(&cfg, "/private/tmp/weft-d-web");
        let after2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(after, after2);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Count the logical `hooks.PreToolUse` array elements via a TOML parse —
    /// separator-agnostic across dotted and table forms (a string scan can't see
    /// either reliably).
    fn pretooluse_len(after: &str) -> usize {
        toml::from_str::<toml::Value>(after)
            .ok()
            .and_then(|v| {
                v.get("hooks")
                    .and_then(|h| h.get("PreToolUse"))
                    .and_then(|p| p.as_array())
                    .map(|a| a.len())
            })
            .unwrap_or(0)
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("weft-codex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn no_existing_hooks() {
        let base = fresh_dir("no-hooks");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        std::fs::write(&cfg, "# mine\nmodel = \"gpt-5\"\n").unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config must stay valid TOML:\n{after}"
        );
        let v: toml::Value = toml::from_str(&after).unwrap();
        assert!(
            v.get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|p| p.as_array())
                .is_some(),
            "hooks.PreToolUse must exist as an array:\n{after}"
        );
        assert!(after.contains("# mine")); // existing content preserved
        assert!(after.contains(&codex_hook_command(&helper)));

        // Idempotent: a second run leaves the file byte-identical.
        ensure_codex_hook_in(&cfg, &helper);
        assert_eq!(after, std::fs::read_to_string(&cfg).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preserves_user_dotted_hook() {
        let base = fresh_dir("user-dotted");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "hooks.PreToolUse=[{ matcher = \"shell\", hooks = [{ type = \"command\", command = \"/usr/local/bin/my-audit\" }] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config must stay valid TOML:\n{after}"
        );
        // Both the user's hook and Weft's are present.
        assert!(after.contains("/usr/local/bin/my-audit"));
        assert!(after.contains(&codex_hook_command(&helper)));
        // Exactly one logical hooks.PreToolUse, now holding both entries.
        assert_eq!(pretooluse_len(&after), 2, "one array, both entries:\n{after}");
        // Helper routed through the per-worktree ask url.
        assert!(std::fs::read_to_string(&helper)
            .unwrap()
            .contains(".weft-codex-ask-url"));

        // Idempotent: a second run does not duplicate Weft's entry.
        ensure_codex_hook_in(&cfg, &helper);
        let after2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(after, after2);
        assert_eq!(after2.matches(&codex_hook_command(&helper)).count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn splices_into_table_form_hooks() {
        // Finding 157: a `[hooks]` table whose PreToolUse array must ALSO receive
        // Weft's entry, not be left unchanged.
        let base = fresh_dir("table-splice");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "[hooks]\nPreToolUse = [{ matcher = \"shell\", hooks = [] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "table-form config must stay valid TOML:\n{after}"
        );
        assert_ne!(after, before, "table-form PreToolUse must be edited, not skipped");
        assert!(
            after.contains(&codex_hook_command(&helper)),
            "weft command must be spliced into the table-form array:\n{after}"
        );
        assert_eq!(pretooluse_len(&after), 2, "user entry + weft entry:\n{after}");

        // Idempotent.
        ensure_codex_hook_in(&cfg, &helper);
        assert_eq!(after, std::fs::read_to_string(&cfg).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn adds_pretooluse_into_existing_hooks_table() {
        // Finding 288: a `[hooks]` table with a DIFFERENT hook kind (no PreToolUse).
        // PreToolUse must be added INSIDE that table, not as a conflicting top-level
        // dotted key (which would be a duplicate-key TOML error against `[hooks]`).
        let base = fresh_dir("hooks-table-add");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "[hooks]\nUserPromptSubmit = [{ matcher = \".*\", hooks = [] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config must stay valid TOML (no dup key):\n{after}"
        );
        let v: toml::Value = toml::from_str(&after).unwrap();
        let hooks = v.get("hooks").expect("hooks table present");
        // PreToolUse landed inside the [hooks] table...
        assert!(
            hooks.get("PreToolUse").and_then(|p| p.as_array()).is_some(),
            "PreToolUse must be inside the [hooks] table:\n{after}"
        );
        // ...alongside the preserved UserPromptSubmit entry.
        assert!(
            hooks.get("UserPromptSubmit").is_some(),
            "UserPromptSubmit must be preserved:\n{after}"
        );
        assert!(after.contains(&codex_hook_command(&helper)));
        assert_eq!(pretooluse_len(&after), 1, "exactly Weft's entry:\n{after}");

        // Idempotent.
        ensure_codex_hook_in(&cfg, &helper);
        assert_eq!(after, std::fs::read_to_string(&cfg).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn top_level_when_projects_table_present() {
        // A trailing [projects."..."] table (exactly what ensure_codex_trusted
        // appends) must not capture Weft's hook: a proper [hooks] table header keeps
        // it top-level regardless of ordering.
        let base = fresh_dir("toplevel");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "model = \"gpt-5\"\n\n[projects.\"/repo\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config must stay valid TOML:\n{after}"
        );
        let v: toml::Value = toml::from_str(&after).unwrap();
        // hooks.PreToolUse resolvable at TOP level, not nested under projects.
        assert!(
            v.get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|p| p.as_array())
                .is_some(),
            "hooks.PreToolUse must be top-level:\n{after}"
        );
        // The projects table is intact.
        assert!(
            v.get("projects")
                .and_then(|p| p.get("/repo"))
                .and_then(|r| r.get("trust_level"))
                .is_some(),
            "[projects.\"/repo\"] must be intact:\n{after}"
        );
        assert!(after.contains(&codex_hook_command(&helper)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrates_legacy_managed_block() {
        // Legacy standalone managed block PLUS a user hooks.PreToolUse the user
        // added later: keeping both would be a duplicate key. The migration strips
        // the block and folds Weft's CURRENT command into the user's array.
        let base = fresh_dir("migrate-legacy");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let managed = "# BEGIN WEFT MANAGED CODEX HOOK\nhooks.PreToolUse=[{ matcher = \".*\", hooks = [{ type = \"command\", command = \"bash /old/weft-codex-hook.sh\", timeout = 3650 }] }]\n# END WEFT MANAGED CODEX HOOK\n";
        let user = "hooks.PreToolUse=[{ matcher = \"shell\", hooks = [{ type = \"command\", command = \"/usr/local/bin/my-audit\" }] }]\n";
        std::fs::write(&cfg, format!("{managed}{user}")).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "must stay valid TOML (no duplicate key):\n{after}"
        );
        assert!(!after.contains("# BEGIN WEFT MANAGED CODEX HOOK")); // markers gone
        assert!(!after.contains("# END WEFT MANAGED CODEX HOOK"));
        assert!(!after.contains("/old/weft-codex-hook.sh")); // stale command dropped
        // Exactly one logical array, holding the user hook + the CURRENT weft command.
        assert_eq!(pretooluse_len(&after), 2, "one array, both entries:\n{after}");
        assert!(after.contains("/usr/local/bin/my-audit"));
        assert!(after.contains(&codex_hook_command(&helper)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn helper_has_loopback_host_guard() {
        // The P1 host-validation guard in the helper script must not be silently
        // dropped by a refactor of the config-editing path.
        let base = fresh_dir("helper-guard");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        std::fs::write(&cfg, "model = \"gpt-5\"\n").unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let helper_text = std::fs::read_to_string(&helper).unwrap();
        assert!(helper_text.contains("127.0.0.1")); // loopback-only allow
        assert!(
            helper_text.contains("hostport") && helper_text.contains("${rest%%/*}"),
            "the host-parsing guard must be present:\n{helper_text}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn splices_into_array_of_tables_hooks() {
        // The documented `[[hooks.PreToolUse]]` array-of-tables layout: Weft's entry
        // must be appended (a new `[[hooks.PreToolUse]]` table), not skipped.
        let base = fresh_dir("aot-splice");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "[[hooks.PreToolUse]]\nmatcher = \"shell\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"/usr/local/bin/my-audit\"\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "array-of-tables config must stay valid TOML:\n{after}"
        );
        assert_ne!(after, before, "array-of-tables PreToolUse must be edited, not skipped");
        // User entry preserved, Weft's command appended into the SAME logical array.
        assert!(after.contains("/usr/local/bin/my-audit"), "user hook preserved:\n{after}");
        assert!(
            after.contains(&codex_hook_command(&helper)),
            "weft command must be appended to the array-of-tables:\n{after}"
        );
        assert_eq!(pretooluse_len(&after), 2, "user entry + weft entry:\n{after}");

        // Idempotent: a second run does not append again.
        ensure_codex_hook_in(&cfg, &helper);
        let after2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(after, after2);
        assert_eq!(after2.matches(&codex_hook_command(&helper)).count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quotes_helper_path_with_spaces() {
        // A helper path with a space must be single-quoted in the shell command so
        // `bash` receives one argument, and the whole config must stay valid TOML.
        let base = fresh_dir("spaced-path");
        let spaced = base.join("Jane Doe").join(".weft");
        std::fs::create_dir_all(&spaced).unwrap();
        let cfg = base.join("config.toml");
        let helper = spaced.join("weft-codex-hook.sh");
        assert!(helper.to_string_lossy().contains(' ')); // precondition: path has a space
        std::fs::write(&cfg, "model = \"gpt-5\"\n").unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config with a spaced helper path must stay valid TOML:\n{after}"
        );
        // The command embeds the single-quoted path: bash '<path with space>'.
        let command = codex_hook_command(&helper);
        assert!(command.starts_with("bash '") && command.ends_with('\''));
        assert!(command.contains(&format!("'{}'", helper.to_string_lossy())));
        // And it round-trips out of the config as that exact quoted string.
        let v: toml::Value = toml::from_str(&after).unwrap();
        let got = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(got, command, "stored command must equal the quoted command:\n{after}");

        // An embedded single quote is escaped as '\'' (no panic, still valid TOML).
        let tricky = base.join("a'b").join("weft-codex-hook.sh");
        let tricky_cmd = codex_hook_command(&tricky);
        assert!(tricky_cmd.contains("'\\''"), "embedded quote must be escaped: {tricky_cmd}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn noop_when_no_config() {
        let base = std::env::temp_dir().join(format!("weft-codex-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        ensure_codex_trusted_in(&cfg, "/x");
        assert!(!cfg.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn helper_splices_the_shared_fail_closed_tail() {
        // The tail lives once (bus::inject::HOOK_DECIDE_OR_DENY) and is shared with
        // claude's per-worktree hook; a dropped splice would silently restore the
        // fail-open `[ -n "$resp" ] && printf …` behavior.
        let base = fresh_dir("helper-tail");
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        std::fs::write(&cfg, "model = \"gpt-5\"\n").unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let helper_text = std::fs::read_to_string(&helper).unwrap();
        assert!(
            !helper_text.contains("__DECIDE_OR_DENY__"),
            "the splice marker must be substituted:\n{helper_text}"
        );
        assert!(
            helper_text.contains("\"permissionDecision\":\"deny\""),
            "the explicit-deny fallback must be present:\n{helper_text}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    const HOOK_PAYLOAD: &str = r#"{"tool_name":"shell","tool_input":{"command":"rm -rf /"}}"#;

    /// Every run below is bounded by the shared runner, which KILLS the script at
    /// the deadline (see `hook_test_support::run_hook_script`).
    #[cfg(unix)]
    const HOOK_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Write the global helper, then run it from a stand-in worktree exactly as
    /// Codex does: PreToolUse JSON on stdin, decision JSON on stdout. Shares one
    /// runner with claude's hook test so the two can't drift.
    #[cfg(unix)]
    async fn run_helper(tag: &str, route_url: Option<&str>, nested: bool) -> (String, Option<i32>) {
        let base = fresh_dir(tag);
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        std::fs::write(&cfg, "model = \"gpt-5\"\n").unwrap();
        ensure_codex_hook_in(&cfg, &helper);

        // The route file (when any) sits at the worktree root, and the hook runs
        // from a nested dir so the walk-up path is exercised too.
        let wt = base.join("wt");
        let run_in = if nested {
            wt.join("pkg").join("src")
        } else {
            wt.clone()
        };
        std::fs::create_dir_all(&run_in).unwrap();
        if let Some(url) = route_url {
            std::fs::write(wt.join(".weft-codex-ask-url"), url).unwrap();
        }

        let out =
            crate::hook_test_support::run_hook_script(&helper, &run_in, HOOK_PAYLOAD, HOOK_LIMIT)
                .await;
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    /// Codex's own hook contract: "exit 0 with no output is treated as success and
    /// Codex continues." So the pre-existing `[ -n "$resp" ] && printf …; exit 0`
    /// ALLOWED every tool call whenever weft wasn't answering. It must deny.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_denies_when_weft_is_unreachable() {
        use crate::hook_test_support::{closed_port, decision_of};
        let url = format!("http://127.0.0.1:{}/ask/2/30?tool=codex", closed_port());
        let (stdout, code) = run_helper("hook-down", Some(&url), true).await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "unreachable weft must fail CLOSED, not continue: {out}"
        );
        assert!(
            out["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("could not be reached"),
            "the reason must tell the human weft is down: {out}"
        );
    }

    /// The other side of the seam: a reachable weft's real answer must reach Codex
    /// unchanged, so "deny everything" can't pass the test above.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_passes_a_real_weft_decision_through() {
        use crate::ask::{Answer, AskRegistry};
        use crate::hook_test_support::{answer_first_ask, decision_of};
        let asks = AskRegistry::new();
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "codex-hook")
            .await
            .unwrap();
        let repo = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/codex-hook-repo",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = crate::store::repo::create_thread(
            &db,
            workspace.id,
            "issue",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        let direction = crate::store::repo::create_direction(
            &db,
            thread.id,
            "task",
            "codex",
            repo.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = crate::store::repo::create_session(
            &db,
            direction.id,
            repo.id,
            "codex",
            "/tmp/codex-hook-session",
        )
        .await
        .unwrap();
        let (base, _h) =
            crate::bus::server::serve(crate::bus::BusRegistry::new(), db.clone(), asks.clone())
                .await
                .unwrap();

        // One task, two concurrent futures: the hook runs while the "human"
        // answers. No detached task to outlive the test, and the runner kills the
        // script if it somehow never exits.
        let url = format!(
            "{base}/ask/{}/{}?tool=codex&session_id={}",
            thread.id, direction.id, session.id
        );
        let ((stdout, code), ()) = tokio::join!(
            run_helper("hook-live", Some(&url), true),
            answer_first_ask(&asks, Answer::Allow),
        );
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "allow",
            "a human's Allow must pass through untouched: {out}"
        );
        assert_eq!(out["permissionDecisionReason"], "Approved in weft");
    }

    /// Review round 2 (P1), codex's own curl: `-f` is per-script, so the error-status
    /// gate is verified on this hook too, not only on claude's.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_denies_a_decision_shaped_error_response() {
        use crate::hook_test_support::{decision_body, decision_of, serve_raw_once};
        let body = decision_body("allow", false);
        let len = body.len();
        let base = serve_raw_once("HTTP/1.1 503 Service Unavailable", body, len).await;
        let url = format!("{base}/ask/2/30?tool=codex");
        let (stdout, code) = run_helper("hook-500", Some(&url), true).await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "an allow carried by a 5xx must not pass through: {out}"
        );
        assert!(
            out["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("error status"),
            "the reason must name the error status: {out}"
        );
    }

    /// Review round 2 (P1): same truncated-after-the-verdict case as claude's, on
    /// codex's spliced copy of the shared tail.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_denies_an_answer_cut_off_after_the_verdict() {
        use crate::hook_test_support::{decision_body, decision_of, serve_raw_once};
        let base = serve_raw_once("HTTP/1.1 200 OK", decision_body("allow", true), 4096).await;
        let url = format!("{base}/ask/2/30?tool=codex");
        let (stdout, code) = run_helper("hook-cut", Some(&url), true).await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "a truncated allow must not pass through: {out}"
        );
    }

    /// The deliberate exception to fail-closed. This hook is GLOBAL
    /// (`~/.codex/config.toml`), so it also runs for the user's own hand-started
    /// Codex sessions; with no `.weft-codex-ask-url` anywhere up the tree, weft
    /// isn't involved and the session must be left completely alone.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_stays_silent_outside_a_weft_worktree() {
        let (stdout, code) = run_helper("hook-foreign", None, true).await;
        assert_eq!(code, Some(0));
        assert!(
            stdout.is_empty(),
            "a non-weft Codex session must be untouched, got: {stdout:?}"
        );
    }

    /// The second exception: a route file is repo-plantable, so a non-loopback one
    /// is IGNORED (silent pass-through), not denied — otherwise any repo could
    /// brick Codex by planting a file. Doubles as a behavioral check on the
    /// userinfo-stripping host guard: the real host here is 127.0.0.2, and if the
    /// guard regressed, curl would fail against a closed port and the fail-closed
    /// tail would print a deny — making this assertion fail instead of silently
    /// posting the payload off-box.
    #[tokio::test]
    #[cfg(unix)]
    async fn codex_hook_ignores_a_non_loopback_route_without_posting() {
        use crate::hook_test_support::closed_port;
        let url = format!(
            "http://127.0.0.1:{}@127.0.0.2:{}/ask/2/30?tool=codex",
            closed_port(),
            closed_port()
        );
        let (stdout, code) = run_helper("hook-planted", Some(&url), false).await;
        assert_eq!(code, Some(0));
        assert!(
            stdout.is_empty(),
            "a planted non-loopback route must be ignored, not denied or posted to, got: {stdout:?}"
        );
    }
}
