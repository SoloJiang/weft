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
}

fn write_builtin(cwd: &Path, name: &str, content: &str) {
    for d in TARGET_DIRS {
        let dir = cwd.join(d).join(name);
        let file = dir.join("SKILL.md");
        if let Ok(existing) = std::fs::read_to_string(&file) {
            if !existing.contains(BUILTIN_MARKER) {
                continue; // user-owned same-name skill wins
            }
            if existing == content {
                continue; // already current
            }
        }
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&file, content).is_ok() {
            crate::git::git_exclude(cwd, &format!("{d}/{name}"));
        }
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
