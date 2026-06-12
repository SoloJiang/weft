//! ensure_default_workspace: returns the most-recently created workspace
//! (highest id), but never creates a synthetic default workspace.
use weft::commands::ensure_default_workspace_inner;
use weft::store::{repo, Db};

#[tokio::test]
async fn errors_when_none() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    assert!(repo::list_workspaces(&db).await.unwrap().is_empty());

    let err = ensure_default_workspace_inner(&db).await.unwrap_err();
    assert!(err.contains("workspace required"));
    assert!(repo::list_workspaces(&db).await.unwrap().is_empty());
}

#[tokio::test]
async fn returns_latest_when_exists() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let a = repo::create_workspace(&db, "A").await.unwrap();
    let b = repo::create_workspace(&db, "B").await.unwrap();
    assert!(b.id > a.id);

    let id = ensure_default_workspace_inner(&db).await.unwrap();
    assert_eq!(id, b.id);

    // No new workspace was inserted.
    assert_eq!(repo::list_workspaces(&db).await.unwrap().len(), 2);
}
