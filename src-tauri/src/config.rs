//! Effective-config resolution (§ MVP "配置物化" / M6 有效配置预览): which skills
//! and rules actually apply to a repo, and which LAYER each comes from. weft
//! drives the native `claude` CLI, so the layers are the conventional Claude
//! ones: personal `~/.claude/` (lowest precedence) is overridden by the repo's
//! own `<repo>/.claude/` + `<repo>/CLAUDE.md` (highest). A team layer (via
//! marketplace) will slot between them later — the precedence ladder already
//! leaves room. The merge logic is pure + unit-tested; FS walking is thin.

use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ConfigItem {
    pub name: String,
    /// "skill" | "rule"
    pub kind: String,
    /// "personal" | "repo" (room for "team" later)
    pub layer: String,
    pub path: String,
    /// A skill shadowed by a same-named one in a higher-precedence layer.
    pub overridden: bool,
}

/// Layer precedence — higher wins. Unknown layers sort lowest.
fn precedence(layer: &str) -> u8 {
    match layer {
        "repo" => 4,
        "weft-workspace" => 3,
        "weft-global" => 2,
        "team" => 1,
        _ => 0, // personal
    }
}

/// Mark each skill shadowed by a same-named skill in a higher-precedence layer
/// as `overridden`, and order the list (skills then rules; by name; effective
/// before overridden). Rules do NOT override — every layer's CLAUDE.md applies,
/// so they're never marked. Pure: no FS, fully unit-testable.
pub fn resolve_effective(mut items: Vec<ConfigItem>) -> Vec<ConfigItem> {
    use std::collections::HashMap;
    let mut best: HashMap<&str, u8> = HashMap::new();
    for it in &items {
        if it.kind != "skill" {
            continue;
        }
        let p = precedence(&it.layer);
        best.entry(it.name.as_str())
            .and_modify(|m| {
                if p > *m {
                    *m = p;
                }
            })
            .or_insert(p);
    }
    // collect winners first to avoid borrow overlap with the mutation below
    let winners: HashMap<String, u8> = best.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    for it in &mut items {
        if it.kind == "skill" {
            it.overridden = precedence(&it.layer) < winners[it.name.as_str()];
        }
    }
    items.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.overridden.cmp(&b.overridden))
            .then(a.name.cmp(&b.name))
    });
    items
}

fn list_skills(base: &Path, layer: &str, out: &mut Vec<ConfigItem>) {
    let dir = base.join("skills");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                out.push(ConfigItem {
                    name: name.to_string(),
                    kind: "skill".into(),
                    layer: layer.into(),
                    path: p.to_string_lossy().into_owned(),
                    overridden: false,
                });
            }
        }
    }
}

fn push_rule_if(path: std::path::PathBuf, layer: &str, out: &mut Vec<ConfigItem>) {
    if path.is_file() {
        out.push(ConfigItem {
            name: "CLAUDE.md".into(),
            kind: "rule".into(),
            layer: layer.into(),
            path: path.to_string_lossy().into_owned(),
            overridden: false,
        });
    }
}

/// Enumerate the effective skills + rules for `repo_path`, given the user's
/// `home` (so it's testable). Personal `~/.claude/`, then the repo's own
/// `.claude/` + root CLAUDE.md, resolved by precedence.
pub fn effective_for(repo_path: &Path, home: &Path) -> Vec<ConfigItem> {
    let mut out = Vec::new();
    let personal = home.join(".claude");
    list_skills(&personal, "personal", &mut out);
    push_rule_if(personal.join("CLAUDE.md"), "personal", &mut out);

    let repo_claude = repo_path.join(".claude");
    list_skills(&repo_claude, "repo", &mut out);
    push_rule_if(repo_path.join("CLAUDE.md"), "repo", &mut out);
    push_rule_if(repo_claude.join("CLAUDE.md"), "repo", &mut out);

    resolve_effective(out)
}

/// Like `effective_for`, but injects weft-managed skills as `weft-global` /
/// `weft-workspace` layers between personal and repo. `weft` is (name, layer,
/// dir) — layer is "weft-global" or "weft-workspace". Pure over its inputs.
pub fn effective_for_with_weft(
    repo_path: &Path,
    home: &Path,
    weft: &[(String, String, String)],
) -> Vec<ConfigItem> {
    let mut out = Vec::new();
    let personal = home.join(".claude");
    list_skills(&personal, "personal", &mut out);
    push_rule_if(personal.join("CLAUDE.md"), "personal", &mut out);

    for (name, layer, dir) in weft {
        out.push(ConfigItem {
            name: name.clone(),
            kind: "skill".into(),
            layer: layer.clone(),
            path: dir.clone(),
            overridden: false,
        });
    }

    let repo_claude = repo_path.join(".claude");
    list_skills(&repo_claude, "repo", &mut out);
    push_rule_if(repo_path.join("CLAUDE.md"), "repo", &mut out);
    push_rule_if(repo_claude.join("CLAUDE.md"), "repo", &mut out);

    resolve_effective(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn item(name: &str, kind: &str, layer: &str) -> ConfigItem {
        ConfigItem {
            name: name.into(),
            kind: kind.into(),
            layer: layer.into(),
            path: format!("/{layer}/{name}"),
            overridden: false,
        }
    }

    #[test]
    fn repo_skill_shadows_same_named_personal() {
        let out = resolve_effective(vec![
            item("planner", "skill", "personal"),
            item("planner", "skill", "repo"),
            item("lint", "skill", "personal"),
        ]);
        let planner: Vec<_> = out.iter().filter(|i| i.name == "planner").collect();
        let personal = planner.iter().find(|i| i.layer == "personal").unwrap();
        let repo = planner.iter().find(|i| i.layer == "repo").unwrap();
        assert!(personal.overridden, "personal planner is shadowed by repo");
        assert!(!repo.overridden, "repo planner wins");
        // a non-conflicting personal skill stays effective
        assert!(!out.iter().find(|i| i.name == "lint").unwrap().overridden);
    }

    #[test]
    fn rules_never_override_each_other() {
        let out = resolve_effective(vec![
            item("CLAUDE.md", "rule", "personal"),
            item("CLAUDE.md", "rule", "repo"),
        ]);
        assert!(out.iter().all(|i| !i.overridden), "both rule layers apply");
    }

    fn tmp() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("weft-config-{}-{}", std::process::id(), id));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn weft_layers_sit_between_personal_and_repo() {
        let out = resolve_effective(vec![
            item("deploy", "skill", "personal"),
            item("deploy", "skill", "weft-global"),
            item("deploy", "skill", "weft-workspace"),
            item("deploy", "skill", "repo"),
        ]);
        // highest precedence (repo) wins; the rest overridden
        let effective: Vec<_> = out.iter().filter(|i| !i.overridden).collect();
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].layer, "repo");
        // and weft-workspace beats weft-global beats personal in the shadow order
        assert!(out.iter().any(|i| i.layer == "weft-workspace" && i.overridden));
        assert!(out.iter().any(|i| i.layer == "weft-global" && i.overridden));
    }

    #[test]
    fn effective_for_reads_both_layers_from_disk() {
        let home = tmp();
        let repo = tmp();
        std::fs::create_dir_all(home.join(".claude/skills/planner")).unwrap();
        std::fs::create_dir_all(repo.join(".claude/skills/planner")).unwrap();
        std::fs::create_dir_all(repo.join(".claude/skills/deploy")).unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "rules").unwrap();

        let out = effective_for(&repo, &home);
        // planner exists in both → repo effective, personal overridden
        let planner: Vec<_> = out.iter().filter(|i| i.name == "planner").collect();
        assert_eq!(planner.len(), 2);
        assert!(planner.iter().any(|i| i.layer == "repo" && !i.overridden));
        assert!(planner.iter().any(|i| i.layer == "personal" && i.overridden));
        // repo-only skill + repo rule present
        assert!(out.iter().any(|i| i.name == "deploy" && i.kind == "skill"));
        assert!(out.iter().any(|i| i.kind == "rule" && i.layer == "repo"));
    }
}
