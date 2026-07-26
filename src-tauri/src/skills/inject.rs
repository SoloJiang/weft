//! Materialize enabled skills into a worker/lead cwd: copy each skill dir into
//! BOTH `.agents/skills/<name>` (Codex + OpenCode) and `.claude/skills/<name>`
//! (Claude), git-excluded so the throwaway worktree stays clean. repo-owned
//! same-name skills win (we skip rather than overwrite). Copy, not symlink —
//! Claude's symlink discovery is buggy. Best-effort: a failed skill is skipped.

use crate::skills::parse::ParsedSkill;
use std::path::Path;

const TARGET_DIRS: [&str; 2] = [".agents/skills", ".claude/skills"];

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Copy each skill into the two target dirs under `cwd`. A skill whose name
/// already exists in EITHER target (repo-owned) is skipped entirely. weft's
/// built-in skills are appended after the enabled ones.
pub fn materialize(skills: &[ParsedSkill], cwd: &Path) {
    for sk in skills {
        let exists = TARGET_DIRS
            .iter()
            .any(|d| cwd.join(d).join(&sk.name).exists());
        if exists {
            continue; // repo-owned same-name wins
        }
        let src = Path::new(&sk.dir);
        for d in TARGET_DIRS {
            let dst = cwd.join(d).join(&sk.name);
            if copy_tree(src, &dst).is_ok() {
                crate::git::git_exclude(cwd, &format!("{d}/{}", sk.name));
            }
        }
    }
    materialize_builtins(cwd);
}

/// weft's built-in skills, compiled into the binary. The `<!-- weft-builtin -->`
/// marker (placed AFTER the frontmatter — skill loaders require `---` on the
/// first line) distinguishes our copy from a user-owned same-name skill: a
/// marked (or absent) target is (re)written so upgrades ship silently with the
/// app; an unmarked existing skill is the user's and wins.
const BUILTIN_TEST_CASES: &str = include_str!("builtin_test_cases.md");
const BUILTIN_MARKER: &str = "<!-- weft-builtin -->";

pub(crate) fn materialize_builtins(cwd: &Path) {
    write_builtin(cwd, "weft-derive-test-cases", BUILTIN_TEST_CASES);
    write_builtin(cwd, "weft-preflight-merge", BUILTIN_MERGE_PREFLIGHT);
}

const BUILTIN_MERGE_PREFLIGHT: &str = include_str!("builtin_merge_preflight.md");

fn write_builtin(cwd: &Path, name: &str, content: &str) {
    if has_repo_owned_builtin(cwd, name) {
        remove_managed_builtin_counterparts(cwd, name);
        return; // an unmarked same-name skill in either target wins everywhere
    }

    for d in TARGET_DIRS {
        let dir = cwd.join(d).join(name);
        let file = dir.join("SKILL.md");
        if !builtin_write_path_is_safe(cwd, &dir, &file) {
            remove_managed_builtin_counterparts(cwd, name);
            return; // unsafe local path has the same precedence as an override
        }
        if let Ok(existing) = std::fs::read_to_string(&file) {
            if !existing.contains(BUILTIN_MARKER) {
                remove_managed_builtin_counterparts(cwd, name);
                return; // preserve whole dual-target override semantics if it races
            }
            if existing == content {
                continue; // already current
            }
        }
        if std::fs::create_dir_all(&dir).is_ok()
            && builtin_write_path_is_safe(cwd, &dir, &file)
            && std::fs::write(&file, content).is_ok()
        {
            crate::git::git_exclude(cwd, &format!("{d}/{name}"));
        }
    }
}

fn has_repo_owned_builtin(cwd: &Path, name: &str) -> bool {
    TARGET_DIRS.iter().any(|d| {
        let dir = cwd.join(d).join(name);
        let file = dir.join("SKILL.md");
        if !builtin_write_path_is_safe(cwd, &dir, &file) {
            return true;
        }
        match std::fs::read_to_string(file) {
            Ok(existing) => !existing.contains(BUILTIN_MARKER),
            Err(_) => false,
        }
    })
}

/// A repository override in either tool-specific directory must hide the
/// built-in from every tool. Remove only our marked entry, leaving the
/// repository-owned file and its directory intact.
fn remove_managed_builtin_counterparts(cwd: &Path, name: &str) {
    for d in TARGET_DIRS {
        let dir = cwd.join(d).join(name);
        let file = dir.join("SKILL.md");
        if !managed_builtin_file_is_safe(cwd, &dir, &file) {
            continue;
        }
        let Ok(existing) = std::fs::read_to_string(&file) else {
            continue;
        };
        if existing.contains(BUILTIN_MARKER) {
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Only remove the exact builtin file we created under this session cwd. A
/// symlinked skill directory or file may point at a user-managed global skill.
fn managed_builtin_file_is_safe(cwd: &Path, dir: &Path, file: &Path) -> bool {
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if dir_meta.file_type().is_symlink() {
        return false;
    }
    let Ok(file_meta) = std::fs::symlink_metadata(file) else {
        return false;
    };
    if file_meta.file_type().is_symlink() {
        return false;
    }
    let (Ok(canonical_cwd), Ok(canonical_dir), Ok(canonical_file)) =
        (cwd.canonicalize(), dir.canonicalize(), file.canonicalize())
    else {
        return false;
    };
    canonical_dir.starts_with(&canonical_cwd) && canonical_file.starts_with(&canonical_dir)
}

/// Refuse to create or update a builtin through a path that resolves outside
/// the worker cwd. Unsafe entries are treated as repository-owned overrides.
fn builtin_write_path_is_safe(cwd: &Path, dir: &Path, file: &Path) -> bool {
    let Ok(canonical_cwd) = cwd.canonicalize() else {
        return false;
    };
    let Ok(relative_dir) = dir.strip_prefix(cwd) else {
        return false;
    };

    let mut current = canonical_cwd.clone();
    for component in relative_dir.components() {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {
                let Ok(canonical_current) = current.canonicalize() else {
                    return false;
                };
                if !canonical_current.starts_with(&canonical_cwd) {
                    return false;
                }
                current = canonical_current;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }

    match std::fs::symlink_metadata(file) {
        Ok(metadata) if metadata.file_type().is_symlink() => false,
        Ok(_) => file
            .canonicalize()
            .is_ok_and(|canonical_file| canonical_file.starts_with(&current)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::parse::ParsedSkill;

    /// Built-in skill semantics: fresh cwd gets it, a stale weft-marked copy is
    /// upgraded in place, and a user-owned same-name skill is never touched.
    #[test]
    fn builtin_writes_upgrades_and_yields_to_user() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        // Fresh: written to both targets, frontmatter FIRST (skill loaders
        // require `---` on line one), marker after it.
        materialize_builtins(cwd);
        let p = cwd.join(".claude/skills/weft-derive-test-cases/SKILL.md");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("---\n"), "frontmatter must be the first block");
        assert!(body.contains(BUILTIN_MARKER));
        assert!(body.contains("weft-derive-test-cases"));
        // weft's own parser surfaces the metadata (description non-empty).
        let parsed = crate::skills::cwd_skills(cwd, &[".claude/skills"]);
        let sk = parsed.iter().find(|s| s.name == "weft-derive-test-cases").expect("parsed");
        assert!(!sk.description.is_empty(), "frontmatter description must parse");
        assert!(cwd.join(".agents/skills/weft-derive-test-cases/SKILL.md").exists());
        // Stale weft copy: upgraded to the current binary's content.
        std::fs::write(&p, format!("{BUILTIN_MARKER}\nold version")).unwrap();
        materialize_builtins(cwd);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), BUILTIN_TEST_CASES);
        // User-owned (no marker): wins, never overwritten.
        std::fs::write(&p, "my own skill").unwrap();
        materialize_builtins(cwd);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "my own skill");

        let merge = cwd.join(".claude/skills/weft-preflight-merge/SKILL.md");
        let merge_body = std::fs::read_to_string(&merge).unwrap();
        assert!(merge_body.starts_with("---\n"));
        assert!(merge_body.contains("weft-preflight-merge"));
        assert!(cwd
            .join(".agents/skills/weft-preflight-merge/SKILL.md")
            .exists());
        std::fs::write(&merge, format!("{BUILTIN_MARKER}\nold version")).unwrap();
        materialize_builtins(cwd);
        assert_eq!(
            std::fs::read_to_string(&merge).unwrap(),
            BUILTIN_MERGE_PREFLIGHT
        );
    }

    #[test]
    fn stale_merge_preflight_builtin_upgrades_in_both_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();

        materialize_builtins(cwd);
        for target in TARGET_DIRS {
            let file = cwd
                .join(target)
                .join("weft-preflight-merge/SKILL.md");
            std::fs::write(&file, format!("{BUILTIN_MARKER}\nold {target}"))
                .unwrap();
        }

        materialize_builtins(cwd);

        for target in TARGET_DIRS {
            let file = cwd
                .join(target)
                .join("weft-preflight-merge/SKILL.md");
            assert_eq!(std::fs::read_to_string(file).unwrap(), BUILTIN_MERGE_PREFLIGHT);
        }
    }

    #[test]
    fn builtin_override_in_either_target_blocks_both_targets() {
        for owner_target in TARGET_DIRS {
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path();
            materialize_builtins(cwd);
            let owned = cwd.join(owner_target).join("weft-preflight-merge/SKILL.md");
            std::fs::write(&owned, "repo-owned").unwrap();

            materialize_builtins(cwd);

            assert_eq!(std::fs::read_to_string(&owned).unwrap(), "repo-owned");
            for target in TARGET_DIRS {
                if target == owner_target {
                    continue;
                }
                assert!(
                    !cwd.join(target)
                        .join("weft-preflight-merge/SKILL.md")
                        .exists(),
                    "builtin must not materialize in {target} when {owner_target} owns the name"
                );
            }

            std::fs::remove_file(&owned).unwrap();
            materialize_builtins(cwd);
            for target in TARGET_DIRS {
                let restored = cwd.join(target).join("weft-preflight-merge/SKILL.md");
                assert_eq!(
                    std::fs::read_to_string(restored).unwrap(),
                    BUILTIN_MERGE_PREFLIGHT,
                    "builtin must return after the override is removed from {owner_target}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn builtin_override_does_not_remove_a_symlinked_counterpart() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let global_skill = outside.path().join("global-skill");
        std::fs::create_dir_all(&global_skill).unwrap();
        let global_file = global_skill.join("SKILL.md");
        std::fs::write(&global_file, BUILTIN_MERGE_PREFLIGHT).unwrap();

        let linked = cwd.join(".agents/skills/weft-preflight-merge");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        symlink(&global_skill, &linked).unwrap();
        let override_file = cwd.join(".claude/skills/weft-preflight-merge/SKILL.md");
        std::fs::create_dir_all(override_file.parent().unwrap()).unwrap();
        std::fs::write(&override_file, "repo-owned").unwrap();

        materialize_builtins(cwd);

        assert_eq!(
            std::fs::read_to_string(global_file).unwrap(),
            BUILTIN_MERGE_PREFLIGHT
        );
        assert!(linked.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn builtin_does_not_write_through_a_symlinked_skill_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let global_skill = outside.path().join("global-skill");
        std::fs::create_dir_all(&global_skill).unwrap();

        let linked = cwd.join(".agents/skills/weft-preflight-merge");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        symlink(&global_skill, &linked).unwrap();

        materialize_builtins(cwd);

        assert!(!global_skill.join("SKILL.md").exists());
        assert!(linked.is_symlink());
        assert!(!cwd
            .join(".claude/skills/weft-preflight-merge/SKILL.md")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn builtin_does_not_write_through_a_symlinked_skill_file() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let global_file = outside.path().join("SKILL.md");
        let stale = format!("{BUILTIN_MARKER}\nstale global builtin");
        std::fs::write(&global_file, &stale).unwrap();

        let linked = cwd.join(".agents/skills/weft-preflight-merge/SKILL.md");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        symlink(&global_file, &linked).unwrap();

        materialize_builtins(cwd);

        assert_eq!(std::fs::read_to_string(&global_file).unwrap(), stale);
        assert!(linked.is_symlink());
        assert!(!cwd
            .join(".claude/skills/weft-preflight-merge/SKILL.md")
            .exists());
    }

    fn mkskill(base: &std::path::Path, name: &str) -> ParsedSkill {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: {name}\n---\nx")).unwrap();
        ParsedSkill {
            name: name.into(),
            description: String::new(),
            dir: d.to_string_lossy().into(),
        }
    }

    #[test]
    fn copies_into_both_dirs_and_skips_repo_owned() {
        let base = std::env::temp_dir().join(format!("weft-skinj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let a = mkskill(&src, "deploy");
        let b = mkskill(&src, "planner");
        // repo already ships its own "planner" under .claude/skills → must be skipped
        std::fs::create_dir_all(cwd.join(".claude/skills/planner")).unwrap();
        std::fs::write(cwd.join(".claude/skills/planner/SKILL.md"), "repo-owned").unwrap();

        materialize(&[a, b], &cwd);

        // deploy copied to BOTH dirs
        assert!(cwd.join(".agents/skills/deploy/SKILL.md").exists());
        assert!(cwd.join(".claude/skills/deploy/SKILL.md").exists());
        // planner skipped (repo-owned wins) → repo copy untouched, no .agents copy
        let planner = std::fs::read_to_string(cwd.join(".claude/skills/planner/SKILL.md")).unwrap();
        assert_eq!(planner, "repo-owned");
        assert!(!cwd.join(".agents/skills/planner").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
