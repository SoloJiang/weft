//! `<repo_state>` block injected into the lead's system prompt: tells the LLM
//! which workspace it's in, how many repos exist, and lists up to 8 of them
//! (single-lined + 80-char-clipped). Empty workspaces get a hint to render an
//! action_card; truncated lists get a hint to call <weft:list_repos/>.
//!
//! This is wire content for the model, not user-facing UI — keep it English
//! and don't run it through i18n.

use crate::store::{repo, Db};

pub const MAX_LISTED: usize = 8;
const FIELD_CAP: usize = 80;
const ELLIPSIS: char = '…';
const EMPTY_HINT: &str =
    "User has no repos. Suggest creating or importing one via <weft:action_card>...</weft:action_card> before further work.";

/// Render the `<repo_state>` block. `workspace_id == None` short-circuits to
/// the empty form (no DB access) so the lead engine can call this even before
/// a workspace is bound.
pub async fn render_repo_state(db: &Db, workspace_id: Option<i32>) -> anyhow::Result<String> {
    let repos = match workspace_id {
        Some(id) => repo::list_repos(db, id).await?,
        None => Vec::new(),
    };
    Ok(render_repo_state_from(workspace_id, &repos))
}

/// Pure renderer used when the caller already loaded the repo list (so lead
/// prompt assembly can decide situational sentinels without a second query).
pub fn render_repo_state_from(
    workspace_id: Option<i32>,
    repos: &[crate::store::entities::repo_ref::Model],
) -> String {
    let ws_id_line = match workspace_id {
        Some(id) => format!("current_workspace_id: {id}"),
        None => "current_workspace_id: null".to_string(),
    };
    let total = repos.len();

    let mut body = String::new();
    body.push_str(&ws_id_line);
    body.push('\n');
    body.push_str(&format!("repo_count: {total}"));
    body.push('\n');

    if total == 0 {
        body.push_str("repos: []\n");
        body.push_str(EMPTY_HINT);
    } else {
        body.push_str("repos:");
        for r in repos.iter().take(MAX_LISTED) {
            body.push('\n');
            body.push_str("  - ");
            body.push_str(&sanitize(&r.name));
            body.push_str(" @ ");
            body.push_str(&sanitize(&r.local_git_path));
        }
        if total > MAX_LISTED {
            body.push('\n');
            body.push_str(&format!(
                "  ... +{} more, use <weft:list_repos/> for full",
                total - MAX_LISTED
            ));
        }
    }

    format!("<repo_state>\n{body}\n</repo_state>")
}

/// Single-line + length-clip a string so it can't escape the `name @ path`
/// row: replace any control char (including newlines/tabs) with a space,
/// collapse runs of whitespace, then truncate to `FIELD_CAP` chars (counting
/// codepoints, not bytes) with an ellipsis if anything was dropped.
fn sanitize(input: &str) -> String {
    let no_ctrl: String = input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = no_ctrl.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = collapsed.chars().count();
    if total <= FIELD_CAP {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(FIELD_CAP - 1).collect();
        out.push(ELLIPSIS);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_strips_control_chars() {
        assert_eq!(sanitize("a\nb\tc"), "a b c");
        assert_eq!(sanitize("  many   spaces  "), "many spaces");
    }

    #[test]
    fn sanitize_caps_at_80_chars() {
        let long = "x".repeat(200);
        let out = sanitize(&long);
        assert_eq!(out.chars().count(), 80);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn sanitize_keeps_short_input() {
        assert_eq!(sanitize("hello"), "hello");
    }
}
