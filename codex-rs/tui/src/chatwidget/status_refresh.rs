//! TUI scheduling and publication of independent local Git and GitHub results.
use super::status_surfaces::StatusSurfaceSelections;
use super::*;
use crate::branch_summary::GitCheckoutIdentity;
use crate::branch_summary::StatusLineLocalGit;
use crate::branch_summary::StatusLinePullRequest;
use crate::branch_summary::{self};
use crate::status_line_refresh::StatusProbeKind;
use crate::status_line_refresh::StatusProbeToken;
use crate::status_line_refresh::StatusRefreshContext;
use crate::status_line_worktree::StatusWorktreeError;
use tokio::time::Instant as ProbeInstant;

impl ChatWidget {
    fn configure_status_refresh(
        &mut self,
        selections: &StatusSurfaceSelections,
        now: ProbeInstant,
    ) {
        let Some(thread_id) = self
            .thread_id
            .filter(|_| self.status_worktree.live && self.workspace_command_runner.is_some())
        else {
            self.status_refresh.cancel();
            return;
        };
        let uses_pr = selections
            .status_line_items
            .contains(&StatusLineItem::PullRequestNumber);
        let uses_counts = selections
            .status_line_items
            .contains(&StatusLineItem::BranchChanges);
        let uses_path = selections.status_line_items.iter().any(|item| {
            matches!(
                item,
                StatusLineItem::CurrentDir | StatusLineItem::ProjectRoot
            )
        }) || selections.terminal_title_items.iter().any(|item| {
            matches!(
                item,
                TerminalTitleItem::CurrentDir | TerminalTitleItem::Project
            )
        });
        let local = selections.uses_git_branch()
            || uses_counts
            || uses_pr
            || (uses_path && self.status_worktree.saved.is_some());
        self.status_refresh.configure(
            StatusRefreshContext {
                thread_id,
                environment_id: self.status_worktree.environment_id.clone(),
                generation: self.status_worktree.generation,
                cwd: self.status_context_cwd().to_path_buf(),
            },
            local,
            uses_pr && self.status_worktree_git_available() && self.status_local_identity.is_some(),
            now,
        );
    }

    pub(super) fn sync_status_refresh(&mut self, selections: &StatusSurfaceSelections) {
        self.configure_status_refresh(selections, ProbeInstant::now());
        if !selections.uses_git_branch() || !self.status_worktree_git_available() {
            self.status_line_branch = None;
        }
        if !selections.uses_git_summary() || !self.status_worktree_git_available() {
            self.status_line_git_summary = None;
        } else if let Some(summary) = &mut self.status_line_git_summary {
            if !selections
                .status_line_items
                .contains(&StatusLineItem::BranchChanges)
            {
                summary.branch_change_stats = None;
            }
            if !selections
                .status_line_items
                .contains(&StatusLineItem::PullRequestNumber)
            {
                summary.pull_request = None;
            }
        }
        self.launch_status_probes(selections);
    }

    pub(crate) fn invalidate_status_refresh(&mut self) {
        self.status_refresh.invalidate(ProbeInstant::now());
        self.schedule_status_refresh();
    }

    pub(super) fn tick_status_refresh(&mut self) {
        let selections = self.status_surface_selections();
        self.sync_status_refresh(&selections);
    }

    fn schedule_status_refresh(&self) {
        if let Some(deadline) = self.status_refresh.next_deadline() {
            // Convert the absolute deadline, keeping paused-clock tests consistent with the frame
            // scheduler's std Instant API. No timer task or polling loop is owned by this feature.
            self.frame_requester.schedule_frame_in(
                deadline
                    .into_std()
                    .saturating_duration_since(Instant::now()),
            );
        }
    }

    fn launch_status_probes(&mut self, selections: &StatusSurfaceSelections) {
        let Some(runner) = self.workspace_command_runner.clone() else {
            return;
        };
        for kind in self.status_refresh.due(ProbeInstant::now()) {
            let Some(task_token) = self.status_refresh.pending(kind).cloned() else {
                continue;
            };
            let runner = runner.clone();
            let tx = self.app_event_tx.clone();
            let task = match kind {
                StatusProbeKind::LocalGit => {
                    let saved = self.status_worktree.saved.clone();
                    let session_cwd = self
                        .current_cwd
                        .clone()
                        .unwrap_or_else(|| self.config.cwd.to_path_buf());
                    let needs_changes = selections
                        .status_line_items
                        .contains(&StatusLineItem::BranchChanges);
                    let needs_identity = needs_changes
                        || selections.uses_git_branch()
                        || selections
                            .status_line_items
                            .contains(&StatusLineItem::PullRequestNumber);
                    tokio::spawn(async move {
                        let deadline = ProbeInstant::now() + branch_summary::LOCAL_REFRESH_BUDGET;
                        let validation = if task_token.environment_id
                            != codex_exec_server::LOCAL_ENVIRONMENT_ID
                            || saved.as_ref().is_some_and(|selection| {
                                selection.environment_id != task_token.environment_id
                            }) {
                            Err(StatusWorktreeError::Unavailable(
                                "workspace environment is unavailable".into(),
                            ))
                        } else if let Some(selection) = &saved {
                            tokio::time::timeout_at(
                                deadline,
                                crate::status_line_worktree::validate_saved_selection(
                                    runner.as_ref(),
                                    selection,
                                    &session_cwd,
                                ),
                            )
                            .await
                            .unwrap_or_else(|_| {
                                Err(StatusWorktreeError::Unavailable(
                                    "validation timed out".into(),
                                ))
                            })
                        } else {
                            Ok(())
                        };
                        let cwd = match (&validation, &saved) {
                            (Ok(()), Some(saved)) => PathBuf::from(saved.root.as_str()),
                            (
                                Err(
                                    StatusWorktreeError::InvalidCandidate(_)
                                    | StatusWorktreeError::DifferentRepository,
                                ),
                                _,
                            ) => session_cwd,
                            _ => task_token.cwd.clone(),
                        };
                        let snapshot = if needs_identity
                            && !matches!(validation, Err(StatusWorktreeError::Unavailable(_)))
                        {
                            tokio::time::timeout_at(
                                deadline,
                                branch_summary::local_git_snapshot(
                                    runner.as_ref(),
                                    &cwd,
                                    needs_changes,
                                ),
                            )
                            .await
                            .ok()
                            .flatten()
                        } else {
                            None
                        };
                        tx.send(AppEvent::StatusLineLocalUpdated {
                            token: task_token,
                            validation,
                            cwd,
                            snapshot,
                        });
                    })
                }
                StatusProbeKind::PullRequest => tokio::spawn(async move {
                    let result = tokio::time::timeout(Duration::from_secs(15), async {
                        let identity =
                            branch_summary::checkout_identity(runner.as_ref(), &task_token.cwd)
                                .await?;
                        let pr =
                            branch_summary::open_pull_request(runner.as_ref(), &task_token.cwd)
                                .await;
                        (branch_summary::checkout_identity(runner.as_ref(), &task_token.cwd)
                            .await?
                            == identity)
                            .then_some((identity, pr))
                    })
                    .await
                    .ok()
                    .flatten();
                    let (identity, pull_request) =
                        result.map_or((None, None), |(identity, pr)| (Some(identity), pr));
                    tx.send(AppEvent::StatusLinePullRequestUpdated {
                        token: task_token,
                        identity,
                        pull_request,
                    });
                }),
            };
            self.status_refresh.attach_task(kind, task);
        }
        self.schedule_status_refresh();
    }

    fn current_status_probe(&self, token: &StatusProbeToken) -> bool {
        self.status_worktree.live
            && self.thread_id == Some(token.thread_id)
            && self.status_worktree.environment_id == token.environment_id
            && self.status_worktree.generation == token.generation
            && self.status_context_cwd() == token.cwd
    }

    pub(crate) fn apply_status_local_result(
        &mut self,
        token: StatusProbeToken,
        validation: Result<(), StatusWorktreeError>,
        cwd: PathBuf,
        snapshot: Option<StatusLineLocalGit>,
    ) {
        let dirty = self.status_refresh.has_dirty(StatusProbeKind::LocalGit);
        if !self.current_status_probe(&token) || !self.status_refresh.accept(&token) {
            return;
        }
        let changed_context = self.update_status_worktree_validation(validation);
        let selections = self.status_surface_selections();
        if changed_context {
            self.configure_status_refresh(&selections, ProbeInstant::now());
            if !dirty {
                self.status_refresh.seed_local(ProbeInstant::now());
            }
        }
        let snapshot = snapshot
            .filter(|_| self.status_worktree_git_available() && self.status_context_cwd() == cwd);
        let identity = snapshot.as_ref().map(|snapshot| snapshot.identity.clone());
        if identity != self.status_local_identity {
            self.status_refresh
                .invalidate_pull_request_identity(ProbeInstant::now());
            if let Some(summary) = &mut self.status_line_git_summary {
                summary.pull_request = None;
            }
        }
        self.status_local_identity = identity;
        self.status_line_branch = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.identity.branch.clone());
        self.status_line_git_summary
            .get_or_insert_with(Default::default)
            .branch_change_stats = snapshot.and_then(|snapshot| snapshot.branch_change_stats);
        self.refresh_status_surfaces();
        self.request_redraw();
    }

    pub(crate) fn apply_status_pr_result(
        &mut self,
        token: StatusProbeToken,
        identity: Option<GitCheckoutIdentity>,
        pull_request: Option<StatusLinePullRequest>,
    ) {
        if !self.current_status_probe(&token) || !self.status_refresh.accept(&token) {
            return;
        }
        // A path can stay selected across external checkout/commit. Request identity alone cannot
        // prove that a slow GitHub response describes the branch displayed by a newer local probe.
        let matches_checkout = identity.is_some() && identity == self.status_local_identity;
        self.status_line_git_summary
            .get_or_insert_with(Default::default)
            .pull_request = if matches_checkout { pull_request } else { None };
        if identity.is_some() && !matches_checkout {
            self.status_refresh.invalidate(ProbeInstant::now());
        }
        self.refresh_status_surfaces();
        self.request_redraw();
    }
}
