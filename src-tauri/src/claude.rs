//! Claude session-storage helpers (encoded-cwd + native session-id capture) and
//! the folder-trust pre-accept.
//!
//! Product principle: we spawn PLAIN `claude` under the user's standard HOME so
//! their own config / permission mode / allowlist apply. We never inject
//! `--dangerously-skip-permissions`; per-action edit/command approvals still
//! apply — they're intercepted and surfaced via the Ask Bridge (see `ask` +
//! `inject_ask_hook`). We DO pre-accept the one-time FOLDER-TRUST onboarding for
//! the dirs weft itself created (`ensure_trusted`): a fresh worktree per dispatch
//! would otherwise stall every unattended agent on "Do you trust this folder?",
//! a startup gate that no hook can surface. That gate is not a per-action
//! permission.

use std::path::{Path, PathBuf};

/// Pre-accept claude's one-time folder-trust onboarding for a dir weft created
/// (a worktree or lead scratch dir), so a dispatched agent starts immediately
/// instead of blocking on the trust gate. This writes exactly what clicking
/// "Yes, I trust this folder" writes — `~/.claude.json` →
/// `projects.<path>.hasTrustDialogAccepted` — and nothing about per-action
/// permissions. Keyed by both the raw and canonical path (macOS /tmp ->
/// /private/tmp). Best-effort + atomic (temp + rename); only writes on the
/// first dispatch per dir, and never creates the config if claude isn't set up.
pub fn ensure_trusted(cwd: &Path) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    ensure_trusted_in(&PathBuf::from(&home).join(".claude.json"), cwd);
}

fn ensure_trusted_in(cfg: &Path, cwd: &Path) {
    // Only touch an existing config — if claude was never run, don't fabricate it.
    let Ok(text) = std::fs::read_to_string(cfg) else {
        return;
    };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };

    let mut keys = vec![cwd.to_string_lossy().to_string()];
    if let Ok(c) = std::fs::canonicalize(cwd) {
        let cs = c.to_string_lossy().to_string();
        if !keys.contains(&cs) {
            keys.push(cs);
        }
    }

    let Some(obj) = root.as_object_mut() else {
        return;
    };
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let Some(pobj) = projects.as_object_mut() else {
        return;
    };

    let mut changed = false;
    for k in keys {
        let entry = pobj.entry(k).or_insert_with(|| serde_json::json!({}));
        let Some(e) = entry.as_object_mut() else {
            continue;
        };
        if e.get("hasTrustDialogAccepted") != Some(&serde_json::Value::Bool(true)) {
            e.insert("hasTrustDialogAccepted".into(), serde_json::json!(true));
            e.entry("hasCompletedProjectOnboarding")
                .or_insert_with(|| serde_json::json!(true));
            e.entry("projectOnboardingSeenCount")
                .or_insert_with(|| serde_json::json!(1));
            changed = true;
        }
    }

    if changed {
        if let Ok(bytes) = serde_json::to_vec_pretty(&root) {
            let tmp = cfg.with_extension("json.weft-tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, cfg);
            }
        }
    }
}

/// Claude encodes the *canonical* cwd into a projects-dir name by replacing
/// both '/' and '.' with '-'. Symlinks MUST be resolved first (macOS /tmp ->
/// /private/tmp) or the encoded dir won't match. Verified empirically.
pub fn encode_cwd(canonical: &Path) -> String {
    canonical
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// `~/.claude/projects/<encoded-canonical-cwd>` for a (possibly symlinked) cwd.
pub fn projects_dir_for(cwd: &Path) -> std::io::Result<PathBuf> {
    let canon = std::fs::canonicalize(cwd)?;
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME unset"))?;
    Ok(PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encode_cwd(&canon)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_slashes_and_dots() {
        let p = Path::new("/private/tmp/weft/.claude-worktrees/x");
        assert_eq!(encode_cwd(p), "-private-tmp-weft--claude-worktrees-x");
    }

    #[test]
    fn no_special_chars_pass_through() {
        assert_eq!(encode_cwd(Path::new("/a/b/c")), "-a-b-c");
    }

    #[test]
    fn ensure_trusted_sets_flag_for_a_dir_without_clobbering() {
        let base = std::env::temp_dir().join(format!("weft-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"numStartups":3,"projects":{"/existing":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        let target = base.join("worktrees/x");
        std::fs::create_dir_all(&target).unwrap();

        ensure_trusted_in(&cfg, &target);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["projects"]["/existing"]["hasTrustDialogAccepted"], true);
        let canon = std::fs::canonicalize(&target).unwrap().to_string_lossy().to_string();
        assert_eq!(v["projects"][&canon]["hasTrustDialogAccepted"], true);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_trusted_noop_when_no_config() {
        let base = std::env::temp_dir().join(format!("weft-trust-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join(".claude.json"); // does not exist
        ensure_trusted_in(&cfg, &base);
        assert!(!cfg.exists(), "must not fabricate a claude config");
        let _ = std::fs::remove_dir_all(&base);
    }

}
