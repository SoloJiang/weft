use weft::store::{repo, Db};

async fn secondary_session_fixture(
    db: &Db,
) -> (
    weft::store::entities::workspace::Model,
    weft::store::entities::repo_ref::Model,
    weft::store::entities::repo_ref::Model,
    weft::store::entities::thread::Model,
    weft::store::entities::direction::Model,
    weft::store::entities::session::Model,
) {
    let workspace = repo::create_workspace(db, "ws").await.unwrap();
    let primary = repo::add_repo_ref(
        db,
        workspace.id,
        "primary",
        "/tmp/late-primary",
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let secondary = repo::add_repo_ref(
        db,
        workspace.id,
        "secondary",
        "/tmp/late-secondary",
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let thread = repo::create_thread(db, workspace.id, "issue", "feature", "codex")
        .await
        .unwrap();
    let direction = repo::create_direction(
        db,
        thread.id,
        "implementation",
        "codex",
        primary.id,
        "why",
        "impl-only",
        "",
    )
    .await
    .unwrap();
    let session = repo::create_session(
        db,
        direction.id,
        secondary.id,
        "codex",
        "/tmp/late-secondary-wt",
    )
    .await
    .unwrap();
    (workspace, primary, secondary, thread, direction, session)
}

#[tokio::test]
async fn deleted_secondary_session_cannot_recreate_worker_messages_or_checkpoints() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let (_workspace, _primary, secondary, thread, direction, session) =
        secondary_session_fixture(&db).await;
    let worktree = repo::record_worktree(
        &db,
        secondary.id,
        direction.id,
        "weft/late",
        "/tmp/late-secondary-wt",
        true,
        true,
        "base",
    )
    .await
    .unwrap();
    let source = repo::insert_lead_message(
        &db,
        thread.id,
        Some(session.id),
        1,
        "user",
        "text",
        r#"{"text":"start"}"#,
        "complete",
    )
    .await
    .unwrap();
    repo::insert_code_checkpoint(
        &db,
        worktree.id,
        session.id,
        source.id,
        source.turn_id,
        "shadow",
        "head",
        "[]",
        "index",
    )
    .await
    .unwrap();

    repo::mark_repo_deleting(&db, secondary.id).await.unwrap();
    repo::delete_repo_cascade_with_human_cancellations(&db, secondary.id)
        .await
        .unwrap();

    let late_message = repo::insert_lead_message(
        &db,
        thread.id,
        Some(session.id),
        2,
        "assistant",
        "text",
        r#"{"text":"late"}"#,
        "complete",
    )
    .await;
    assert!(late_message.is_err());
    assert!(repo::insert_code_checkpoint(
        &db,
        worktree.id,
        session.id,
        source.id,
        source.turn_id,
        "late-shadow",
        "late-head",
        "[]",
        "late-index",
    )
    .await
    .is_err());
    assert!(repo::get_session(&db, session.id).await.unwrap().is_none());
    assert!(repo::code_checkpoint_for(&db, worktree.id, source.id)
        .await
        .unwrap()
        .is_none());
    assert!(repo::list_lead_messages(&db, thread.id)
        .await
        .unwrap()
        .iter()
        .all(|message| message.session_id != Some(session.id)));
}

#[tokio::test]
async fn deleted_repo_cannot_be_recreated_by_late_curator_profile_or_map_publish() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let (workspace, primary, secondary, _thread, _direction, _session) =
        secondary_session_fixture(&db).await;
    repo::upsert_repo_profile(
        &db,
        secondary.id,
        "backend",
        "[]",
        "before",
        "[]",
        "agent",
        "sha",
    )
    .await
    .unwrap();
    let expected_ids = vec![primary.id, secondary.id];

    repo::mark_repo_deleting(&db, secondary.id).await.unwrap();
    repo::delete_repo_cascade_with_human_cancellations(&db, secondary.id)
        .await
        .unwrap();

    assert!(repo::upsert_repo_profile(
        &db,
        secondary.id,
        "backend",
        "[]",
        "late",
        "[]",
        "agent",
        "late-sha",
    )
    .await
    .is_err());
    assert!(repo::get_repo_profile(&db, secondary.id)
        .await
        .unwrap()
        .is_none());
    assert!(!repo::set_repo_map_doc_if_repo_ids_match(
        &db,
        workspace.id,
        &expected_ids,
        "## stale map",
    )
    .await
    .unwrap());
    assert!(repo::get_repo_map_doc(&db, workspace.id)
        .await
        .unwrap()
        .is_none());
}
