use super::*;
use crate::status_test_support::GitFixture;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn status_worktree_subagent_switch_keeps_reporting_and_execution_directories_separate()
-> Result<()> {
    let repo = GitFixture::new();
    let child_worktree = repo.root.parent().unwrap().join("child checkout");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "child",
        child_worktree.to_str().unwrap(),
    ]);
    let (mut app, mut events, _ops) = Box::pin(make_test_app_with_channels()).await;
    app.config.cwd = repo.root.clone().abs();
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let primary = server.start_thread(&app.config).await?;
    let primary_id = primary.session.thread_id;
    app.enqueue_primary_thread_session(primary.session, primary.turns)
        .await?;
    let service = server.status_worktree_tools();
    let response = service
        .execute(
            server.request_handle(),
            primary_id.to_string(),
            json!({"path": repo.worktree}),
            &app.app_event_tx,
        )
        .await;
    assert!(response.success, "{response:?}");
    let event = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::StatusWorktreeSelected { .. }))
        .expect("primary selection event");
    Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
    assert_eq!(app.chat_widget.status_context_cwd(), repo.worktree);

    // Model a parent-owned child through notification and picker records backed by a real
    // server thread. This tests TUI routing; the CLI suite separately exercises actual spawning.
    // A complete live session must not need a resume to discard the primary's selection.
    let child = server.start_thread(&app.config).await?;
    let child_id = child.session.thread_id;
    let mut child_thread = server.thread_read(child_id, false).await?;
    child_thread.parent_thread_id = Some(primary_id.to_string());
    child_thread.can_accept_direct_input = Some(false);
    child_thread.source =
        codex_app_server_protocol::SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: primary_id,
            depth: 1,
            agent_path: Some("/root/child".parse().expect("valid agent path")),
            agent_nickname: None,
            agent_role: None,
        });
    child_thread.thread_source = Some(codex_app_server_protocol::ThreadSource::Subagent);
    assert_eq!(child_thread.status_line_worktree, None);
    app.enqueue_thread_notification(
        child_id,
        ServerNotification::ThreadStarted(ThreadStartedNotification {
            thread: child_thread.clone(),
        }),
    )
    .await?;
    let snapshot = app.thread_event_channels[&child_id]
        .store
        .lock()
        .await
        .snapshot();
    assert!(!app.should_refresh_snapshot_session(child_id, false, &snapshot));
    assert_eq!(app.chat_widget.status_context_cwd(), repo.worktree);

    let request_id = app
        .agent_navigation
        .begin_picker_refresh(primary_id)
        .expect("picker refresh");
    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::AgentPickerThreadsLoaded {
            primary_thread_id: primary_id,
            request_id,
            result: Ok(crate::app_event::AgentPickerThreadRefresh {
                threads: vec![child_thread.clone()],
                ..Default::default()
            }),
        },
    ))
    .await?;
    assert!(app.agent_navigation.is_parent_owned(child_id));

    // Background commands must not become selection intent during later history replay.
    app.enqueue_thread_notification(
        child_id,
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: child_id.to_string(),
            turn_id: "child-turn".into(),
            started_at_ms: 0,
            item: ThreadItem::CommandExecution {
                id: "background-command".into(),
                command: "pwd".into(),
                cwd: child_worktree.clone().abs().into(),
                source: codex_app_server_protocol::CommandExecutionSource::Agent,
                status: codex_app_server_protocol::CommandExecutionStatus::InProgress,
                command_actions: Vec::new(),
                model_context: None,
                sandbox_type: None,
                process_id: None,
                plugin_id: None,
                script_path: None,
                aggregated_output: None,
                exit_code: None,
                duration_ms: None,
            },
        }),
    )
    .await?;
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::StatusWorktreeCandidate { .. }))
    );
    Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::SelectAgentThread(child_id)))
        .await?;
    assert!(app.agent_navigation.is_parent_owned(child_id));
    assert_eq!(app.active_thread_id, Some(child_id));
    assert_eq!(app.chat_widget.thread_id(), Some(child_id));
    assert_eq!(app.chat_widget.status_context_cwd(), repo.root);
    assert_eq!(app.chat_widget.config_ref().cwd.as_path(), repo.root);
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::StatusWorktreeCandidate { .. }))
    );
    assert_eq!(
        app.thread_event_channels[&child_id]
            .store
            .lock()
            .await
            .session
            .as_ref()
            .unwrap()
            .status_line_worktree,
        None
    );

    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::SelectAgentThread(primary_id),
    ))
    .await?;
    assert_eq!(app.chat_widget.status_context_cwd(), repo.worktree);

    // Explicit selection belongs to the calling child even while the parent is foreground.
    let response = service
        .execute(
            server.request_handle(),
            child_id.to_string(),
            json!({"path": child_worktree}),
            &app.app_event_tx,
        )
        .await;
    assert!(response.success, "{response:?}");
    let event = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::StatusWorktreeSelected { .. }))
        .expect("child selection event");
    Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
    assert_eq!(app.chat_widget.thread_id(), Some(primary_id));
    assert_eq!(app.chat_widget.status_context_cwd(), repo.worktree);

    for (thread_id, expected_path) in [(child_id, &child_worktree), (primary_id, &repo.worktree)] {
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::SelectAgentThread(thread_id),
        ))
        .await?;
        assert_eq!(app.chat_widget.thread_id(), Some(thread_id));
        assert_eq!(app.chat_widget.status_context_cwd(), expected_path);
        assert_eq!(app.chat_widget.config_ref().cwd.as_path(), repo.root);
        while events.try_recv().is_ok() {}
        app.chat_widget.insert_str("/pwd");
        app.chat_widget
            .handle_key_event(KeyEvent::from(KeyCode::Enter));
        let output = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => {
                    Some(lines_to_single_string(&cell.display_lines(200)))
                }
                _ => None,
            })
            .collect::<String>();
        if thread_id == primary_id {
            assert!(
                output.contains(&format!(
                    "Current working directory: {}",
                    repo.root.display()
                )),
                "{output}"
            );
        } else {
            // Parent-owned views retain their existing command restrictions, including `/pwd`.
            assert!(app.agent_navigation.is_parent_owned(child_id));
            assert!(
                output.contains(
                    "This sub-agent is controlled by its parent. Direct input is disabled."
                ),
                "{output}"
            );
            assert!(!output.contains("Current working directory:"), "{output}");
        }
    }
    let persisted_child = server.thread_read(child_id, false).await?;
    assert_eq!(persisted_child.cwd, child_thread.cwd);
    assert_eq!(persisted_child.environments, child_thread.environments);
    assert_eq!(
        persisted_child.status_line_worktree.unwrap().root.as_str(),
        child_worktree.to_str().unwrap()
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn status_worktree_committed_selection_survives_failed_newer_intent() -> Result<()> {
    let repo = GitFixture::new();
    let (mut app, mut events, _ops) = Box::pin(make_test_app_with_channels()).await;
    app.config.cwd = repo.root.clone().abs();
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    let original = server.thread_read(thread_id, false).await?;
    let service = server.status_worktree_tools();
    let selected = service
        .execute(
            server.request_handle(),
            thread_id.to_string(),
            json!({"path": repo.worktree}),
            &app.app_event_tx,
        )
        .await;
    assert!(selected.success, "{selected:?}");

    // Keep the successful event queued while a later intent fails validation. MCP calls run
    // independently of the UI event loop, so a rejected attempt must not discard this update.
    let rejected = service
        .execute(
            server.request_handle(),
            thread_id.to_string(),
            json!({"path": repo.root.join("missing")}),
            &app.app_event_tx,
        )
        .await;
    assert!(!rejected.success);
    let persisted = server.thread_read(thread_id, false).await?;
    assert_eq!(persisted.cwd, original.cwd);
    assert_eq!(persisted.environments, original.environments);
    assert_eq!(
        persisted
            .status_line_worktree
            .as_ref()
            .unwrap()
            .root
            .as_str(),
        repo.worktree.to_str().unwrap()
    );

    let event = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::StatusWorktreeSelected { .. }))
        .expect("successful selection event must be queued");
    let mut tui = crate::tui::test_support::make_test_tui()?;
    Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
    assert_eq!(app.chat_widget.status_context_cwd(), repo.worktree);
    assert_eq!(app.chat_widget.thread_id(), Some(thread_id));
    assert_eq!(app.config.cwd.as_path(), repo.root);
    assert_eq!(
        app.primary_session_configured
            .as_ref()
            .unwrap()
            .status_line_worktree,
        persisted.status_line_worktree
    );
    assert_eq!(
        app.thread_event_channels[&thread_id]
            .store
            .lock()
            .await
            .session
            .as_ref()
            .unwrap()
            .status_line_worktree,
        persisted.status_line_worktree
    );
    server.shutdown().await?;
    Ok(())
}
