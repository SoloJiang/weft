//! Brief skeleton shared by Weft and weft-codex.
//!
//! Tool lists are adapter-filled via `extra_tools`. The Codex adapter injects
//! `task_create` / `repo_list`; the Tauri adapter keeps its richer curator brief
//! and only reuses party identity + envelope helpers from here.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

/// Which bus party a spawned thread acts as.
pub fn direction_party(direction_id: i64) -> String {
    direction_id.to_string()
}

pub const LEAD_PARTY: &str = "lead";

/// Shared mandate discriminator. Adapter copy around it may differ.
pub fn is_impl_only_mandate(mandate: &str) -> bool {
    mandate == "impl-only"
}

/// The bus usage block appended to every brief. `extra_tools` is adapter copy
/// (lead-only MCP tools, weft ask_human, …) inserted after the shared tools.
pub fn bus_block(party: &str, bus_url: &str, extra_tools: &str) -> String {
    format!(
        "\n\n## Thread bus\n\
         You are party `{party}` on this issue's thread bus. An MCP server \
         `weft-bus` is attached to this thread (endpoint {bus_url}) with \
         these tools:\n\
         - `bus_post(to, text)` — message another participant (`lead` or a \
         task id). Reports, questions, and completion notices go here.\n\
         - `bus_read()` — drain your inbox (fallback pull; messages are \
         normally injected straight into this conversation).{extra_tools}"
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
    extra_tools: &str,
) -> String {
    let spec_line = if spec.is_empty() {
        String::new()
    } else {
        format!("\nTask:\n{spec}\n")
    };
    let mandate_line = if is_impl_only_mandate(mandate) {
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
        + &bus_block(party, bus_url, extra_tools)
}

/// First message for an issue's lead thread.
pub fn lead_brief(
    issue_title: &str,
    issue_kind: &str,
    tasks: &[(i64, String)],
    repos: &[(i64, String, String)],
    bus_url: &str,
    extra_tools: &str,
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
    lines + &bus_block(LEAD_PARTY, bus_url, extra_tools)
}

/// Envelope for a bus message injected into a recipient's session.
pub fn bus_envelope(from: &str, text: &str) -> String {
    format!("[bus message from {from}]\n{text}")
}

#[cfg(test)]
mod tests {
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
            "",
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

    #[test]
    fn direction_brief_carries_identity_and_mandate() {
        let b = direction_brief(
            "Fix login",
            "backend-fix",
            "Create hello.txt containing 'hi'",
            "plan+impl",
            "sessions expire early",
            "api",
            "3",
            "http://127.0.0.1:47810/bus/1/3/mcp",
            None,
            "",
        );
        assert!(b.contains("Fix login"));
        assert!(b.contains("Create hello.txt"));
        assert!(b.contains("plan+impl"));
        assert!(b.contains("sessions expire early"));
        assert!(b.contains("party `3`"));
        assert!(b.contains("weft-bus"));
    }

    #[test]
    fn impl_only_mandate_is_shared() {
        assert!(is_impl_only_mandate("impl-only"));
        assert!(!is_impl_only_mandate("plan+impl"));
        assert!(!is_impl_only_mandate(""));
    }

    #[test]
    fn extra_tools_are_adapter_filled() {
        let b = lead_brief(
            "Fix login",
            "bugfix",
            &[(3, "backend-fix".to_string())],
            &[(1, "api".to_string(), "main".to_string())],
            "http://127.0.0.1:47810/bus/1/lead/mcp",
            "\n- `task_create` — adapter tool",
        );
        assert!(b.contains("task_create"));
        assert!(b.contains("party `lead`"));
    }
}
