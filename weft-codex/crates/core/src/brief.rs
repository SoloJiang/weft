//! Codex brief adapter. Tool lists stay here; the skeleton is `weft-brief`.

pub use weft_brief::{bus_envelope, direction_party, ArtifactBasis, LEAD_PARTY};

const LEAD_TOOLS: &str = "\n- `task_create(name, repo_id, spec, reason?, mandate?, base_branch?)` — create and automatically dispatch a worker task after decomposing the issue.\n- `repo_list()` — refresh the workspace repository list after the human adds repositories.";

/// First message for a direction (worker) thread.
#[allow(clippy::too_many_arguments)]
pub fn direction_brief(
    issue_title: &str,
    direction_name: &str,
    spec: &str,
    mandate: &str,
    reason: &str,
    repo_name: &str,
    party: &str,
    bus_url: &str,
    basis: Option<&ArtifactBasis<'_>>,
) -> String {
    weft_brief::direction_brief(
        issue_title,
        direction_name,
        spec,
        mandate,
        reason,
        repo_name,
        party,
        bus_url,
        basis,
        "",
    )
}

/// First message for an issue's lead thread.
pub fn lead_brief(
    issue_title: &str,
    issue_kind: &str,
    tasks: &[(i64, String)],
    repos: &[(i64, String, String)],
    bus_url: &str,
) -> String {
    weft_brief::lead_brief(issue_title, issue_kind, tasks, repos, bus_url, LEAD_TOOLS)
}

#[cfg(test)]
mod basis_tests {
    use super::*;

    fn brief_with(basis: Option<&ArtifactBasis<'_>>) -> String {
        direction_brief(
            "Fix login",
            "backend-fix",
            "do the thing",
            "plan+impl",
            "why",
            "api",
            "3",
            "http://127.0.0.1:47810/bus/1/3/mcp",
            basis,
        )
    }

    #[test]
    fn the_brief_references_the_planning_document_without_copying_it() {
        let basis = ArtifactBasis {
            id: 7,
            revision: 3,
            title: "Checkout test cases",
        };
        let brief = brief_with(Some(&basis));

        assert!(brief.contains("Checkout test cases"));
        assert!(brief.contains("artifact 7"));
        assert!(brief.contains("revision 3"));
        assert!(brief.contains("artifact_read(id: 7)"));
        assert!(brief.contains("higher than 3"));
    }

    #[test]
    fn a_task_planned_from_nothing_says_nothing() {
        let brief = brief_with(None);
        assert!(!brief.contains("Planned from"));
        assert!(!brief.contains("artifact_read"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn direction_brief_carries_identity_and_mandate() {
        let b = super::direction_brief(
            "Fix login",
            "backend-fix",
            "Create hello.txt containing 'hi'",
            "plan+impl",
            "sessions expire early",
            "api",
            "3",
            "http://127.0.0.1:47810/bus/1/3/mcp",
            None,
        );
        assert!(b.contains("Fix login"));
        assert!(b.contains("Create hello.txt"));
        assert!(b.contains("plan+impl"));
        assert!(b.contains("sessions expire early"));
        assert!(b.contains("party `3`"));
        assert!(b.contains("weft-bus"));
    }

    #[test]
    fn lead_brief_lists_tasks_repos_and_creation_tool() {
        let b = super::lead_brief(
            "Fix login",
            "bugfix",
            &[
                (3, "backend-fix".to_string()),
                (4, "frontend-copy".to_string()),
            ],
            &[(1, "api".to_string(), "main".to_string())],
            "http://127.0.0.1:47810/bus/1/lead/mcp",
        );
        assert!(b.contains("`3`: backend-fix"));
        assert!(b.contains("Issue type: bugfix"));
        assert!(b.contains("`4`: frontend-copy"));
        assert!(b.contains("`1`: api (base: main)"));
        assert!(b.contains("task_create"));
        assert!(b.contains("dispatch automatically"));
        assert!(b.contains("repo_list"));
        assert!(b.contains("party `lead`"));
    }

    #[test]
    fn lead_brief_without_repos_explains_the_refresh_flow() {
        let b = super::lead_brief(
            "Explore caching",
            "spike",
            &[],
            &[],
            "http://127.0.0.1:47810/bus/1/lead/mcp",
        );
        assert!(b.contains("Ask the human to add a repository"));
        assert!(b.contains("then call `repo_list`"));
        assert!(b.contains("None yet"));
    }
}
