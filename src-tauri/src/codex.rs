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
//! worktrees through it via a local `.weft-codex-ask-url` file. If the user
//! already declares their own `hooks.PreToolUse`, Weft splices its entry into
//! that array (Codex runs matchers in sequence) instead of skipping — so the
//! Ask Bridge stays active alongside user hooks. No config is fabricated if
//! Codex was never set up.

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

/// The `bash <helper>` command Weft writes into Codex's `hooks.PreToolUse`, with
/// backslashes and quotes escaped for TOML (Windows paths). Centralized so tests
/// assert against the exact string production writes, staying correct on any
/// path separator.
fn codex_hook_command(helper: &Path) -> String {
    format!(
        "bash {}",
        helper
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
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
    # SECURITY: only post to Weft's local ask endpoint (always http://127.0.0.1:<port>).
    # A repo could plant a .weft-codex-ask-url pointing off-box; refusing non-loopback
    # URLs stops tool-approval payloads from being exfiltrated and stops a remote from
    # driving the approval verdict. Non-loopback (or empty) → exit without posting.
    case "$url" in
      http://127.0.0.1:*|http://127.0.0.1/*|http://localhost:*|http://localhost/*)
        resp="$(curl -s -m 3600 -X POST "$url" -H 'Content-Type: application/json' --data-binary @- 2>/dev/null)"
        [ -n "$resp" ] && printf '%s' "$resp"
        ;;
    esac
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
    // Weft's PreToolUse array element (no top-level key).
    let entry = format!(
        "{{ matcher = \".*\", hooks = [{{ type = \"command\", command = \"{command}\", timeout = 3650 }}] }}"
    );
    let next = if has_user_pretooluse_hook(&text) {
        // The user already declares hooks.PreToolUse. Codex runs PreToolUse
        // matchers in sequence, so Weft coexists by splicing its element into the
        // user's existing array — emitting a second top-level hooks.PreToolUse
        // would be a TOML duplicate-key error and break Codex. If the array can't
        // be located safely, leave the user config untouched.
        //
        // Drop any prior Weft-managed block FIRST: if the user added their own
        // hook after Weft installed the standalone block, keeping both would be the
        // very duplicate key we're avoiding, and the splice's command-already-present
        // check would otherwise short-circuit on the stale managed block.
        let base = if text.contains("# BEGIN WEFT MANAGED CODEX HOOK") {
            remove_managed_block(&text)
        } else {
            text.clone()
        };
        match splice_pretooluse_entry(&base, &entry, &command) {
            Some(spliced) => spliced,
            None => return,
        }
    } else {
        let block = format!(
            "# BEGIN WEFT MANAGED CODEX HOOK\n\
hooks.PreToolUse=[{entry}]\n\
# END WEFT MANAGED CODEX HOOK\n"
        );
        replace_managed_block(&text, &block)
    };
    if next != text {
        write_atomic(cfg, next.as_bytes());
    }
}

/// Splice Weft's PreToolUse `entry` into the user's existing
/// `hooks.PreToolUse = [ ... ]` array (inline or multiline). Returns the edited
/// text, or `None` if no such dotted-array assignment is found (e.g. an
/// array-of-tables form we won't risk rewriting). Idempotent: if `command` is
/// already present anywhere, returns the text unchanged.
fn splice_pretooluse_entry(text: &str, entry: &str, command: &str) -> Option<String> {
    if text.contains(command) {
        return Some(text.to_string()); // already wired in
    }
    const NEEDLE: &str = "hooks.PreToolUse";
    let mut from = 0;
    while let Some(rel) = text[from..].find(NEEDLE) {
        let idx = from + rel;
        let line_start = text[..idx].rfind('\n').map(|n| n + 1).unwrap_or(0);
        if text[line_start..idx].trim_start().starts_with('#') {
            from = idx + NEEDLE.len();
            continue; // commented-out assignment
        }
        let after = &text[idx + NEEDLE.len()..];
        if let Some(eq_rel) = after.find('=') {
            let after_eq = &after[eq_rel + 1..];
            if let Some(br_rel) = after_eq.find('[') {
                if after_eq[..br_rel].trim().is_empty() {
                    let open = idx + NEEDLE.len() + eq_rel + 1 + br_rel;
                    if let Some(close) = matching_bracket(text, open) {
                        let inner = text[open + 1..close].trim();
                        // An empty array, or one that already ends with a trailing
                        // comma (`[ {..}, ]`), needs no leading comma — adding one
                        // would produce a double comma and corrupt the TOML.
                        let insertion = if inner.is_empty() || inner.ends_with(',') {
                            entry.to_string()
                        } else {
                            format!(", {entry}")
                        };
                        let mut out = String::with_capacity(text.len() + insertion.len());
                        out.push_str(&text[..close]);
                        out.push_str(&insertion);
                        out.push_str(&text[close..]);
                        return Some(out);
                    }
                }
            }
        }
        from = idx + NEEDLE.len();
    }
    None
}

/// Index of the `]` matching the `[` at `open`, honoring nesting and strings.
fn matching_bracket(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn replace_managed_block(text: &str, block: &str) -> String {
    const BEGIN: &str = "# BEGIN WEFT MANAGED CODEX HOOK";
    const END: &str = "# END WEFT MANAGED CODEX HOOK";
    let mut out = String::with_capacity(text.len() + block.len() + 2);
    let mut lines = text.lines();
    let mut done = false; // managed block written (replaced in place or inserted)
    while let Some(line) = lines.next() {
        if line.trim() == BEGIN {
            // Replace the existing managed block in place.
            for inner in lines.by_ref() {
                if inner.trim() == END {
                    break;
                }
            }
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            done = true;
            continue;
        }
        // No managed block yet and we've reached the first TOML table header
        // (`[table]` / `[[array]]`): insert the block BEFORE it so hooks.PreToolUse
        // stays a TOP-LEVEL key. A key written after a `[projects."..."]` table
        // (which ensure_codex_trusted appends) would belong to that table instead,
        // silently disabling the Ask Bridge.
        if !done && line.trim_start().starts_with('[') {
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            done = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !done {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(block);
    }
    out
}

fn has_user_pretooluse_hook(text: &str) -> bool {
    let stripped = remove_managed_block(text);
    // Dotted form (`hooks.PreToolUse = [..]`) — lenient string scan, works even on
    // configs the TOML parser would reject.
    if stripped.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.contains("hooks.PreToolUse")
    }) {
        return true;
    }
    // Table form (`[hooks]` then `PreToolUse = [..]`) — detected via a TOML parse so
    // we DON'T insert a conflicting top-level `hooks.PreToolUse` dotted key before an
    // existing `[hooks]` table (which would redefine `hooks` and corrupt the config).
    // The line-based splice only edits the dotted form, so a table-form config is
    // left untouched here rather than corrupted.
    toml::from_str::<toml::Value>(&stripped)
        .ok()
        .and_then(|v| v.get("hooks").and_then(|h| h.get("PreToolUse")).cloned())
        .is_some()
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

    #[test]
    fn appends_managed_hook_alongside_user_pretooluse() {
        let base =
            std::env::temp_dir().join(format!("weft-codex-user-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "hooks.PreToolUse=[{ matcher = \"shell\", hooks = [{ type = \"command\", command = \"/usr/local/bin/my-audit\" }] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        // user's own hook is preserved, Weft's hook is spliced in alongside it
        assert!(after.contains("/usr/local/bin/my-audit"));
        assert!(after.contains(&codex_hook_command(&helper)));
        // exactly one top-level hooks.PreToolUse assignment (no TOML dup key)
        let assigns = after
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with('#') && t.contains("hooks.PreToolUse")
            })
            .count();
        assert_eq!(assigns, 1);
        // helper script written and routed through the per-worktree ask url
        assert!(std::fs::read_to_string(&helper)
            .unwrap()
            .contains(".weft-codex-ask-url"));

        // idempotent: a second run does not duplicate Weft's entry
        ensure_codex_hook_in(&cfg, &helper);
        let after2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(after, after2);
        assert_eq!(
            after2
                .matches(&codex_hook_command(&helper))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn installs_managed_global_hook_without_clobbering_config() {
        let base = std::env::temp_dir().join(format!("weft-codex-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        std::fs::write(&cfg, "# mine\nmodel = \"gpt-5\"\n").unwrap();

        ensure_codex_hook_in(&cfg, &helper);
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("# mine"));
        assert!(after.contains("# BEGIN WEFT MANAGED CODEX HOOK"));
        assert!(after.contains("hooks.PreToolUse"));
        assert!(after.contains(&codex_hook_command(&helper)));
        let helper_text = std::fs::read_to_string(&helper).unwrap();
        assert!(helper_text.contains(".weft-codex-ask-url"));
        assert!(helper_text.contains("127.0.0.1")); // loopback-only guard (anti-exfil)

        ensure_codex_hook_in(&cfg, &helper);
        assert_eq!(after, std::fs::read_to_string(&cfg).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn splices_into_trailing_comma_array_without_corrupting_toml() {
        let base =
            std::env::temp_dir().join(format!("weft-codex-trailcomma-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        // Formatted multiline array WITH a trailing comma after the last element.
        let before = "hooks.PreToolUse = [\n  { matcher = \"shell\", hooks = [{ type = \"command\", command = \"/usr/local/bin/my-audit\" }] },\n]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        // Must remain valid TOML — no `, ,` double comma corrupting the array.
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "spliced config must stay valid TOML:\n{after}"
        );
        assert!(!after.replace(' ', "").contains(",,"));
        assert!(after.contains("/usr/local/bin/my-audit")); // user hook preserved
        assert!(after.contains(&codex_hook_command(&helper))); // weft hook spliced in
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn installs_top_level_hook_before_projects_table() {
        let base =
            std::env::temp_dir().join(format!("weft-codex-toplevel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        // Config whose tail is a [projects."..."] table, exactly as
        // ensure_codex_trusted appends. The managed hook must NOT land inside it.
        let before = "# mine\nmodel = \"gpt-5\"\n\n[projects.\"/repo\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "config must stay valid TOML:\n{after}"
        );
        // hooks.PreToolUse must be a TOP-LEVEL key, not captured by the table.
        let v: toml::Value = toml::from_str(&after).unwrap();
        assert!(
            v.get("hooks").and_then(|h| h.get("PreToolUse")).is_some(),
            "hooks.PreToolUse must be top-level:\n{after}"
        );
        // ...and physically precede the table header in the file.
        let hook_pos = after.find("hooks.PreToolUse").unwrap();
        let table_pos = after.find("[projects.").unwrap();
        assert!(
            hook_pos < table_pos,
            "managed hook must precede the table:\n{after}"
        );
        assert!(after.contains("[projects.\"/repo\"]")); // trust table intact
        assert!(after.contains("trust_level = \"trusted\""));
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn migrates_managed_block_into_user_array_without_duplicate_key() {
        let base = std::env::temp_dir().join(format!("weft-codex-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        // Weft previously installed its standalone managed block, and the user
        // LATER added their own hooks.PreToolUse — keeping both is a duplicate key.
        let managed = "# BEGIN WEFT MANAGED CODEX HOOK\nhooks.PreToolUse=[{ matcher = \".*\", hooks = [{ type = \"command\", command = \"bash /old/weft-codex-hook.sh\", timeout = 3650 }] }]\n# END WEFT MANAGED CODEX HOOK\n";
        let user = "hooks.PreToolUse=[{ matcher = \"shell\", hooks = [{ type = \"command\", command = \"/usr/local/bin/my-audit\" }] }]\n";
        std::fs::write(&cfg, format!("{managed}{user}")).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "must stay valid TOML (no duplicate key):\n{after}"
        );
        assert!(!after.contains("# BEGIN WEFT MANAGED CODEX HOOK")); // stale block dropped
        let assigns = after
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with('#') && t.contains("hooks.PreToolUse")
            })
            .count();
        assert_eq!(assigns, 1, "exactly one hooks.PreToolUse:\n{after}");
        assert!(after.contains("/usr/local/bin/my-audit")); // user hook preserved
        assert!(after.contains(&codex_hook_command(&helper))); // weft hook migrated into the array
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn leaves_table_form_hooks_uncorrupted() {
        let base =
            std::env::temp_dir().join(format!("weft-codex-tableform-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        // Table-form hooks: a [hooks] table with a PreToolUse key (valid Codex layout).
        let before = "model = \"gpt-5\"\n\n[hooks]\nPreToolUse = [{ matcher = \"shell\", hooks = [] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        let after = std::fs::read_to_string(&cfg).unwrap();
        // Detected as a user hook (table form) and left untouched — NOT corrupted by
        // a conflicting top-level dotted key inserted before the [hooks] table.
        assert!(
            toml::from_str::<toml::Value>(&after).is_ok(),
            "table-form config must stay valid TOML:\n{after}"
        );
        assert_eq!(after, before);
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
