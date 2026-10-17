use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn status_worktree_adopts_first_live_linked_checkout() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    let original_cwd = chat.config.cwd.clone();
    let mut item = begin_exec_with_source(&mut chat, "root", "pwd", ExecCommandSource::Agent);
    while rx.try_recv().is_ok() {}
    let candidate = original_cwd.join("linked-checkout");
    if let AppServerThreadItem::CommandExecution { cwd, id, .. } = &mut item {
        *cwd = candidate.into();
        *id = "implementation".into();
    }
    handle_exec_begin(&mut chat, item);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(format!("{event:?}"));
    }
    assert!(
        events
            .iter()
            .any(|event| event.contains("StatusWorktreeCandidate")),
        "{events:?}"
    );
    assert_eq!(chat.config.cwd, original_cwd);
}

use crate::status_line_refresh::StatusProbeKind;
use crate::status_line_refresh::StatusProbeToken;
use crate::status_line_refresh::StatusRefreshContext;
use crate::status_line_worktree::StatusWorktreeError;
use crate::status_test_support::GitFixture;
use codex_protocol::StatusLineWorktree;
use codex_utils_path_uri::LegacyAppPathString;

fn selection(root: &Path, common: &Path) -> StatusLineWorktree {
    StatusLineWorktree {
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        root: LegacyAppPathString::from_path(root),
        git_common_dir: LegacyAppPathString::from_path(common),
    }
}

fn validation(chat: &mut ChatWidget, result: Result<(), StatusWorktreeError>) {
    chat.update_status_worktree_validation(result);
    chat.refresh_status_surfaces();
}

fn seed_old_values(chat: &mut ChatWidget) {
    chat.status_line_branch = Some("obsolete".into());
    chat.status_line_git_summary = Some(crate::branch_summary::StatusLineGitSummary {
        pull_request: Some(crate::branch_summary::StatusLinePullRequest {
            number: 9,
            url: "https://example.invalid/9".into(),
        }),
        branch_change_stats: Some(crate::branch_summary::GitBranchDiffStats {
            additions: 99,
            deletions: 3,
        }),
    });
}

#[tokio::test]
async fn status_worktree_stays_selected_after_incidental_root_command() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    let original = chat.config.cwd.clone();
    let permissions = chat.config.permissions.effective_permission_profile();
    let saved = selection(
        original.join("feature").as_path(),
        original.join(".git").as_path(),
    );
    seed_old_values(&mut chat);
    chat.select_status_worktree(saved.clone());
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    assert_eq!(chat.status_context_cwd(), Path::new(saved.root.as_str()));
    while rx.try_recv().is_ok() {}
    begin_exec_with_source(&mut chat, "incidental", "pwd", ExecCommandSource::Agent);
    chat.observe_status_worktree_candidate(original.join("another-checkout").to_path_buf());
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event, AppEvent::StatusWorktreeCandidate { .. }));
    }
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert_eq!(chat.config.cwd, original);
    assert_eq!(
        chat.config.permissions.effective_permission_profile(),
        permissions
    );
    // Explicit selection of the original root is sticky too.
    chat.select_status_worktree(selection(
        original.as_path(),
        original.join(".git").as_path(),
    ));
    chat.observe_status_worktree_candidate(original.join("feature").to_path_buf());
    assert_eq!(chat.status_context_cwd(), original.as_path());
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event, AppEvent::StatusWorktreeCandidate { .. }));
    }
}

#[tokio::test]
async fn status_worktree_ignores_replay_and_other_threads() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    let thread = ThreadId::new();
    chat.thread_id = Some(thread);
    let mut item = begin_exec_with_source(&mut chat, "root", "pwd", ExecCommandSource::Agent);
    if let AppServerThreadItem::CommandExecution { cwd, .. } = &mut item {
        *cwd = chat.config.cwd.join("feature").into();
    }
    while rx.try_recv().is_ok() {}
    for (id, replay) in [
        (thread, Some(ReplayKind::ResumeInitialMessages)),
        (ThreadId::new(), None),
    ] {
        chat.handle_server_notification(
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id: id.to_string(),
                turn_id: "turn".into(),
                started_at_ms: 0,
                item: item.clone(),
            }),
            replay,
        );
    }
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event, AppEvent::StatusWorktreeCandidate { .. }));
    }
    assert!(chat.status_worktree.saved.is_none());
}

#[tokio::test]
async fn status_worktree_successful_removal_falls_back_once() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    let original = chat.config.cwd.clone();
    let path = original.join("feature");
    let saved = selection(path.as_path(), original.join(".git").as_path());
    chat.select_status_worktree(saved.clone());
    seed_old_values(&mut chat);
    while rx.try_recv().is_ok() {}
    chat.status_worktree_removed(path.as_path());
    assert_eq!(chat.status_context_cwd(), original.as_path());
    assert_eq!(chat.status_worktree.saved, Some(saved.clone()));
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    let warning = warnings(&mut rx);
    assert!(warning.contains("Cannot use status worktree"), "{warning}");
    assert!(
        warning.contains(path.as_path().to_str().unwrap()),
        "{warning}"
    );
    assert!(warning.contains("the worktree was removed"), "{warning}");
    assert!(warning.contains("Showing session directory"), "{warning}");
    validation(
        &mut chat,
        Err(StatusWorktreeError::InvalidCandidate("missing".into())),
    );
    assert!(warnings(&mut rx).is_empty());
    validation(&mut chat, Ok(()));
    assert_eq!(chat.status_context_cwd(), path.as_path());
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    validation(
        &mut chat,
        Err(StatusWorktreeError::InvalidCandidate(
            "missing again".into(),
        )),
    );
    assert!(warnings(&mut rx).contains("missing again"));
}

#[tokio::test]
async fn status_worktree_unselected_removal_has_no_effect() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(None).await;
    let original = chat.config.cwd.clone();
    let saved = selection(
        original.join("feature").as_path(),
        original.join(".git").as_path(),
    );
    chat.select_status_worktree(saved.clone());
    seed_old_values(&mut chat);
    chat.status_worktree_removed(original.join("other").as_path());
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert_eq!(chat.status_line_branch.as_deref(), Some("obsolete"));
    assert!(!chat.status_worktree.warned);
}

#[tokio::test]
async fn status_worktree_transient_failure_does_not_claim_removal() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    let original = chat.config.cwd.clone();
    let saved = selection(
        original.join("feature").as_path(),
        original.join(".git").as_path(),
    );
    chat.select_status_worktree(saved.clone());
    seed_old_values(&mut chat);
    validation(
        &mut chat,
        Err(StatusWorktreeError::Unavailable("permission denied".into())),
    );
    assert_eq!(chat.status_context_cwd(), Path::new(saved.root.as_str()));
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert!(!chat.status_worktree.fallback);
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn status_worktree_restore_revalidates_selection() {
    let repo = GitFixture::new();
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.current_cwd = Some(repo.root.clone());
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    chat.workspace_command_runner = Some(Arc::new(repo.runner.clone()));
    seed_old_values(&mut chat);
    chat.restore_status_worktree(Some(saved.clone()));
    assert!(!chat.status_worktree.available);
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    chat.local_settings.tui.status_line = Some(vec!["current-dir".into()]);
    chat.tick_status_refresh();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(AppEvent::StatusLineLocalUpdated {
                token,
                validation,
                cwd,
                snapshot,
            }) = rx.recv().await
            {
                break (token, validation, cwd, snapshot);
            }
        }
    })
    .await
    .unwrap();
    chat.apply_status_local_result(event.0, event.1, event.2, event.3);
    assert!(chat.status_worktree.available);
    assert_eq!(chat.status_context_cwd(), repo.worktree);
    assert_eq!(chat.status_worktree.saved, Some(saved));
}

#[tokio::test]
async fn status_worktree_missing_on_resume_warns_once() {
    let repo = GitFixture::new();
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    std::fs::remove_dir_all(&repo.worktree).unwrap();
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.current_cwd = Some(repo.root.clone());
    chat.restore_status_worktree(Some(saved.clone()));
    for _ in 0..2 {
        let result =
            crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
                .await;
        assert!(matches!(
            result,
            Err(StatusWorktreeError::InvalidCandidate(_))
        ));
        validation(&mut chat, result);
    }
    assert_eq!(chat.status_context_cwd(), repo.root);
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert_eq!(
        warnings(&mut rx)
            .matches("Cannot use status worktree")
            .count(),
        1
    );
}

#[tokio::test]
async fn status_worktree_recreated_path_refreshes_before_display() {
    let repo = GitFixture::new();
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.current_cwd = Some(repo.root.clone());
    chat.select_status_worktree(saved.clone());
    seed_old_values(&mut chat);
    repo.git(&[
        "worktree",
        "remove",
        "--force",
        repo.worktree.to_str().unwrap(),
    ]);
    validation(
        &mut chat,
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
            .await,
    );
    let removed_generation = chat.status_worktree.generation;
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "replacement",
        repo.worktree.to_str().unwrap(),
        "main",
    ]);
    validation(
        &mut chat,
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
            .await,
    );
    assert_eq!(chat.status_context_cwd(), repo.worktree);
    assert!(chat.status_worktree.generation > removed_generation);
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    assert!(!chat.status_worktree.warned);
    // A completion from the previous validation cannot undo recovery or warn again.
    let token = StatusProbeToken {
        thread_id: chat.thread_id.unwrap(),
        environment_id: "local".into(),
        generation: removed_generation,
        cwd: repo.worktree.clone(),
        request_id: 0,
    };
    chat.apply_status_local_result(
        token,
        Err(StatusWorktreeError::InvalidCandidate("late missing".into())),
        repo.root.clone(),
        None,
    );
    assert_eq!(chat.status_context_cwd(), repo.worktree);
    assert!(!warnings(&mut rx).contains("late missing"));
}

#[tokio::test]
async fn status_worktree_replacement_repository_is_rejected() {
    let repo = GitFixture::new();
    let other = GitFixture::new();
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    repo.git(&[
        "worktree",
        "remove",
        "--force",
        repo.worktree.to_str().unwrap(),
    ]);
    std::fs::rename(&other.root, &repo.worktree).unwrap();
    assert_eq!(
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
            .await,
        Err(StatusWorktreeError::DifferentRepository)
    );
}

fn warnings(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    super::helpers::drain_insert_history_with(rx, |cell| cell.transcript_lines(4096))
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn status_worktree_creation_alone_does_not_select() {
    let repo = GitFixture::new();
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.current_cwd = Some(repo.root.clone());
    chat.refresh_status_surfaces();
    assert!(repo.worktree.is_dir());
    assert_eq!(chat.status_context_cwd(), repo.root);
    assert!(chat.status_worktree.saved.is_none());
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event, AppEvent::StatusWorktreeCandidate { .. }));
    }
}

#[tokio::test]
async fn status_worktree_missing_registration_is_not_a_live_checkout() {
    use crate::workspace_command::WorkspaceCommand;
    use crate::workspace_command::WorkspaceCommandExecutor;
    let repo = GitFixture::new();
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    std::fs::remove_dir_all(&repo.worktree).unwrap();
    let listed = repo
        .runner
        .run(WorkspaceCommand::new(
            ["git", "worktree", "list", "--porcelain"],
            &repo.root,
        ))
        .await
        .unwrap();
    assert!(listed.stdout.contains(repo.worktree.to_str().unwrap()));
    assert!(matches!(
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
            .await,
        Err(StatusWorktreeError::InvalidCandidate(_))
    ));
}

#[tokio::test]
async fn status_worktree_stale_validation_after_thread_change_is_ignored() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    let old_thread = ThreadId::new();
    chat.thread_id = Some(old_thread);
    let original = chat.config.cwd.clone();
    chat.restore_status_worktree(Some(selection(
        original.join("feature").as_path(),
        original.join(".git").as_path(),
    )));
    let now = tokio::time::Instant::now();
    chat.status_refresh.configure(
        StatusRefreshContext {
            thread_id: old_thread,
            environment_id: chat.status_worktree.environment_id.clone(),
            generation: chat.status_worktree.generation,
            cwd: chat.status_context_cwd().to_path_buf(),
        },
        true,
        false,
        now,
    );
    chat.status_refresh.due(now);
    let pending = chat
        .status_refresh
        .pending(StatusProbeKind::LocalGit)
        .unwrap()
        .clone();
    chat.thread_id = Some(ThreadId::new());
    chat.restore_status_worktree(None);
    chat.apply_status_local_result(
        pending,
        Err(StatusWorktreeError::InvalidCandidate("old missing".into())),
        original.to_path_buf(),
        None,
    );
    assert!(chat.status_worktree.saved.is_none());
    assert_eq!(chat.status_context_cwd(), original.as_path());
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn status_worktree_all_status_surfaces_share_the_effective_directory() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.local_settings.tui.status_line = Some(vec![
        "current-dir".into(),
        "git-branch".into(),
        "branch-changes".into(),
        "pull-request-number".into(),
    ]);
    let root = chat.config.cwd.clone();
    let path = root.join("feature");
    chat.select_status_worktree(selection(path.as_path(), root.join(".git").as_path()));
    assert!(
        chat.status_line_value(StatusLineItem::CurrentDir)
            .unwrap()
            .ends_with("feature")
    );
    assert_eq!(chat.status_line_value(StatusLineItem::GitBranch), None);
    assert_eq!(chat.status_line_value(StatusLineItem::BranchChanges), None);
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        None
    );
    chat.status_line_branch = Some("feature".into());
    chat.status_line_git_summary = Some(crate::branch_summary::StatusLineGitSummary {
        pull_request: Some(crate::branch_summary::StatusLinePullRequest {
            number: 123,
            url: "https://example.invalid/123".into(),
        }),
        branch_change_stats: Some(crate::branch_summary::GitBranchDiffStats {
            additions: 2,
            deletions: 1,
        }),
    });
    chat.refresh_status_surfaces();
    assert_eq!(
        chat.status_line_value(StatusLineItem::GitBranch).as_deref(),
        Some("feature")
    );
    assert_eq!(
        chat.status_line_value(StatusLineItem::BranchChanges)
            .as_deref(),
        Some("+2 -1")
    );
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber)
            .as_deref(),
        Some("PR #123")
    );
    assert_eq!(
        chat.terminal_title_value_for_item(TerminalTitleItem::GitBranch, Instant::now())
            .as_deref(),
        Some("feature")
    );
    assert!(
        chat.terminal_title_value_for_item(TerminalTitleItem::CurrentDir, Instant::now())
            .unwrap()
            .ends_with("feature")
    );
    chat.status_worktree_removed(path.as_path());
    assert_eq!(chat.status_context_cwd(), root.as_path());
    for item in [
        StatusLineItem::GitBranch,
        StatusLineItem::BranchChanges,
        StatusLineItem::PullRequestNumber,
    ] {
        assert_eq!(chat.status_line_value(item), None);
    }
}

#[tokio::test]
async fn status_worktree_restore_rejects_changed_session_repository() {
    let repo = GitFixture::new();
    let other = GitFixture::new();
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    assert_eq!(
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &other.root)
            .await,
        Err(StatusWorktreeError::DifferentRepository)
    );
    // Losing the original checkout still permits the selected path if it is accessible.
    assert_eq!(
        crate::status_line_worktree::validate_saved_selection(
            &repo.runner,
            &saved,
            &repo.root.join("missing")
        )
        .await,
        Ok(())
    );
}

#[tokio::test]
async fn status_worktree_environment_change_does_not_probe_wrong_host() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    let repo = GitFixture::new();
    chat.thread_id = Some(ThreadId::new());
    struct NoHostCommands;
    impl crate::workspace_command::WorkspaceCommandExecutor for NoHostCommands {
        fn run(
            &self,
            _: crate::workspace_command::WorkspaceCommand,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::workspace_command::WorkspaceCommandOutput,
                            crate::workspace_command::WorkspaceCommandError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            panic!("an unavailable executor must not run a command on another host")
        }
    }
    chat.workspace_command_runner = Some(Arc::new(NoHostCommands));
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    chat.status_worktree.environment_id = "another-executor".into();
    chat.restore_status_worktree(Some(saved.clone()));
    chat.local_settings.tui.status_line = Some(vec!["current-dir".into()]);
    chat.tick_status_refresh();
    let AppEvent::StatusLineLocalUpdated {
        token,
        validation: result,
        cwd,
        snapshot,
    } = rx.recv().await.unwrap()
    else {
        panic!("validation event")
    };
    assert!(matches!(result, Err(StatusWorktreeError::Unavailable(_))));
    chat.apply_status_local_result(token, result, cwd, snapshot);
    assert!(!chat.status_worktree_git_available());
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert!(warnings(&mut rx).is_empty());
    assert!(repo.worktree.exists());
}

#[tokio::test]
async fn status_worktree_moved_path_does_not_follow_new_location() {
    let repo = GitFixture::new();
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.current_cwd = Some(repo.root.clone());
    let saved = selection(&repo.worktree, &repo.root.join(".git"));
    chat.select_status_worktree(saved.clone());
    let moved = repo.worktree.with_file_name("moved-checkout");
    repo.git(&[
        "worktree",
        "move",
        repo.worktree.to_str().unwrap(),
        moved.to_str().unwrap(),
    ]);
    let result =
        crate::status_line_worktree::validate_saved_selection(&repo.runner, &saved, &repo.root)
            .await;
    assert!(matches!(
        result,
        Err(StatusWorktreeError::InvalidCandidate(_))
    ));
    validation(&mut chat, result);
    assert_eq!(chat.status_context_cwd(), repo.root);
    assert_eq!(chat.status_worktree.saved, Some(saved));
    assert!(moved.is_dir());
    assert!(warnings(&mut rx).contains("Cannot use status worktree"));
}
