//! Brief rendering for lead and direction spawns. The brief is the first
//! user-role message of the Codex thread — it must carry everything the agent
//! cannot discover on its own: mandate, scope, and bus identity.

/// Which bus party a spawned thread acts as.
pub fn direction_party(direction_id: i64) -> String {
    direction_id.to_string()
}

pub const LEAD_PARTY: &str = "lead";

/// The bus usage block appended to every brief. `bus_url` is this thread's
/// per-thread MCP endpoint (registered as the `weft-bus` MCP server).
fn bus_block(party: &str, bus_url: &str) -> String {
    let lead_tool = if party == LEAD_PARTY {
        "\n- `task_create(name, repo_id, spec, reason?, mandate?, base_branch?)` — create and automatically dispatch a worker task after decomposing the issue.\n- `repo_list()` — refresh the workspace repository list after the human adds repositories."
    } else {
        ""
    };
    format!(
        "\n\n## Thread bus\n\
         You are party `{party}` on this issue's thread bus. An MCP server \
         `weft-bus` is attached to this thread (endpoint {bus_url}) with \
         these tools:\n\
         - `bus_post(to, text)` — message another participant (`lead` or a \
         task id). Reports, questions, and completion notices go here.\n\
         - `bus_read()` — drain your inbox (fallback pull; messages are \
         normally injected straight into this conversation).{lead_tool}"
    )
}

/// What planning document a task was derived from, if any.
///
/// A reference, never a copy: the artifact belongs to the issue and keeps
/// moving, so embedding its text would hand the worker a snapshot that silently
/// goes out of date. The worker reads the live document through MCP instead.
pub struct ArtifactBasis<'a> {
    pub id: i64,
    pub revision: i64,
    pub title: &'a str,
}

fn artifact_block(basis: Option<&ArtifactBasis<'_>>) -> String {
    let Some(basis) = basis else {
        return String::new();
    };
    let title = if basis.title.is_empty() {
        "test cases"
    } else {
        basis.title
    };
    format!(
        "\n\nPlanned from: {title} (artifact {id}, revision {revision}).\n\
         Read it with `artifact_read(id: {id})` before you start. If the \
         revision you read is higher than {revision}, the plan predates the \
         current document — reconcile the difference with the lead over the \
         bus rather than guessing which one is right.",
        id = basis.id,
        revision = basis.revision
    )
}

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
    let spec_line = if spec.is_empty() {
        String::new()
    } else {
        format!("\nTask:\n{spec}\n")
    };
    let mandate_line = if mandate == "impl-only" {
        "Mandate: impl-only — the scope is fully specified; build straight away."
    } else {
        "Mandate: plan+impl — plan your own direction first, then build it."
    };
    let reason_line = if reason.is_empty() {
        String::new()
    } else {
        format!("\nWhy this repo must change: {reason}")
    };
    format!(
        "You are the worker for task `{direction_name}` on issue: {issue_title}\n\
         Repo: {repo_name} (you write ONLY this repo; your working directory \
         is its dedicated worktree).\n\
         {spec_line}\
         {mandate_line}{reason_line}\n\
         When done, post a completion summary to `lead` via the bus."
    ) + &artifact_block(basis)
        + &bus_block(party, bus_url)
}

/// First message for an issue's lead thread.
pub fn lead_brief(
    issue_title: &str,
    issue_kind: &str,
    tasks: &[(i64, String)],
    repos: &[(i64, String, String)],
    bus_url: &str,
) -> String {
    let mut lines = format!(
        "You are the lead on issue: {issue_title}\n\
         Issue type: {issue_kind}\n\
         You own decomposition and coordination; you do not write code yourself.\n\
         Available repositories:\n"
    );
    if repos.is_empty() {
        lines.push_str(
            "- None. Ask the human to add a repository, then call `repo_list` before creating tasks.\n",
        );
    }
    for (id, name, base_ref) in repos {
        lines.push_str(&format!("- `{id}`: {name} (base: {base_ref})\n"));
    }
    lines.push_str("Existing tasks:\n");
    if tasks.is_empty() {
        lines.push_str(
            "- None yet. Decompose the issue and use `task_create` for each worker task.\n",
        );
    }
    for (id, name) in tasks {
        lines.push_str(&format!("- `{id}`: {name}\n"));
    }
    lines.push_str(
        "Create tasks through `task_create`; workers dispatch automatically \
         without a human approval step. Keep each task scoped to one repository \
         with a complete implementation brief. Track worker bus reports, answer \
         questions, and synthesize the final outcome for the human.",
    );
    lines + &bus_block(LEAD_PARTY, bus_url)
}

/// Envelope for a bus message injected into a recipient's Codex thread.
pub fn bus_envelope(from: &str, text: &str) -> String {
    format!("[bus message from {from}]\n{text}")
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
        // A reference, not a copy: the worker must read the live document, or
        // it would act on a snapshot that keeps drifting.
        assert!(brief.contains("artifact_read(id: 7)"));
        // And it must be told what to do when the document has moved on.
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
