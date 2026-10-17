use super::*;
use crate::branch_summary::GitBranchDiffStats;
use crate::branch_summary::GitCheckoutIdentity;
use crate::status_line_refresh::StatusProbeKind;
use crate::workspace_command::*;
use codex_protocol::StatusLineWorktree;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

struct RefreshRunner {
    additions: AtomicU64,
    pr: AtomicU64,
    pr_calls: AtomicU64,
    hold_pr: bool,
    selected: std::sync::Mutex<Option<PathBuf>>,
    present: AtomicBool,
    local_calls: AtomicU64,
    validations: AtomicU64,
    release_pr: tokio::sync::Semaphore,
}
impl RefreshRunner {
    fn new(hold_pr: bool) -> Self {
        Self {
            selected: std::sync::Mutex::new(None),
            present: true.into(),
            local_calls: 0.into(),
            validations: 0.into(),
            additions: 0.into(),
            pr: 7.into(),
            pr_calls: 0.into(),
            hold_pr,
            release_pr: tokio::sync::Semaphore::new(0),
        }
    }
}
impl WorkspaceCommandExecutor for RefreshRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let (exit_code, stdout) = if command.argv[0] == "gh" {
                if command.argv[1] == "pr" {
                    self.pr_calls.fetch_add(1, Ordering::SeqCst);
                    if self.hold_pr {
                        self.release_pr.acquire().await.unwrap().forget();
                    }
                    let number = self.pr.load(Ordering::SeqCst);
                    (0, serde_json::json!({"number":number,"url":format!("https://example.invalid/{number}"),"state":if number == 0 { "CLOSED" } else { "OPEN" }}).to_string())
                } else {
                    (1, String::new())
                }
            } else {
                assert_eq!(command.argv[0], "git");
                if command.argv[1] == "-C" {
                    self.validations.fetch_add(1, Ordering::SeqCst);
                    let path = PathBuf::from(&command.argv[2]);
                    if self.selected.lock().unwrap().as_ref() == Some(&path)
                        && !self.present.load(Ordering::SeqCst)
                    {
                        return Ok(WorkspaceCommandOutput {
                            exit_code: 128,
                            stdout: String::new(),
                            stderr: "No such file or directory".into(),
                        });
                    }
                    return Ok(WorkspaceCommandOutput {
                        exit_code: 0,
                        stdout: if command.argv.last().unwrap() == "--git-common-dir" {
                            "/common\n".into()
                        } else {
                            format!("{}\n", path.display())
                        },
                        stderr: String::new(),
                    });
                }
                self.local_calls.fetch_add(1, Ordering::SeqCst);
                match command.argv[3].as_str() {
                    "branch" => (0, "feature\n".into()),
                    "rev-parse" => (0, "head-sha\n".into()),
                    "merge-base" => (0, "base-sha\n".into()),
                    "remote" | "ls-files" => (0, String::new()),
                    "diff" => (
                        0,
                        format!("{}\t0\tfile\0", self.additions.load(Ordering::SeqCst)),
                    ),
                    unexpected => panic!("unexpected Git command {unexpected}"),
                }
            };
            Ok(WorkspaceCommandOutput {
                exit_code,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

fn prepare(chat: &mut ChatWidget, runner: Arc<RefreshRunner>, items: &[&str]) {
    chat.thread_id = Some(ThreadId::new());
    chat.workspace_command_runner = Some(runner);
    chat.local_settings.tui.status_line =
        Some(items.iter().map(|item| (*item).to_owned()).collect());
    chat.local_settings.tui.terminal_title = Some(vec![]);
    tokio::time::pause();
    chat.refresh_status_surfaces();
}

async fn settle(
    chat: &mut ChatWidget,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    wanted: StatusProbeKind,
) {
    loop {
        let kind = match rx.recv().await.unwrap() {
            AppEvent::StatusLineLocalUpdated {
                token,
                validation,
                cwd,
                snapshot,
            } => {
                chat.apply_status_local_result(token, validation, cwd, snapshot);
                StatusProbeKind::LocalGit
            }
            AppEvent::StatusLinePullRequestUpdated {
                token,
                identity,
                pull_request,
            } => {
                chat.apply_status_pr_result(token, identity, pull_request);
                StatusProbeKind::PullRequest
            }
            _ => continue,
        };
        if kind == wanted {
            break;
        }
    }
}

#[tokio::test]
async fn status_refresh_external_pr_creation_and_closure_without_commit() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(false));
    runner.pr.store(0, Ordering::SeqCst);
    prepare(
        &mut chat,
        runner.clone(),
        &["git-branch", "pull-request-number"],
    );
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    settle(&mut chat, &mut rx, StatusProbeKind::PullRequest).await;
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        None
    );
    let identity = chat.status_local_identity.clone();
    runner.pr.store(42, Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(59999)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert_eq!(runner.pr_calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::PullRequest).await;
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        Some("PR #42".into())
    );
    assert_eq!(chat.status_local_identity, identity);
    runner.pr.store(0, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::PullRequest).await;
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        None
    );
    assert_eq!(chat.status_local_identity, identity);
}

#[tokio::test]
async fn status_refresh_slow_pr_allows_busy_local_content_updates() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(true));
    prepare(
        &mut chat,
        runner.clone(),
        &["git-branch", "branch-changes", "pull-request-number"],
    );
    chat.on_task_started();
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    let pr_token = chat
        .status_refresh
        .pending(StatusProbeKind::PullRequest)
        .unwrap()
        .clone();
    runner.additions.store(2, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert_eq!(
        chat.status_line_git_summary
            .as_ref()
            .unwrap()
            .branch_change_stats,
        Some(GitBranchDiffStats {
            additions: 2,
            deletions: 0
        })
    );
    assert_eq!(
        chat.status_refresh.pending(StatusProbeKind::PullRequest),
        Some(&pr_token)
    );
    assert!(chat.bottom_pane.is_task_running());
    runner.release_pr.add_permits(1);
    settle(&mut chat, &mut rx, StatusProbeKind::PullRequest).await;
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        Some("PR #7".into())
    );
}

#[tokio::test]
async fn status_refresh_timer_wakes_idle_tui_without_input_or_output() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    prepare(
        &mut chat,
        Arc::new(RefreshRunner::new(false)),
        &["git-branch"],
    );
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    // Install the real scheduler after initial publication, excluding startup redraws. Measure
    // against the controller's absolute deadline: receiving a frame can advance paused time.
    let (draw_tx, mut draw_rx) = tokio::sync::broadcast::channel(16);
    chat.frame_requester = crate::tui::FrameRequester::new(draw_tx);
    chat.tick_status_refresh();
    let deadline = chat.status_refresh.next_deadline().unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(deadline - tokio::time::Instant::now() - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(draw_rx.try_recv().is_err());
    assert!(rx.try_recv().is_err());
    tokio::time::advance(Duration::from_millis(2)).await;
    draw_rx.recv().await.unwrap();
    chat.pre_draw_tick();
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::LocalGit)
            .is_some()
    );
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
}

#[tokio::test]
async fn status_refresh_disconnection_cancels_both_classes_and_rejects_late_events() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(true));
    prepare(&mut chat, runner, &["git-branch", "pull-request-number"]);
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    let pr = chat
        .status_refresh
        .pending(StatusProbeKind::PullRequest)
        .unwrap()
        .clone();
    tokio::time::advance(Duration::from_secs(5)).await;
    chat.pre_draw_tick();
    let local = chat
        .status_refresh
        .pending(StatusProbeKind::LocalGit)
        .unwrap()
        .clone();
    let cwd = chat.status_context_cwd().to_path_buf();
    chat.set_status_worktree_live(false);
    chat.apply_status_local_result(local, Ok(()), cwd, None);
    chat.apply_status_pr_result(
        pr,
        Some(GitCheckoutIdentity {
            branch: Some("obsolete".into()),
            head: Some("old".into()),
        }),
        Some(crate::branch_summary::StatusLinePullRequest {
            number: 9,
            url: "https://example.invalid/9".into(),
        }),
    );
    assert!(chat.status_refresh.next_deadline().is_none());
    assert!(chat.status_line_branch.is_none());
    assert!(chat.status_line_git_summary.is_none());
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::LocalGit)
            .is_none()
    );
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::PullRequest)
            .is_none()
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    assert!(chat.status_refresh.next_deadline().is_none());
    chat.set_status_worktree_live(true);
    chat.pre_draw_tick();
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::LocalGit)
            .is_some()
    );
}

fn select(chat: &mut ChatWidget, runner: &RefreshRunner) -> PathBuf {
    let path = chat.config.cwd.join("selected").to_path_buf();
    *runner.selected.lock().unwrap() = Some(path.clone());
    chat.select_status_worktree(StatusLineWorktree {
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        root: LegacyAppPathString::from_path(&path),
        git_common_dir: LegacyAppPathString::from_string("/common"),
    });
    path
}

#[tokio::test]
async fn status_refresh_directory_only_validates_recovery_without_github() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(false));
    prepare(&mut chat, runner.clone(), &["current-dir"]);
    assert!(chat.status_refresh.next_deadline().is_none());
    let original = chat.config.cwd.clone();
    let selected = select(&mut chat, &runner);
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert_eq!(chat.status_context_cwd(), selected);
    let mut history = String::new();
    for present in [false, false, true, false] {
        runner.present.store(present, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(5)).await;
        chat.pre_draw_tick();
        settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
        assert_eq!(
            chat.status_context_cwd(),
            if present {
                selected.as_path()
            } else {
                original.as_path()
            }
        );
        assert_eq!(
            chat.status_worktree.saved.as_ref().unwrap().root.as_str(),
            selected.to_str().unwrap()
        );
        history.push_str(&history_text(&mut rx));
    }
    assert_eq!(
        history.matches("Cannot use status worktree").count(),
        2,
        "{history}"
    );
    assert!(runner.validations.load(Ordering::SeqCst) >= 8);
    assert_eq!(runner.local_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.pr_calls.load(Ordering::SeqCst), 0);
    chat.local_settings.tui.status_line = Some(vec!["model-name".into()]);
    chat.refresh_status_surfaces();
    assert!(chat.status_refresh.next_deadline().is_none());
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::LocalGit)
            .is_none()
    );
}

#[tokio::test]
async fn status_refresh_removal_and_recreation_reject_in_flight_results() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(true));
    prepare(
        &mut chat,
        runner.clone(),
        &["current-dir", "git-branch", "pull-request-number"],
    );
    let selected = select(&mut chat, &runner);
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    let old_pr = chat
        .status_refresh
        .pending(StatusProbeKind::PullRequest)
        .unwrap()
        .clone();
    tokio::time::advance(Duration::from_secs(5)).await;
    chat.pre_draw_tick();
    let old_local = chat
        .status_refresh
        .pending(StatusProbeKind::LocalGit)
        .unwrap()
        .clone();
    runner.present.store(false, Ordering::SeqCst);
    chat.status_worktree_removed(&selected);
    let mut history = history_text(&mut rx);
    assert_eq!(chat.status_context_cwd(), chat.config.cwd.as_path());
    chat.apply_status_local_result(old_local.clone(), Ok(()), selected.clone(), None);
    chat.apply_status_pr_result(
        old_pr,
        Some(GitCheckoutIdentity {
            branch: Some("feature".into()),
            head: Some("head-sha".into()),
        }),
        Some(crate::branch_summary::StatusLinePullRequest {
            number: 99,
            url: "https://example.invalid/99".into(),
        }),
    );
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        None
    );
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    runner.present.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert_eq!(chat.status_context_cwd(), selected);
    assert_eq!(
        chat.status_line_value(StatusLineItem::GitBranch),
        Some("feature".into())
    );
    chat.apply_status_local_result(
        old_local,
        Err(
            crate::status_line_worktree::StatusWorktreeError::InvalidCandidate(
                "late missing".into(),
            ),
        ),
        chat.config.cwd.to_path_buf(),
        None,
    );
    assert_eq!(chat.status_context_cwd(), selected);
    assert!(!chat.status_worktree.warned);
    history.push_str(&history_text(&mut rx));
    assert_eq!(history.matches("Cannot use status worktree").count(), 1);
}

fn history_text(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    helpers::drain_insert_history_transcript(rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn status_refresh_transient_validation_preserves_pr_spacing() {
    let (mut chat, mut rx, _) = make_chatwidget_manual(None).await;
    let runner = Arc::new(RefreshRunner::new(true));
    prepare(
        &mut chat,
        runner.clone(),
        &["current-dir", "git-branch", "pull-request-number"],
    );
    let selected = select(&mut chat, &runner);
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::PullRequest)
            .is_some()
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    chat.invalidate_status_refresh();
    tokio::time::advance(Duration::from_millis(250)).await;
    chat.pre_draw_tick();
    let local = chat
        .status_refresh
        .pending(StatusProbeKind::LocalGit)
        .unwrap()
        .clone();
    chat.apply_status_local_result(
        local,
        Err(
            crate::status_line_worktree::StatusWorktreeError::Unavailable(
                "temporary connection failure".into(),
            ),
        ),
        selected.clone(),
        None,
    );
    assert_eq!(chat.status_context_cwd(), selected);
    assert_eq!(
        chat.status_line_value(StatusLineItem::PullRequestNumber),
        None
    );
    chat.invalidate_status_refresh();
    tokio::time::advance(Duration::from_millis(250)).await;
    chat.pre_draw_tick();
    settle(&mut chat, &mut rx, StatusProbeKind::LocalGit).await;
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::PullRequest)
            .is_none(),
        "recovery at 1.5 s must preserve the same-context ten-second PR spacing"
    );
    tokio::time::advance(Duration::from_millis(8499)).await;
    chat.pre_draw_tick();
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::PullRequest)
            .is_none()
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    chat.pre_draw_tick();
    assert!(
        chat.status_refresh
            .pending(StatusProbeKind::PullRequest)
            .is_some()
    );
}
