//! weft-managed skills: git-sourced, synced, injected into worker/lead cwds.
pub mod inject;
pub mod parse;
pub mod sync;

use crate::skills::parse::{parse_source, ParsedSkill};
use crate::store::{repo, Db};
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnabledSkill {
    pub source_id: i32,
    pub name: String,
    pub description: String,
    pub dir: String,
    /// True when a same-named skill from a lower source id already won.
    pub overridden: bool,
    /// True when enabled at "global" scope (all workspaces) rather than only
    /// this workspace — drives the weft-global vs weft-workspace layer label.
    pub global: bool,
}

/// Pure enable-resolution: given each source's parsed skills (tagged by an
/// opaque source key), the enable rows, the target ws id, and a key→id mapper,
/// return enabled skills with cross-source same-name dedupe (lower source id
/// wins; the rest are `overridden`). Generic over the key so it's testable
/// without a DB.
pub fn resolve_enabled<K: AsRef<str>>(
    parsed: &[(K, ParsedSkill)],
    enables: &[(i32, String, String)], // (source_id, skill_name, scope)
    ws_id: i32,
    key_to_id: impl Fn(&str) -> i32,
) -> Vec<EnabledSkill> {
    let ws_scope = format!("ws:{ws_id}");
    let is_enabled = |sid: i32, name: &str| {
        enables.iter().any(|(s, n, scope)| {
            *s == sid && n == name && (scope == "global" || scope == &ws_scope)
        })
    };
    // collect enabled, tagged with source id, sorted by (name, source id)
    let mut rows: Vec<(i32, &ParsedSkill)> = parsed
        .iter()
        .map(|(k, p)| (key_to_id(k.as_ref()), p))
        .filter(|(sid, p)| is_enabled(*sid, &p.name))
        .collect();
    rows.sort_by(|a, b| a.1.name.cmp(&b.1.name).then(a.0.cmp(&b.0)));
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    rows.into_iter()
        .map(|(sid, p)| {
            let overridden = !seen.insert(p.name.as_str());
            // global when a "global"-scope enable row exists for this (source, skill)
            let global = enables
                .iter()
                .any(|(s, n, scope)| *s == sid && n == &p.name && scope == "global");
            EnabledSkill {
                source_id: sid,
                name: p.name.clone(),
                description: p.description.clone(),
                dir: p.dir.clone(),
                overridden,
                global,
            }
        })
        .collect()
}

/// DB-backed: parse every source's cache, resolve enables for `ws_id`.
pub async fn enabled_for_workspace(db: &Db, ws_id: i32) -> Result<Vec<EnabledSkill>> {
    let sources = repo::list_skill_sources(db).await?;
    let home = crate::paths::skills_home()?;
    let mut parsed: Vec<(i32, ParsedSkill)> = Vec::new();
    for s in &sources {
        let cache = home.join(s.id.to_string());
        for p in parse_source(&cache) {
            parsed.push((s.id, p));
        }
    }
    let enables: Vec<(i32, String, String)> = repo::list_skill_enable(db)
        .await?
        .into_iter()
        .map(|e| (e.source_id, e.skill_name, e.scope))
        .collect();
    // key is already the i32 id rendered as string; map back trivially.
    let tagged: Vec<(String, ParsedSkill)> = parsed
        .into_iter()
        .map(|(id, p)| (id.to_string(), p))
        .collect();
    Ok(resolve_enabled(&tagged, &enables, ws_id, |k| {
        k.parse().unwrap_or(-1)
    }))
}

/// Materialize the ws's enabled (non-overridden) skills into `cwd`. Best-effort.
pub async fn inject_for(db: &Db, ws_id: i32, cwd: &Path) {
    let Ok(enabled) = enabled_for_workspace(db, ws_id).await else {
        return;
    };
    let active: Vec<ParsedSkill> = enabled
        .into_iter()
        .filter(|e| !e.overridden)
        .map(|e| ParsedSkill {
            name: e.name,
            description: e.description,
            dir: e.dir,
        })
        .collect();
    inject::materialize(&active, cwd);
}

/// Sync one source by id, updating its status. Best-effort status recording.
pub async fn sync_source(db: &Db, id: i32) -> Result<()> {
    let Some(s) = repo::get_skill_source(db, id).await? else {
        return Ok(());
    };
    let cache = crate::paths::skills_home()?.join(id.to_string());
    match sync::sync_to(&s.git_url, &s.git_ref, &cache) {
        Ok(()) => {
            let now = repo::now_unix();
            repo::set_skill_source_status(db, id, "ok", Some(&now)).await?;
        }
        Err(e) => {
            repo::set_skill_source_status(db, id, &format!("error:{e}"), None).await?;
        }
    }
    Ok(())
}

/// Sweep all sources at startup, then every interval (WEFT_SKILLS_SYNC_SECS,
/// default 6h; floored at 60s). Best-effort, never blocks.
pub fn spawn_periodic(app: tauri::AppHandle) {
    use tauri::Manager;
    std::thread::spawn(move || {
        let interval = std::env::var("WEFT_SKILLS_SYNC_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(6 * 3600)
            .max(60);
        loop {
            if let Some(db) = app.try_state::<Db>() {
                let db = Db(db.0.clone());
                tauri::async_runtime::spawn(async move {
                    if let Ok(sources) = repo::list_skill_sources(&db).await {
                        for s in sources {
                            let _ = sync_source(&db, s.id).await;
                        }
                    }
                });
            }
            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enabled_resolves_global_and_ws_and_dedupes_by_name() {
        let parsed = vec![
            (
                "s1".to_string(),
                crate::skills::parse::ParsedSkill {
                    name: "deploy".into(),
                    description: "".into(),
                    dir: "/a/deploy".into(),
                },
            ),
            (
                "s2".to_string(),
                crate::skills::parse::ParsedSkill {
                    name: "deploy".into(),
                    description: "".into(),
                    dir: "/b/deploy".into(),
                },
            ),
            (
                "s1".to_string(),
                crate::skills::parse::ParsedSkill {
                    name: "lint".into(),
                    description: "".into(),
                    dir: "/a/lint".into(),
                },
            ),
        ];
        // s1.deploy enabled globally; s2.deploy enabled in ws:1; s1.lint not enabled
        let enables = vec![
            (1i32, "deploy".to_string(), "global".to_string()), // source_id=1
            (2i32, "deploy".to_string(), "ws:1".to_string()),   // source_id=2
        ];
        let by_id = |sid: &str| if sid == "s1" { 1 } else { 2 };
        let resolved = resolve_enabled(&parsed, &enables, 1, by_id);
        // deploy enabled by both s1(global) and s2(ws) but deduped by name;
        // s1 (lower id) wins, s2.deploy overridden
        let active: Vec<_> = resolved.iter().filter(|e| !e.overridden).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "deploy");
        assert_eq!(active[0].dir, "/a/deploy");
        assert!(resolved
            .iter()
            .any(|e| e.name == "deploy" && e.dir == "/b/deploy" && e.overridden));
        // lint not enabled → absent
        assert!(!resolved.iter().any(|e| e.name == "lint"));
    }
}
