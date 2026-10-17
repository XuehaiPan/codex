//! Regression coverage for batched metadata reads and independent row decoding failures.

use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::StateRuntime;
use crate::SqliteConfig;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;

#[tokio::test]
async fn get_threads_preserves_valid_metadata_across_batches() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let mut ids = (0..1_001).map(|_| ThreadId::new()).collect::<Vec<_>>();
    let mut expected = HashMap::new();
    for id in [ids[0], ids[1_000]] {
        let mut metadata = test_thread_metadata(&home, id, home.clone());
        metadata.title = format!("Title for {id}");
        metadata.name = Some(format!("Name for {id}"));
        runtime.upsert_thread(&metadata).await?;
        expected.insert(id, runtime.get_thread(id).await?.expect("persisted thread"));
    }

    // An unreadable record must not discard the other names in the same page.
    let invalid = test_thread_metadata(&home, ids[1], home.clone());
    runtime.upsert_thread(&invalid).await?;
    sqlx::query("UPDATE threads SET created_at_ms = ? WHERE id = ?")
        .bind(i64::MAX)
        .bind(invalid.id.to_string())
        .execute(runtime.pool.as_ref())
        .await?;
    assert!(runtime.get_thread(invalid.id).await.is_err());
    ids.push(ids[0]);

    assert_eq!(runtime.get_threads(&ids).await?, expected);
    assert_eq!(runtime.get_threads(&[]).await?, HashMap::new());
    runtime.close().await;
    std::fs::remove_dir_all(home)?;
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_survives_reconciliation() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    std::fs::create_dir_all(&home)?;
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let runtime = StateRuntime::init(config.clone(), "test-provider".to_string()).await?;
    let id = ThreadId::new();
    let metadata = test_thread_metadata(home.as_path(), id, home.as_path().join("session"));
    runtime.upsert_thread(&metadata).await?;
    let selection = codex_protocol::StatusLineWorktree {
        environment_id: "remote-environment".to_string(),
        root: serde_json::from_value(serde_json::json!("/remote/missing/worktree"))?,
        git_common_dir: serde_json::from_value(serde_json::json!("/remote/repo/.git"))?,
    };
    assert!(
        runtime
            .set_thread_status_line_worktree(id, Some(&selection))
            .await?
    );
    let mut stale = runtime.get_thread(id).await?.expect("saved metadata");
    for expected in [Some(selection.clone()), None] {
        assert!(
            runtime
                .set_thread_status_line_worktree(id, expected.as_ref())
                .await?
        );
        stale.title = "unrelated update".to_string();
        runtime.upsert_thread(&stale).await?;
        let actual = runtime.get_thread(id).await?.expect("saved metadata");
        assert_eq!(actual.status_line_worktree, expected);
        assert_eq!(actual.cwd, metadata.cwd);
        assert_eq!(actual.sandbox_policy, metadata.sandbox_policy);
        assert_eq!(actual.approval_mode, metadata.approval_mode);
        assert_eq!(actual.title, "unrelated update");
        // A fork/new row cannot inherit reporting selection through an ordinary metadata upsert.
        stale.id = ThreadId::new();
        runtime.upsert_thread(&stale).await?;
        assert_eq!(
            runtime
                .get_thread(stale.id)
                .await?
                .unwrap()
                .status_line_worktree,
            None
        );
        stale.id = id;
    }
    assert!(
        !runtime
            .set_thread_status_line_worktree(ThreadId::new(), Some(&selection))
            .await?
    );
    assert!(
        runtime
            .set_thread_status_line_worktree(id, Some(&selection))
            .await?
    );
    runtime.close().await;
    let reopened = StateRuntime::init(config, "test-provider".to_string()).await?;
    assert_eq!(
        reopened.get_thread(id).await?.unwrap().status_line_worktree,
        Some(selection)
    );
    reopened.close().await;
    std::fs::remove_dir_all(home)?;
    Ok(())
}

#[tokio::test]
async fn status_worktree_metadata_migrates_populated_database() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    std::fs::create_dir_all(&home)?;
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let pool = config.open_read_write_pool(&config.state_db_path()).await?;
    let migrator = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::migrations::STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= 61)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: crate::migrations::STATE_MIGRATOR.table_name.clone(),
        create_schemas: crate::migrations::STATE_MIGRATOR.create_schemas.clone(),
    };
    migrator.run(&pool).await?;
    let id = ThreadId::new();
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES (?, '/saved.jsonl', 1, 2, 'cli', 'test', '/session', 'keep me', 'read-only', 'on-request')")
        .bind(id.to_string()).execute(&pool).await?;
    pool.close().await;
    let runtime = StateRuntime::init(config, "test".to_string()).await?;
    let actual = runtime.get_thread(id).await?.expect("migrated thread");
    assert_eq!(actual.status_line_worktree, None);
    assert_eq!(actual.title, "keep me");
    assert_eq!(actual.cwd, std::path::PathBuf::from("/session"));
    runtime.close().await;
    std::fs::remove_dir_all(home)?;
    Ok(())
}
