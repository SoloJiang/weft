//! The worker brief (ARCHITECTURE §4.10): the structured contract a dispatched
//! worker is seeded with — objective, write-scope, the cross-repo contracts it
//! must respect (derived from the dependency graph), what it must NOT edit, and
//! how to coordinate. Assembled deterministically from the plan + repo map; the
//! lead can enrich it later.

use crate::curator;
use crate::store::{repo, Db};
use anyhow::Result;

pub struct RepoBrief {
    pub name: String,
    pub summary: String,
}

pub struct BriefData {
    pub task: String,
    pub kind: String,
    pub direction: String,
    /// Worker mandate: "plan+impl" | "impl-only" — selects the status contract.
    pub mandate: String,
    /// Repos this direction owns (its worktrees).
    pub writes: Vec<RepoBrief>,
    /// Human-readable cross-repo contract lines from the dependency graph.
    pub contracts: Vec<String>,
    /// Workspace repos this direction must not edit (read freely, don't write).
    pub non_goals: Vec<String>,
}

/// Render the brief as the worker's seed message. Markdown so it reads cleanly in
/// any of the TUIs; sections collapse when empty.
pub fn format_brief(d: &BriefData) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Brief: {}\n\n", d.direction));
    s.push_str(&format!("Task ({}): {}\n", d.kind, d.task));

    s.push_str("\n## You write\n");
    if d.writes.is_empty() {
        s.push_str("- (no write repos — coordinate with the lead)\n");
    } else {
        for w in &d.writes {
            if w.summary.is_empty() {
                s.push_str(&format!("- {}\n", w.name));
            } else {
                s.push_str(&format!("- {} — {}\n", w.name, w.summary));
            }
        }
    }

    if !d.contracts.is_empty() {
        s.push_str("\n## Contracts to respect\n");
        for c in &d.contracts {
            s.push_str(&format!("- {c}\n"));
        }
    }

    if !d.non_goals.is_empty() {
        s.push_str("\n## Read freely, but do NOT edit\n");
        s.push_str(&format!("{}\n", d.non_goals.join(", ")));
    }

    s.push_str(
        "\n## Coordinate\n\
         Other directions in this thread may run in parallel. Use the weft_bus \
         tools to post updates (bus_post / bus_broadcast), read your inbox \
         (bus_inbox), announce contract changes (announce_interface_change), and \
         ask_human when a decision is only the operator's to make. Stay within \
         your write repos; read anything you need.\n",
    );

    // The status contract (§4.6): the board is only honest if YOU move your
    // task with set_task_status as work really progresses.
    if d.mandate == "impl-only" {
        s.push_str(
            "\n## Status contract\n\
             You are dispatched impl-only: this direction is already scoped — \
             skip planning and build straight away. When the code is done and \
             your checks pass, call set_task_status(\"review\"). If the human \
             sends you back for changes, set it to \"working\" again.\n\n\
             Start now.\n",
        );
    } else {
        s.push_str(
            "\n## Status contract\n\
             Your task starts in **planning**: first work out a short \
             implementation plan for THIS direction (use your planning skill if \
             you have one), and post the essentials to the bus (bus_post) so \
             the lead can follow. When you move from planning to building, call \
             set_task_status(\"working\"). When the code is done and your \
             checks pass, call set_task_status(\"review\"). If the human sends \
             you back for changes, set it to \"working\" again.\n\n\
             Start by planning now.\n",
        );
    }
    s
}

/// Gather a direction's brief from the DB + the curator's dependency graph.
pub async fn assemble(db: &Db, direction_id: i32) -> Result<String> {
    use sea_orm::EntityTrait;
    let dir = crate::store::entities::direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction not found"))?;
    let thread = repo::get_thread(db, dir.thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;

    let write_repos: Vec<_> = repo::direction_repo_of(db, direction_id)
        .await?
        .into_iter()
        .collect();
    let write_ids: Vec<i32> = write_repos.iter().map(|r| r.id).collect();

    let graph = curator::graph(db, thread.workspace_id).await?;
    let name_of = |id: i32| {
        graph
            .nodes
            .iter()
            .find(|n| n.repo_id == id)
            .map(|n| n.repo_name.clone())
            .unwrap_or_else(|| format!("repo {id}"))
    };

    // summaries for the write repos
    let writes: Vec<RepoBrief> = write_repos
        .iter()
        .map(|r| {
            let summary = graph
                .nodes
                .iter()
                .find(|n| n.repo_id == r.id)
                .map(|n| n.summary.clone())
                .unwrap_or_default();
            RepoBrief {
                name: r.name.clone(),
                summary,
            }
        })
        .collect();

    // contracts: any edge touching a write repo
    let mut contracts = Vec::new();
    for e in &graph.edges {
        let from_w = write_ids.contains(&e.from);
        let to_w = write_ids.contains(&e.to);
        if to_w && !from_w {
            contracts.push(format!(
                "{} depends on your {} (via {}) — keep that interface stable, or announce the change on the bus.",
                name_of(e.from), name_of(e.to), e.via
            ));
        } else if from_w && !to_w {
            contracts.push(format!(
                "Your {} depends on {} (via {}) — read it for the contract; don't edit it.",
                name_of(e.from), name_of(e.to), e.via
            ));
        }
    }

    // non-goals: workspace repos this direction doesn't write
    let non_goals: Vec<String> = graph
        .nodes
        .iter()
        .filter(|n| !write_ids.contains(&n.repo_id))
        .map(|n| n.repo_name.clone())
        .collect();

    Ok(format_brief(&BriefData {
        task: thread.title,
        kind: thread.kind,
        direction: dir.name,
        mandate: repo::normalize_mandate(&dir.mandate).to_string(),
        writes,
        contracts,
        non_goals,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> BriefData {
        BriefData {
            task: "Add a discount code".into(),
            kind: "feature".into(),
            direction: "Backend endpoint".into(),
            mandate: "plan+impl".into(),
            writes: vec![
                RepoBrief { name: "api".into(), summary: "Checkout API".into() },
                RepoBrief { name: "core".into(), summary: "Shared types".into() },
            ],
            contracts: vec![
                "web-app depends on your api (via @scope/api-cli) — keep that interface stable, or announce the change on the bus.".into(),
            ],
            non_goals: vec!["web-app".into()],
        }
    }

    #[test]
    fn brief_has_objective_writes_contracts_nongoals() {
        let s = format_brief(&data());
        assert!(s.contains("# Brief: Backend endpoint"));
        assert!(s.contains("Task (feature): Add a discount code"));
        assert!(s.contains("- api — Checkout API"));
        assert!(s.contains("- core — Shared types"));
        assert!(s.contains("Contracts to respect"));
        assert!(s.contains("web-app depends on your api"));
        assert!(s.contains("do NOT edit"));
        assert!(s.contains("web-app"));
        assert!(s.contains("ask_human"));
    }

    #[test]
    fn empty_contracts_and_nongoals_collapse() {
        let mut d = data();
        d.contracts.clear();
        d.non_goals.clear();
        let s = format_brief(&d);
        assert!(!s.contains("Contracts to respect"));
        assert!(!s.contains("do NOT edit"));
        // still has the coordinate section + start cue
        assert!(s.contains("Coordinate"));
        assert!(s.contains("Start by planning now."));
    }

    #[test]
    fn plan_impl_brief_carries_planning_contract() {
        let s = format_brief(&data());
        assert!(s.contains("Status contract"));
        assert!(s.contains("starts in **planning**"));
        assert!(s.contains("set_task_status(\"working\")"));
        assert!(s.contains("set_task_status(\"review\")"));
        assert!(s.contains("Start by planning now."));
        assert!(!s.contains("impl-only"));
    }

    #[test]
    fn impl_only_brief_skips_planning() {
        let mut d = data();
        d.mandate = "impl-only".into();
        let s = format_brief(&d);
        assert!(s.contains("impl-only"));
        assert!(s.contains("skip planning"));
        assert!(s.contains("set_task_status(\"review\")"));
        assert!(s.contains("Start now."));
        assert!(!s.contains("Start by planning now."));
    }
}
