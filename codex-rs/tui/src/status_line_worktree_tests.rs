use super::*;
use crate::workspace_command::WorkspaceCommandError;
use crate::workspace_command::WorkspaceCommandOutput;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::pin::Pin;

use crate::status_test_support::GitFixture;

#[tokio::test]
async fn status_worktree_tool_validates_host_and_repository() {
    let repo = GitFixture::new();
    let nested = repo.worktree.join("nested directory");
    std::fs::create_dir(&nested).unwrap();
    let (selection, _) = resolve_status_line_worktree(&repo.runner, "local", &repo.root, &nested)
        .await
        .unwrap();
    assert_eq!(PathBuf::from(selection.root.as_str()), repo.worktree);
    assert_eq!(
        PathBuf::from(selection.git_common_dir.as_str()),
        repo.root.join(".git")
    );
    assert!(matches!(
        resolve_status_line_worktree(
            &repo.runner,
            "local",
            &repo.root,
            &repo.worktree.join("missing")
        )
        .await,
        Err(StatusWorktreeError::InvalidCandidate(_))
    ));
    assert!(matches!(
        resolve_status_line_worktree(&repo.runner, "local", &repo.root, Path::new("relative"))
            .await,
        Err(StatusWorktreeError::InvalidCandidate(_))
    ));
    assert!(matches!(
        resolve_status_line_worktree(&repo.runner, "", &repo.root, &repo.worktree).await,
        Err(StatusWorktreeError::Unavailable(_))
    ));
    let other = GitFixture::new();
    assert_eq!(
        resolve_status_line_worktree(&repo.runner, "local", &repo.root, &other.root).await,
        Err(StatusWorktreeError::DifferentRepository)
    );
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        other.root.to_str().unwrap(),
        "child",
    ]);
    assert_eq!(
        resolve_status_line_worktree(&repo.runner, "local", &repo.root, &repo.root.join("child"))
            .await,
        Err(StatusWorktreeError::DifferentRepository)
    );
    #[cfg(unix)]
    {
        let alias = repo.root.join("alias");
        std::os::unix::fs::symlink(&repo.worktree, &alias).unwrap();
        assert_eq!(
            resolve_status_line_worktree(&repo.runner, "local", &repo.root, &alias)
                .await
                .unwrap()
                .0,
            selection
        );
    }
}

struct RemoteRunner {
    calls: Mutex<Vec<Vec<String>>>,
}
impl WorkspaceCommandExecutor for RemoteRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> Pin<
        Box<dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.calls.lock().unwrap().push(command.argv.clone());
            let path = &command.argv[2];
            assert!(path.starts_with("/remote/"));
            let stdout = if command.argv.last().unwrap() == "--git-common-dir" {
                "/remote/repo/.git"
            } else if path == "/remote/repo" {
                "/remote/repo"
            } else {
                "/remote/feature"
            };
            Ok(WorkspaceCommandOutput {
                exit_code: 0,
                stdout: format!("{stdout}\n"),
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test]
async fn status_worktree_tool_resolves_remote_paths_only_on_runner() {
    let runner = RemoteRunner {
        calls: Mutex::new(Vec::new()),
    };
    let (selected, _) = resolve_status_line_worktree(
        &runner,
        "workspace-host",
        Path::new("/remote/repo"),
        Path::new("/remote/feature/nested"),
    )
    .await
    .unwrap();
    assert_eq!(selected.environment_id, "workspace-host");
    assert_eq!(selected.root.as_str(), "/remote/feature");
    assert_eq!(runner.calls.lock().unwrap().len(), 4);
}

fn selection(path: &str) -> StatusLineWorktree {
    StatusLineWorktree {
        environment_id: "local".into(),
        root: LegacyAppPathString::from_string(path),
        git_common_dir: LegacyAppPathString::from_string("/repo/.git"),
    }
}

#[tokio::test]
async fn status_worktree_committed_events_follow_successful_replacements_only() {
    let service = StatusLineWorktreeTools::default();
    let thread = ThreadId::new();
    let first = service.reserve(thread, None).unwrap();
    service
        .publish(&first, selection("/first"), |_| async { Ok(false) })
        .await
        .unwrap();
    service.finish(&first);
    let pending = service.reserve(thread, None).unwrap();
    assert!(service.accepts_selection(&first));
    assert!(!service.accepts_selection(&pending));
    service.cancel_pending(thread);
    assert!(service.accepts_selection(&first));

    let replacement = service.reserve(thread, None).unwrap();
    service
        .publish(&replacement, selection("/replacement"), |_| async {
            Ok(false)
        })
        .await
        .unwrap();
    assert!(!service.accepts_selection(&first));
    assert!(service.accepts_selection(&replacement));
    service.forget_thread(thread);
    assert!(!service.accepts_selection(&replacement));

    let reused = service.reserve(thread, None).unwrap();
    service
        .publish(&reused, selection("/first"), |_| async { Ok(false) })
        .await
        .unwrap();
    assert!(!service.accepts_selection(&first));
    assert!(service.accepts_selection(&reused));
    service.invalidate_connection();
    assert!(!service.accepts_selection(&reused));
}

#[tokio::test]
async fn status_worktree_explicit_selection_supersedes_pending_auto_candidate() {
    let service = StatusLineWorktreeTools::default();
    let id = ThreadId::new();
    let auto = service.reserve(id, Some("/auto".into())).unwrap();
    let explicit = service.reserve(id, None).unwrap();
    let writes = Mutex::new(Vec::new());
    let writer = |value| {
        writes.lock().unwrap().push(value);
        async { Ok(true) }
    };
    let expected = selection("/explicit");
    assert!(
        service
            .publish(&explicit, expected.clone(), writer)
            .await
            .unwrap()
    );
    assert!(
        service
            .publish(&auto, selection("/auto"), writer)
            .await
            .is_err()
    );
    assert_eq!(*writes.lock().unwrap(), vec![Some(expected)]);
    assert!(service.accepts(&explicit));
    assert!(!service.accepts(&auto));
    service.finish(&explicit);
    assert!(service.reserve(id, Some("/auto".into())).is_none());
}

#[tokio::test]
async fn status_worktree_tool_serializes_in_flight_writes_and_compensates_stale_intent() {
    let service = StatusLineWorktreeTools::default();
    let id = ThreadId::new();
    let token = service.reserve(id, Some("/auto".into())).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let channels = Arc::new(Mutex::new(Some((entered, gate))));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let work = {
        let service = service.clone();
        let writes = writes.clone();
        tokio::spawn(async move {
            service
                .publish(&token, selection("/auto"), |value| {
                    writes.lock().unwrap().push(value);
                    let channels = channels.lock().unwrap().take();
                    async move {
                        if let Some((entered, gate)) = channels {
                            entered.send(()).unwrap();
                            gate.await.unwrap();
                        }
                        Ok(true)
                    }
                })
                .await
        })
    };
    ready.await.unwrap();
    let explicit = service.reserve(id, None).unwrap();
    release.send(()).unwrap();
    assert!(work.await.unwrap().is_err());
    let expected = selection("/explicit");
    assert!(
        service
            .publish(&explicit, expected.clone(), |value| {
                writes.lock().unwrap().push(value);
                async { Ok(true) }
            })
            .await
            .unwrap()
    );
    assert_eq!(
        *writes.lock().unwrap(),
        vec![Some(selection("/auto")), None, Some(expected)]
    );
}

#[tokio::test]
async fn status_worktree_tool_rejects_obsolete_connection_and_storage_failure() {
    let service = StatusLineWorktreeTools::default();
    let id = ThreadId::new();
    let stale = service.reserve(id, None).unwrap();
    service.invalidate_connection();
    assert!(
        service
            .publish(&stale, selection("/old"), |_| async {
                panic!("obsolete connection wrote metadata")
            })
            .await
            .is_err()
    );
    let current = service.reserve(id, None).unwrap();
    assert!(
        service
            .publish(&current, selection("/new"), |_| async {
                Err("database unavailable".into())
            })
            .await
            .unwrap_err()
            .contains("database unavailable")
    );
    assert_eq!(service.registry.lock().unwrap().threads[&id].selected, None);
    assert!(
        !service
            .publish(&current, selection("/new"), |_| async { Ok(false) })
            .await
            .unwrap()
    );
    assert_eq!(
        service.registry.lock().unwrap().threads[&id].selected,
        Some(selection("/new"))
    );
    assert_eq!(service.transient_selection(id), Some(selection("/new")));
    service.forget_thread(id);
    assert_eq!(service.transient_selection(id), None);
    assert!(!service.accepts(&current));
}

#[tokio::test]
async fn status_worktree_tool_adopts_during_turn() -> color_eyre::Result<()> {
    crate::test_support::boxed_future!(status_worktree_tool_adopts_during_turn_scenario()).await
}

async fn status_worktree_tool_adopts_during_turn_scenario() -> color_eyre::Result<()> {
    use codex_app_server_protocol::TurnStartParams;
    use codex_app_server_protocol::TurnStartResponse;
    use core_test_support::responses;
    use core_test_support::streaming_sse::StreamingSseChunk;
    use core_test_support::streaming_sse::start_streaming_sse_server;
    let repo = GitFixture::new();
    let home = tempfile::tempdir()?;
    let (release, gate) = tokio::sync::oneshot::channel();
    let (responses, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("resp")]),
        },
        StreamingSseChunk {
            gate: Some(gate),
            body: responses::sse(vec![responses::ev_completed("resp")]),
        },
    ]])
    .await;
    app_test_support::MockResponsesConfig::new(responses.uri()).write(home.path())?;
    let config = crate::legacy_core::config::ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::legacy_core::config::ConfigOverrides {
            cwd: Some(repo.root.clone()),
            ..Default::default()
        })
        .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = AppEventSender::new(tx);
    let (status, _) = tokio::sync::broadcast::channel(16);
    server
        .start_dynamic_tool_mcp(config.clone(), events.clone(), status)
        .await?;
    let started = server.start_thread(&config).await?;
    let id = started.session.thread_id;
    let before = server.thread_read(id, false).await?;
    let active: TurnStartResponse = server
        .request_handle()
        .request_typed(ClientRequest::TurnStart {
            request_id: request_id(),
            params: TurnStartParams {
                thread_id: id.to_string(),
                input: vec![codex_app_server_protocol::UserInput::Text {
                    text: "start".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), responses.wait_for_request_count(1)).await?;
    let service = server.status_worktree_tools();
    let mut overrides = None;
    server.thread_tool_transport().configure_mcp(&mut overrides);
    let overrides = overrides.unwrap();
    let mcp = &overrides["mcp_servers.codex_worktrees"];
    let response = codex_http_client::HttpClientBuilder::new().build_direct()?
        .post(mcp["url"].as_str().unwrap())
        .header("Authorization", mcp["http_headers"]["Authorization"].as_str().unwrap())
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("MCP-Method", "tools/call").header("MCP-Name", TOOL_NAME)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":TOOL_NAME,"arguments":{"path":repo.worktree},
            "_meta":{"threadId":id.to_string(),"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}
        }})).send().await?.text().await?;
    let response: Value = serde_json::from_str(
        response
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(&response),
    )?;
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    assert!(response.get("error").is_none(), "{response}");
    let AppEvent::StatusWorktreeSelected {
        token,
        selection,
        persisted,
    } = rx.recv().await.unwrap()
    else {
        panic!("selection event")
    };
    assert!(persisted);
    assert!(service.accepts(&token));
    let after = server.thread_read(id, true).await?;
    assert_eq!(after.status_line_worktree, Some(selection.clone()));
    assert_eq!(after.cwd, before.cwd);
    assert_eq!(after.environments, before.environments);
    assert_eq!(after.id, before.id);
    assert_eq!(after.session_id, before.session_id);
    assert!(matches!(
        after.status,
        codex_app_server_protocol::ThreadStatus::Active { .. }
    ));
    assert!(after.turns.iter().any(|turn| turn.id == active.turn.id));
    let invalid = service
        .execute(
            server.request_handle(),
            id.to_string(),
            json!({"path":repo.root.join("missing")}),
            &events,
        )
        .await;
    assert!(!invalid.success);
    assert_eq!(
        server.thread_read(id, false).await?.status_line_worktree,
        Some(selection)
    );
    assert!(rx.try_recv().is_err());
    // An argument cannot impersonate another thread; ownership comes from authenticated transport.
    assert!(
        !service
            .execute(
                server.request_handle(),
                id.to_string(),
                json!({"path":repo.root,"threadId":ThreadId::new().to_string()}),
                &events
            )
            .await
            .success
    );
    release.send(()).unwrap();
    Ok(())
}

#[tokio::test]
async fn status_worktree_thread_switch_retires_pending_selection_without_cleanup() {
    let service = StatusLineWorktreeTools::default();
    let thread = ThreadId::new();
    let pending = service.reserve(thread, Some("/auto".into())).unwrap();
    service.cancel_pending(thread);
    assert!(!service.accepts(&pending));
    assert!(
        service
            .publish(&pending, selection("/old"), |_| async {
                panic!("a cancelled candidate must not write metadata")
            })
            .await
            .is_err()
    );
    let explicit = service.reserve(thread, None).unwrap();
    service
        .publish(&explicit, selection("/chosen"), |_| async { Ok(false) })
        .await
        .unwrap();
    service.finish(&explicit);
    service.cancel_pending(thread);
    assert_eq!(
        service.transient_selection(thread),
        Some(selection("/chosen"))
    );
    service.invalidate_connection();
    assert_eq!(service.transient_selection(thread), None);
    assert!(!service.accepts(&explicit));
}

#[tokio::test]
async fn status_worktree_tool_uses_workspace_host_path_roots() {
    struct WindowsRunner;
    impl WorkspaceCommandExecutor for WindowsRunner {
        fn run(
            &self,
            command: WorkspaceCommand,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let cwd = LegacyAppPathString::from_path(&command.cwd);
                let root = cwd.to_inferred_path_uri().unwrap();
                assert!(root.parent().is_none());
                assert!(cwd.as_str().starts_with("C:"));
                let candidate = &command.argv[2];
                let stdout = if command.argv.last().unwrap() == "--git-common-dir" {
                    r"C:\repo\.git"
                } else if candidate == r"C:\repo" {
                    r"C:\repo"
                } else {
                    r"C:\feature checkout"
                };
                Ok(WorkspaceCommandOutput {
                    exit_code: 0,
                    stdout: format!("{stdout}\n"),
                    stderr: String::new(),
                })
            })
        }
    }
    let (selected, _) = resolve_status_line_worktree(
        &WindowsRunner,
        "workspace-host",
        Path::new(r"C:\repo"),
        Path::new(r"C:\feature checkout\nested"),
    )
    .await
    .unwrap();
    assert_eq!(selected.root.as_str(), r"C:\feature checkout");
}

#[tokio::test]
async fn status_worktree_pending_auto_candidate_preserves_later_candidate() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let (seen, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut first = Some((entered, gate));
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap() else {
                continue;
            };
            let response = if request.method == "initialize" {
                json!({"id":request.id,"result":{"userAgent":"status-test"}})
            } else {
                assert_eq!(request.method, "thread/read");
                if let Some((entered, gate)) = first.take() {
                    entered.send(()).unwrap();
                    gate.await.unwrap();
                }
                seen.send(()).unwrap();
                json!({"id":request.id,"error":{"code":-32000,"message":"candidate validation unavailable"}})
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        }
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: url,
            auth_token: None,
        },
        client_name: "status-test".into(),
        client_version: "0.0.0".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    })
    .await
    .unwrap();
    let handle = AppServerRequestHandle::Remote(client.request_handle());
    let service = StatusLineWorktreeTools::default();
    let id = ThreadId::new();
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let sender = AppEventSender::new(tx);
    service.observe_candidate(handle.clone(), id, "/repo/nested".into(), sender.clone());
    ready.await.unwrap();
    service.observe_candidate(handle, id, "/feature".into(), sender);
    release.send(()).unwrap();
    requests.recv().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await
            .is_ok(),
        "a later candidate must be validated after the first candidate fails"
    );
    assert!(events.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn status_worktree_auto_queue_preserves_order_and_explicit_priority() {
    let service = StatusLineWorktreeTools::default();
    let id = ThreadId::new();
    let first = service.reserve(id, Some("/repo/nested".into())).unwrap();
    assert_eq!(
        service.next_candidate(&first).as_deref(),
        Some("/repo/nested")
    );
    assert!(service.reserve(id, Some("/first-feature".into())).is_none());
    assert!(service.reserve(id, Some("/later-feature".into())).is_none());
    assert_eq!(
        service.next_candidate(&first).as_deref(),
        Some("/first-feature")
    );
    service
        .publish(&first, selection("/first-feature"), |_| async { Ok(true) })
        .await
        .unwrap();
    assert_eq!(service.next_candidate(&first), None);
    assert!(service.reserve(id, Some("/later-feature".into())).is_none());

    for invalidation in ["explicit", "switch", "delete", "connection"] {
        let id = ThreadId::new();
        let auto = service.reserve(id, Some("/repo/nested".into())).unwrap();
        assert!(service.reserve(id, Some("/feature".into())).is_none());
        match invalidation {
            "explicit" => {
                let explicit = service.reserve(id, None).unwrap();
                assert!(service.reserve(id, Some("/another".into())).is_none());
                service.finish(&explicit);
            }
            "switch" => service.cancel_pending(id),
            "delete" => service.forget_thread(id),
            "connection" => service.invalidate_connection(),
            _ => unreachable!(),
        }
        assert_eq!(service.next_candidate(&auto), None);
        assert!(!service.accepts(&auto));
    }
}

#[tokio::test]
async fn status_worktree_metadata_transport_distinguishes_unsupported_from_write_failure() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let id = ThreadId::new();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut replies = [
            (-32601, "method not found"),
            (-32000, "database write failed"),
        ]
        .into_iter();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap() else {
                continue;
            };
            let response = if request.method == "initialize" {
                json!({"id":request.id,"result":{"userAgent":"status-test"}})
            } else {
                assert_eq!(request.method, "thread/metadata/update");
                assert_eq!(
                    request.params.as_ref().unwrap()["threadId"],
                    json!(id.to_string())
                );
                let (code, message) = replies.next().unwrap();
                json!({"id":request.id,"error":{"code":code,"message":message}})
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        }
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: url,
            auth_token: None,
        },
        client_name: "status-test".into(),
        client_version: "0.0.0".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    })
    .await
    .unwrap();
    let handle = AppServerRequestHandle::Remote(client.request_handle());
    let service = StatusLineWorktreeTools::default();
    let token = service.reserve(id, None).unwrap();
    let selected = selection("/feature");
    let persisted = service
        .publish(&token, selected.clone(), |value| {
            persist_selection(&handle, id.to_string(), value)
        })
        .await
        .unwrap();
    assert!(!persisted);
    assert_eq!(service.transient_selection(id), Some(selected.clone()));
    let newer = service.reserve(id, None).unwrap();
    let error = service
        .publish(&newer, selection("/other"), |value| {
            persist_selection(&handle, id.to_string(), value)
        })
        .await
        .unwrap_err();
    assert!(error.contains("database write failed"), "{error}");
    assert_eq!(service.transient_selection(id), Some(selected));
    service.invalidate_connection();
    assert_eq!(service.transient_selection(id), None);
    server.abort();
}
