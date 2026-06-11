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
/// already exists in EITHER target (repo-owned) is skipped entirely.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::parse::ParsedSkill;

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
