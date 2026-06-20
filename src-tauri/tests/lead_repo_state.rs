//! lead_chat::repo_state::render_repo_state — emits a <repo_state> hint for
//! the lead's system prompt: count, current workspace id, up to 8 repos
//! (name @ path, single-lined + 80-char-clipped), with truncation tail and
//! a "no repos" action_card hint for empty workspaces.
use weft::lead_chat::repo_state::render_repo_state;
use weft::store::{repo, Db};

#[tokio::test]
async fn renders_empty_with_hint() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let out = render_repo_state(&db, None).await.unwrap();
    assert!(
        out.contains("repo_count: 0"),
        "missing repo_count: 0 in {out}"
    );
    assert!(
        out.contains("current_workspace_id: null"),
        "missing current_workspace_id: null in {out}"
    );
    assert!(
        out.contains("User has no repos"),
        "missing hint sentence in {out}"
    );
    assert!(
        out.contains("<weft:action_card>"),
        "missing action_card mention in {out}"
    );
    assert!(
        out.starts_with("<repo_state>"),
        "missing opening tag in {out}"
    );
    assert!(
        out.trim_end().ends_with("</repo_state>"),
        "missing closing tag in {out}"
    );
}

#[tokio::test]
async fn renders_three_repos_no_hint() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    repo::add_repo_ref(&db, ws.id, "alpha", "/tmp/alpha", "main", "", true)
        .await
        .unwrap();
    repo::add_repo_ref(&db, ws.id, "beta", "/tmp/beta", "main", "", true)
        .await
        .unwrap();
    repo::add_repo_ref(&db, ws.id, "gamma", "/tmp/gamma", "main", "", true)
        .await
        .unwrap();

    let out = render_repo_state(&db, Some(ws.id)).await.unwrap();
    assert!(
        out.contains("repo_count: 3"),
        "missing repo_count: 3 in {out}"
    );
    assert!(out.contains(&format!("current_workspace_id: {}", ws.id)));
    assert!(
        out.contains("  - alpha @ /tmp/alpha"),
        "missing alpha row in {out}"
    );
    assert!(
        out.contains("  - beta @ /tmp/beta"),
        "missing beta row in {out}"
    );
    assert!(
        out.contains("  - gamma @ /tmp/gamma"),
        "missing gamma row in {out}"
    );
    assert!(
        !out.contains("User has no repos"),
        "unexpected empty hint in {out}"
    );
    assert!(
        !out.contains("more, use <weft:list_repos/>"),
        "unexpected truncation in {out}"
    );
}

#[tokio::test]
async fn truncates_over_eight() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    for i in 0..15 {
        repo::add_repo_ref(
            &db,
            ws.id,
            &format!("repo{i:02}"),
            &format!("/tmp/repo{i:02}"),
            "main",
            "",
            true,
        )
        .await
        .unwrap();
    }
    let out = render_repo_state(&db, Some(ws.id)).await.unwrap();
    assert!(
        out.contains("repo_count: 15"),
        "missing repo_count: 15 in {out}"
    );
    assert!(
        out.contains("... +7 more"),
        "missing truncation tail in {out}"
    );
    assert!(
        out.contains("<weft:list_repos/>"),
        "missing list_repos sentinel in {out}"
    );
    let row_count = out.lines().filter(|l| l.starts_with("  - ")).count();
    assert_eq!(
        row_count, 8,
        "expected 8 listed rows, got {row_count}\n{out}"
    );
}

#[tokio::test]
async fn sanitizes_special_chars() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let nasty_name = "weird\nname\twith\rctrl";
    let long_path = format!("/tmp/{}", "x".repeat(250));
    repo::add_repo_ref(&db, ws.id, nasty_name, &long_path, "main", "", true)
        .await
        .unwrap();

    let out = render_repo_state(&db, Some(ws.id)).await.unwrap();
    // No raw newline injection in the rendered repo row.
    let row = out
        .lines()
        .find(|l| l.starts_with("  - "))
        .expect("expected a repo row");
    assert!(!row.contains('\n') && !row.contains('\t') && !row.contains('\r'));
    assert!(!row.contains("weird\nname"));
    // 2 (indent) + 2 ("- ") + 80 (name cap) + 3 (" @ ") + 80 (path cap) = 167 max.
    assert!(
        row.chars().count() <= 167,
        "row too long: {} chars",
        row.chars().count()
    );
    // Original 250-char path must have been clipped (no 100 consecutive 'x').
    assert!(
        !row.contains(&"x".repeat(100)),
        "path was not clipped: {row}"
    );
}

#[tokio::test]
async fn isolates_workspaces() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws1 = repo::create_workspace(&db, "one").await.unwrap();
    let ws2 = repo::create_workspace(&db, "two").await.unwrap();
    repo::add_repo_ref(&db, ws1.id, "only-one", "/tmp/one", "main", "", true)
        .await
        .unwrap();
    repo::add_repo_ref(&db, ws2.id, "only-two", "/tmp/two", "main", "", true)
        .await
        .unwrap();

    let out = render_repo_state(&db, Some(ws1.id)).await.unwrap();
    assert!(out.contains("repo_count: 1"));
    assert!(out.contains("only-one"));
    assert!(
        !out.contains("only-two"),
        "ws2's repo leaked into ws1 render: {out}"
    );
}
