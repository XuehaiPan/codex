//! Saved reporting paths survive history reconciliation without changing execution settings.

use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

use super::LocalThreadStore;
use super::test_support::test_config;
use super::test_support::write_session_file;
use crate::ReadThreadParams;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::UpdateThreadMetadataParams;

#[test]
fn status_worktree_metadata_patch_omitted_vs_null() -> Result<(), Box<dyn std::error::Error>> {
    let selection =
        json!({"environmentId": "local", "root": "/repo/worktree", "gitCommonDir": "/repo/.git"});
    let mut patch: ThreadMetadataPatch =
        serde_json::from_value(json!({"status_line_worktree": selection}))?;
    patch.merge(serde_json::from_value(json!({"title": "updated"}))?);
    assert_eq!(
        serde_json::to_value(&patch)?["status_line_worktree"],
        selection
    );
    patch.merge(serde_json::from_value(
        json!({"status_line_worktree": null}),
    )?);
    assert_eq!(
        serde_json::to_value(&patch)?.get("status_line_worktree"),
        Some(&serde_json::Value::Null)
    );
    assert!(
        serde_json::to_value(ThreadMetadataPatch::default())?
            .get("status_line_worktree")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = write_session_file(home.path(), "2025-01-03T14-20-00", uuid)?;
    let original_rollout = std::fs::read(&rollout_path)?;
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    // The checkout deliberately does not exist. Reporting metadata must not delete a saved path
    // merely because the UI temporarily falls back while that path is unavailable.
    let selection = json!({"environmentId": "local", "root": "/missing/worktree", "gitCommonDir": "/repo/.git"});
    for value in [selection.clone(), serde_json::Value::Null] {
        let updated = store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id,
                patch: serde_json::from_value(json!({"status_line_worktree": value}))?,
                include_archived: false,
            })
            .await?
            .expect("updated thread");
        assert_eq!(
            serde_json::to_value(&updated)?.get("status_line_worktree"),
            Some(&value)
        );
        codex_rollout::state_db::reconcile_rollout(
            Some(runtime.as_ref()),
            &rollout_path,
            &config.default_model_provider_id,
            None,
            &[],
            None,
            None,
        )
        .await;
        let cold_store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
        for include_history in [false, true] {
            let reread = cold_store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history,
                })
                .await?;
            assert_eq!(
                serde_json::to_value(&reread)?["status_line_worktree"],
                value
            );
            assert_eq!(
                std::fs::canonicalize(&reread.cwd)?,
                std::fs::canonicalize(&updated.cwd)?
            );
        }
    }
    assert_eq!(std::fs::read(rollout_path)?, original_rollout);
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_requires_sqlite() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    write_session_file(home.path(), "2025-01-03T14-20-00", uuid)?;
    let store = LocalThreadStore::new(test_config(home.path()), None);
    let result = store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: serde_json::from_value(json!({"status_line_worktree": null}))?,
            include_archived: false,
        })
        .await;
    assert!(matches!(
        result,
        Err(crate::ThreadStoreError::Internal { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_paginated_and_old_record()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    super::test_support::write_session_file_with_history_mode(
        home.path(),
        "2025-01-03T14-20-00",
        uuid,
        codex_protocol::protocol::ThreadHistoryMode::Paginated,
    )?;
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(runtime));
    let selection =
        json!({"environmentId": "remote", "root": "/worktrees/a", "gitCommonDir": "/repo/.git"});
    store
        .stage_pending_thread_metadata(
            thread_id,
            serde_json::from_value(json!({"status_line_worktree": selection}))?,
        )
        .await?;
    let updated = store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                title: Some("updated".into()),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("updated thread");
    assert_eq!(
        serde_json::to_value(&updated)?["status_line_worktree"],
        selection
    );
    let mut old_record = serde_json::to_value(updated)?;
    old_record
        .as_object_mut()
        .unwrap()
        .remove("status_line_worktree");
    let decoded: crate::StoredThread = serde_json::from_value(old_record)?;
    assert_eq!(decoded.status_line_worktree, None);
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_write_failure_is_not_success()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    write_session_file(home.path(), "2025-01-03T14-20-00", uuid)?;
    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(runtime.clone()));
    let params = UpdateThreadMetadataParams {
        thread_id,
        patch: serde_json::from_value(
            json!({"status_line_worktree": {"environmentId": "local", "root": "/worktree", "gitCommonDir": "/repo/.git"}}),
        )?,
        include_archived: false,
    };
    assert!(
        store
            .update_thread_metadata(params.clone())
            .await?
            .is_some()
    );
    runtime.close().await;
    let result = store
        .update_thread_metadata(UpdateThreadMetadataParams {
            patch: serde_json::from_value(json!({"status_line_worktree": null}))?,
            ..params
        })
        .await;
    assert!(matches!(
        result,
        Err(crate::ThreadStoreError::Internal { .. })
    ));
    Ok(())
}
