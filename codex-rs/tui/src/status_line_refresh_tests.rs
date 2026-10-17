use super::*;
use pretty_assertions::assert_eq;

fn context(generation: u64, path: &str) -> StatusRefreshContext {
    StatusRefreshContext {
        thread_id: ThreadId::from_string("00000000-0000-0000-0000-000000000001").unwrap(),
        environment_id: "local".into(),
        generation,
        cwd: path.into(),
    }
}
fn started(now: Instant) -> StatusLineRefreshState {
    let mut state = StatusLineRefreshState::default();
    state.configure(context(1, "/a"), true, true, now);
    assert_eq!(
        state.due(now),
        vec![StatusProbeKind::LocalGit, StatusProbeKind::PullRequest]
    );
    state
}
fn accept(state: &mut StatusLineRefreshState, kind: StatusProbeKind) {
    assert!(state.accept(&state.pending(kind).unwrap().clone()));
}

#[test]
fn status_refresh_pr_without_head_change() {
    let now = Instant::now();
    let mut state = started(now);
    accept(&mut state, StatusProbeKind::PullRequest);
    assert!(
        state
            .due(now + PR_INTERVAL - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + PR_INTERVAL),
        vec![StatusProbeKind::PullRequest]
    );
}

#[test]
fn status_refresh_slow_pr_does_not_block_git() {
    let now = Instant::now();
    let mut state = started(now);
    let pr = state.pending(StatusProbeKind::PullRequest).cloned();
    accept(&mut state, StatusProbeKind::LocalGit);
    assert_eq!(state.next_deadline(), Some(now + LOCAL_INTERVAL));
    assert!(
        state
            .due(now + LOCAL_INTERVAL - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + LOCAL_INTERVAL),
        vec![StatusProbeKind::LocalGit]
    );
    assert_eq!(state.pending(StatusProbeKind::PullRequest), pr.as_ref());
}

#[test]
fn status_refresh_preserves_dirty_invalidation() {
    let now = Instant::now();
    let mut state = started(now);
    state.invalidate(now);
    state.invalidate(now + Duration::from_millis(100));
    assert!(state.due(now + EVENT_COALESCE).is_empty());
    accept(&mut state, StatusProbeKind::LocalGit);
    accept(&mut state, StatusProbeKind::PullRequest);
    assert_eq!(state.next_deadline(), Some(now + EVENT_COALESCE));
    assert!(
        state
            .due(now + EVENT_COALESCE - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + EVENT_COALESCE),
        vec![StatusProbeKind::LocalGit]
    );
    assert!(
        state
            .due(now + PR_MIN_INTERVAL - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + PR_MIN_INTERVAL),
        vec![StatusProbeKind::PullRequest]
    );
}

#[test]
fn status_refresh_stale_same_path_result_is_ignored() {
    let now = Instant::now();
    let mut state = started(now);
    let old = state.pending(StatusProbeKind::LocalGit).unwrap().clone();
    state.configure(context(2, "/b"), true, true, now);
    state.due(now);
    state.configure(context(3, "/a"), true, true, now);
    state.due(now);
    let current = state.pending(StatusProbeKind::LocalGit).cloned();
    assert!(!state.accept(&old));
    assert_eq!(state.pending(StatusProbeKind::LocalGit), current.as_ref());
    state.cancel();
    assert!(!state.accept(current.as_ref().unwrap()));
    assert!(state.next_deadline().is_none());
    assert!(state.due(now + PR_INTERVAL).is_empty());
}

#[test]
fn status_refresh_disabled_classes_cancel_and_reenable_immediately() {
    let now = Instant::now();
    let mut state = started(now);
    let old = state.pending(StatusProbeKind::PullRequest).unwrap().clone();
    state.configure(context(1, "/a"), true, false, now);
    assert!(!state.accept(&old));
    assert!(state.pending(StatusProbeKind::LocalGit).is_some());
    state.configure(context(1, "/a"), false, false, now);
    assert!(state.next_deadline().is_none());
    state.configure(context(1, "/a"), true, false, now);
    assert_eq!(state.next_deadline(), Some(now));
    assert_eq!(state.due(now), vec![StatusProbeKind::LocalGit]);
}

#[test]
fn status_refresh_branch_change_preserves_pr_minimum_spacing() {
    let now = Instant::now();
    let mut state = started(now);
    let old = state.pending(StatusProbeKind::PullRequest).unwrap().clone();
    state.invalidate_pull_request_identity(now + Duration::from_secs(1));
    assert!(!state.accept(&old));
    assert!(
        state
            .due(now + PR_MIN_INTERVAL - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + PR_MIN_INTERVAL),
        vec![StatusProbeKind::PullRequest]
    );
}

#[test]
fn status_refresh_temporary_pr_disable_retains_minimum_spacing() {
    let now = Instant::now();
    let mut state = started(now);
    state.configure(context(1, "/a"), true, false, now + Duration::from_secs(1));
    state.configure(context(1, "/a"), true, true, now + Duration::from_secs(2));
    assert!(
        state
            .due(now + PR_MIN_INTERVAL - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        state.due(now + PR_MIN_INTERVAL),
        vec![StatusProbeKind::PullRequest]
    );
}

#[tokio::test]
async fn status_refresh_pending_owns_task_lifetime() {
    let pending_task = || {
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _sender = sender;
            std::future::pending::<()>().await;
        });
        (task, receiver)
    };
    async fn assert_canceled(receiver: tokio::sync::oneshot::Receiver<()>) {
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .expect("retired probe must drop its task")
                .is_err()
        );
    }
    let mut state = started(Instant::now());
    let [(local, local_canceled), (pr, pr_canceled)] =
        [StatusProbeKind::LocalGit, StatusProbeKind::PullRequest].map(|kind| {
            let (task, receiver) = pending_task();
            let handle = task.abort_handle();
            state.attach_task(kind, task);
            (handle, receiver)
        });
    let mut stale = state.pending(StatusProbeKind::LocalGit).unwrap().clone();
    stale.request_id = 0;
    assert!(!state.accept(&stale));
    assert!(!local.is_finished());
    assert!(!pr.is_finished());

    accept(&mut state, StatusProbeKind::LocalGit);
    assert_canceled(local_canceled).await;
    assert!(!pr.is_finished());

    state.cancel();
    assert_canceled(pr_canceled).await;

    let mut state = started(Instant::now());
    let (task, receiver) = pending_task();
    state.attach_task(StatusProbeKind::LocalGit, task);
    drop(state);
    assert_canceled(receiver).await;
}
