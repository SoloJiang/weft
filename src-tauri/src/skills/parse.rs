//! Parse a cloned skill source into a list of skills, via two paths that merge
//! by name (marketplace.json wins on dup): the Claude marketplace manifest, and
//! the repo's first-level `skills/<name>/SKILL.md` (or root `<name>/SKILL.md`).
//! Pure — input is an already-cloned dir tree, no network. Unit-tested.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    /// Absolute path to the skill's directory (the one holding SKILL.md).
    pub dir: String,
}

/// Read `name` + `description` from a SKILL.md's YAML frontmatter. Missing name
/// → fall back to the dir name; missing description → empty. None if no SKILL.md.
fn read_skill(dir: &Path) -> Option<(String, String)> {
    let md = dir.join("SKILL.md");
    if !md.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&md).ok()?;
    let (name, desc) = parse_frontmatter(&text);
    let name = if name.is_empty() {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        name
    };
    if name.is_empty() {
        return None;
    }
    Some((name, desc))
}

/// Extract name/description from a leading `---\n...\n---` YAML block. Minimal:
/// only these two scalar keys (the open-standard required pair). Empty if absent.
fn parse_frontmatter(text: &str) -> (String, String) {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix("---") else {
        return (String::new(), String::new());
    };
    let Some(end) = rest.find("\n---") else {
        return (String::new(), String::new());
    };
    let block = &rest[..end];
    let mut name = String::new();
    let mut desc = String::new();
    for line in block.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = v.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }
    (name, desc)
}

fn push_skill(dir: &Path, out: &mut Vec<ParsedSkill>) {
    if out.iter().any(|s| s.dir == dir.to_string_lossy()) {
        return;
    }
    if let Some((name, description)) = read_skill(dir) {
        if !out.iter().any(|s| s.name == name) {
            out.push(ParsedSkill {
                name,
                description,
                dir: dir.to_string_lossy().into_owned(),
            });
        }
    }
}

/// Path A: plugins listed in .claude-plugin/marketplace.json. Each `source` is a
/// repo-relative dir; treat it as a skill dir (has SKILL.md) or a container of
/// `skills/<name>/SKILL.md`.
fn from_marketplace(root: &Path, out: &mut Vec<ParsedSkill>) {
    let mf = root.join(".claude-plugin/marketplace.json");
    let Ok(text) = std::fs::read_to_string(&mf) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(plugins) = v.get("plugins").and_then(|p| p.as_array()) else {
        return;
    };
    for p in plugins {
        let Some(src) = p.get("source").and_then(|s| s.as_str()) else {
            continue;
        };
        let base: PathBuf = root.join(src.trim_start_matches("./"));
        push_skill(&base, out); // base itself may be a skill dir
        scan_skills_dir(&base.join("skills"), out); // or contain skills/<name>
    }
}

/// Path B: first-level `skills/<name>/SKILL.md`, plus root-level `<name>/SKILL.md`.
fn from_first_level(root: &Path, out: &mut Vec<ParsedSkill>) {
    scan_skills_dir(&root.join("skills"), out);
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                push_skill(&p, out);
            }
        }
    }
}

fn scan_skills_dir(dir: &Path, out: &mut Vec<ParsedSkill>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            push_skill(&p, out);
        }
    }
}

/// Parse all skills in a cloned source. marketplace.json first (priority on dup),
/// then first-level dirs.
pub fn parse_source(root: &Path) -> Vec<ParsedSkill> {
    let mut out = Vec::new();
    from_marketplace(root, &mut out);
    from_first_level(root, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("weft-skparse-{}-{}", std::process::id(), id));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn write_skill(dir: &std::path::Path, name: &str, desc: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn first_level_skills_dir_is_parsed() {
        let root = tmp();
        write_skill(&root.join("skills/deploy"), "deploy", "ship it");
        write_skill(&root.join("planner"), "planner", "plan it"); // root-level <name>/SKILL.md
        let mut got = parse_source(&root);
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["deploy", "planner"]
        );
        assert_eq!(got[0].description, "ship it");
    }

    #[test]
    fn marketplace_json_path_takes_priority_on_dup() {
        let root = tmp();
        // a marketplace plugin pointing at ./mp/deploy with a SKILL.md
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/marketplace.json"),
            r#"{"plugins":[{"source":"./mp/deploy"}]}"#,
        )
        .unwrap();
        write_skill(&root.join("mp/deploy"), "deploy", "from marketplace");
        // also a first-level skills/deploy with a different description
        write_skill(&root.join("skills/deploy"), "deploy", "from first-level");
        let got = parse_source(&root);
        let deploy: Vec<_> = got.iter().filter(|s| s.name == "deploy").collect();
        assert_eq!(deploy.len(), 1, "dup name deduped");
        assert_eq!(
            deploy[0].description, "from marketplace",
            "marketplace path wins"
        );
    }

    #[test]
    fn name_falls_back_to_dir_when_frontmatter_missing() {
        let root = tmp();
        std::fs::create_dir_all(root.join("skills/orphan")).unwrap();
        std::fs::write(root.join("skills/orphan/SKILL.md"), "no frontmatter body").unwrap();
        let got = parse_source(&root);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "orphan");
        assert_eq!(got[0].description, "");
    }
}
