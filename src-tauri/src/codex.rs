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
//! worktrees through it via a local `.weft-codex-ask-url` file. User configs are
//! preserved, and no config is fabricated if Codex was never set up.

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
    if [ -n "$url" ]; then
      resp="$(curl -s -m 3600 -X POST "$url" -H 'Content-Type: application/json' --data-binary @- 2>/dev/null)"
      [ -n "$resp" ] && printf '%s' "$resp"
    fi
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

    let command = format!(
        "bash {}",
        helper
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    );
    let block = format!(
        "# BEGIN WEFT MANAGED CODEX HOOK\n\
hooks.PreToolUse=[{{ matcher = \".*\", hooks = [{{ type = \"command\", command = \"{command}\", timeout = 3650 }}] }}]\n\
# END WEFT MANAGED CODEX HOOK\n"
    );
    if has_user_pretooluse_hook(&text) {
        return;
    }
    let next = replace_managed_block(&text, &block);
    if next != text {
        write_atomic(cfg, next.as_bytes());
    }
}

fn replace_managed_block(text: &str, block: &str) -> String {
    const BEGIN: &str = "# BEGIN WEFT MANAGED CODEX HOOK";
    const END: &str = "# END WEFT MANAGED CODEX HOOK";
    let mut out = String::with_capacity(text.len() + block.len() + 2);
    let mut lines = text.lines();
    let mut replaced = false;
    while let Some(line) = lines.next() {
        if line.trim() == BEGIN {
            for inner in lines.by_ref() {
                if inner.trim() == END {
                    break;
                }
            }
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
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
    stripped.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.contains("hooks.PreToolUse")
    })
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
    fn skips_global_hook_when_user_pretooluse_exists() {
        let base =
            std::env::temp_dir().join(format!("weft-codex-user-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("config.toml");
        let helper = base.join("weft-codex-hook.sh");
        let before = "hooks.PreToolUse=[{ matcher = \"shell\", hooks = [] }]\n";
        std::fs::write(&cfg, before).unwrap();

        ensure_codex_hook_in(&cfg, &helper);

        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
        assert!(!std::fs::read_to_string(&helper).unwrap().is_empty());
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
        assert!(after.contains(&format!("bash {}", helper.to_string_lossy())));
        assert!(std::fs::read_to_string(&helper)
            .unwrap()
            .contains(".weft-codex-ask-url"));

        ensure_codex_hook_in(&cfg, &helper);
        assert_eq!(after, std::fs::read_to_string(&cfg).unwrap());
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
