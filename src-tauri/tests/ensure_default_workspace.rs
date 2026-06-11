//! ensure_default_workspace: creates a "Default" workspace on empty DB,
//! otherwise returns the most-recently created one (highest id).
use weft_app_lib::commands::ensure_default_workspace_inner;
use weft_app_lib::store::{repo, Db};

#[tokio::test]
async fn creates_when_none() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    assert!(repo::list_workspaces(&db).await.unwrap().is_empty());

    let id = ensure_default_workspace_inner(&db).await.unwrap();
    let all = repo::list_workspaces(&db).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, id);
    assert_eq!(all[0].name, "Default");
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
