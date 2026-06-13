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
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let Some(root) = repo_root(cwd) else {
        return;
    };
    ensure_codex_trusted_in(
        &PathBuf::from(&home).join(".codex").join("config.toml"),
        &root,
    );
}

pub fn ensure_codex_hook() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    ensure_codex_hook_in(
        &home.join(".codex").join("config.toml"),
        &home.join(".weft").join("weft-codex-hook.sh"),
    );
}

/// The git repository root Codex trusts (a worktree → its main repo root).
fn repo_root(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
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

/// The raw `bash <helper>` command Weft writes into Codex's `hooks.PreToolUse`.
/// Returned UNescaped: `toml_edit` owns TOML string escaping when it serializes
/// this value, so escaping here too would double-escape backslashes on Windows
/// paths (`C:\x` → `C:\\\\x`). Centralized so production and tests share one
/// source for the command string, staying correct on any path separator.
fn codex_hook_command(helper: &Path) -> String {
    format!("bash {}", helper.to_string_lossy())
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
    let helper_body = r#"#!/usr/bin/env bash
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
    resp="$(curl -s -m 3600 -X POST "$url" -H 'Content-Type: application/json' --data-binary @- 2>/dev/null)"
    [ -n "$resp" ] && printf '%s' "$resp"
    exit 0
  fi
  [ "$dir" = "/" ] && exit 0
  next="$(dirname "$dir")"
  [ "$next" = "$dir" ] && exit 0
  dir="$next"
done
"#;
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

    // Resolve / create the `hooks.PreToolUse` array. `doc["hooks"]` covers both the
    // dotted form (`hooks.PreToolUse = [..]`, stored as an implicit table) and the
    // table form (`[hooks]` then `PreToolUse = [..]`); `as_table_like_mut` also
    // covers an inline `hooks = { .. }`. We index via accessors (not `doc[..]`,
    // which panics on a missing key) since production paths must not panic.
    let root = doc.as_table_mut();
    let hooks_item = root
        .entry("hooks")
        .or_insert_with(toml_edit::table); // a `[hooks]` table if absent
    let Some(hooks) = hooks_item.as_table_like_mut() else {
        return; // `hooks` exists but isn't a table/inline-table — leave it be.
    };

    let array = match hooks.get_mut("PreToolUse") {
        Some(item) => match item.as_array_mut() {
            Some(arr) => arr, // dotted or table form: edit the existing array in place
            None => return,   // present but not an array — don't corrupt it.
        },
        None => {
            // `[hooks]` table (or inline) with no PreToolUse: add the array INTO it,
            // never a conflicting top-level dotted `hooks.PreToolUse` key.
            hooks.insert(
                "PreToolUse",
                toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())),
            );
            let Some(item) = hooks.get_mut("PreToolUse") else {
                return;
            };
            let Some(arr) = item.as_array_mut() else {
                return;
            };
            arr
        }
    };

    // Idempotence: if any element already carries `command` in a nested
    // `hooks[].command`, do nothing. Compare the PARSED (unescaped) value against
    // the raw command so the check is correct on Windows paths too — comparing
    // serialized text would mismatch toml_edit's backslash escaping.
    let already_present = array.iter().any(|el| {
        el.as_inline_table()
            .and_then(|t| t.get("hooks"))
            .and_then(|h| h.as_array())
            .map(|inner| {
                inner.iter().any(|ih| {
                    ih.as_inline_table()
                        .and_then(|t| t.get("command"))
                        .and_then(|c| c.as_str())
                        == Some(command.as_str())
                })
            })
            .unwrap_or(false)
    });
    if already_present {
        return;
    }

    // Append Weft's entry, equivalent to:
    //   { matcher = ".*", hooks = [{ type = "command", command = "<command>", timeout = 3650 }] }
    // Built from toml_edit values so it serializes with correct escaping.
    let mut inner = toml_edit::InlineTable::new();
    inner.insert("type", toml_edit::Value::from("command"));
    inner.insert("command", toml_edit::Value::from(command));
    inner.insert("timeout", toml_edit::Value::from(3650));
    let mut hooks_arr = toml_edit::Array::new();
    hooks_arr.push(toml_edit::Value::InlineTable(inner));
    let mut entry = toml_edit::InlineTable::new();
    entry.insert("matcher", toml_edit::Value::from(".*"));
    entry.insert("hooks", toml_edit::Value::Array(hooks_arr));
    array.push(toml_edit::Value::InlineTable(entry));

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
    fn noop_when_no_config() {
        let base = std::env::temp_dir().join(format!("weft-codex-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        ensure_codex_trusted_in(&cfg, "/x");
        assert!(!cfg.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
