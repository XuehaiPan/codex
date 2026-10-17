//! Reporting context is independent of execution cwd. A missing saved path is retained while
//! the footer falls back; transient failures hide Git values without claiming that path vanished.
//! Revalidation uses the normal refresh flow, never filesystem watches or checkout relocation.

use super::*;
use crate::status_line_worktree::StatusWorktreeError;
use codex_protocol::StatusLineWorktree;
use uuid::Uuid;

pub(super) struct StatusWorktreeState {
    pub(super) saved: Option<StatusLineWorktree>,
    pub(super) environment_id: String,
    pub(super) generation: u64,
    pub(super) view_id: Uuid,
    pub(super) fallback: bool,
    pub(super) available: bool,
    pub(super) warned: bool,
    pub(super) live: bool,
}

impl Default for StatusWorktreeState {
    fn default() -> Self {
        Self {
            saved: None,
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            generation: 0,
            view_id: Uuid::new_v4(),
            fallback: false,
            available: true,
            warned: false,
            live: true,
        }
    }
}

impl ChatWidget {
    pub(super) fn status_worktree_git_available(&self) -> bool {
        self.status_worktree.live
            && self.status_worktree.available
            && self.status_worktree.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID
    }

    /// Directory for reporting only. `/pwd`, tool defaults, and permissions use execution cwd.
    pub(crate) fn status_context_cwd(&self) -> &Path {
        if !self.status_worktree.fallback
            && let Some(saved) = &self.status_worktree.saved
        {
            return Path::new(saved.root.as_str());
        }
        self.current_cwd
            .as_deref()
            .unwrap_or(self.config.cwd.as_path())
    }

    pub(crate) fn accepts_status_worktree_candidate(
        &self,
        thread_id: ThreadId,
        view_id: Uuid,
    ) -> bool {
        self.thread_id == Some(thread_id)
            && self.status_worktree.view_id == view_id
            && self.status_worktree.saved.is_none()
            && self.status_worktree.live
    }

    /// Only callers handling live foreground command notifications may infer a candidate.
    /// Creation, command text (`git -C`), replay, and subagent activity are not intent signals.
    pub(crate) fn observe_status_worktree_candidate(&mut self, cwd: PathBuf) {
        if self.status_worktree.live
            && self.status_worktree.saved.is_none()
            && let Some(thread_id) = self.thread_id
            && cwd != self.status_context_cwd()
        {
            self.app_event_tx.send(AppEvent::StatusWorktreeCandidate {
                thread_id,
                cwd,
                view_id: self.status_worktree.view_id,
            });
        }
    }

    pub(crate) fn select_status_worktree(&mut self, selection: StatusLineWorktree) {
        self.status_worktree.saved = Some(selection);
        self.status_worktree.fallback = false;
        self.status_worktree.available = true;
        self.status_worktree.warned = false;
        self.invalidate_status_worktree_context();
        self.refresh_status_surfaces();
    }

    /// Restore metadata before starting probes. Cached branch/PR data is never restored with it.
    pub(crate) fn restore_status_worktree(&mut self, selection: Option<StatusLineWorktree>) {
        self.status_worktree.saved = selection;
        self.status_worktree.fallback = false;
        self.status_worktree.available = self.status_worktree.saved.is_none();
        self.status_worktree.warned = false;
        self.invalidate_status_worktree_context();
    }

    pub(super) fn invalidate_status_worktree_context(&mut self) {
        self.status_refresh.cancel();
        self.status_local_identity = None;
        self.status_worktree.generation = self.status_worktree.generation.wrapping_add(1);
        self.status_worktree.view_id = Uuid::new_v4();
        self.status_line_branch = None;
        self.status_line_git_summary = None;
        self.status_line_project_root_name_cache = None;
    }

    pub(crate) fn set_status_worktree_live(&mut self, live: bool) {
        self.status_worktree.live = live;
        self.invalidate_status_worktree_context();
    }

    pub(super) fn update_status_worktree_validation(
        &mut self,
        result: Result<(), StatusWorktreeError>,
    ) -> bool {
        let previous = (
            self.status_worktree.fallback,
            self.status_worktree.available,
        );
        match result {
            Ok(()) => {
                self.status_worktree.fallback = false;
                self.status_worktree.available = true;
                self.status_worktree.warned = false;
            }
            Err(StatusWorktreeError::Unavailable(_)) => {
                // A timeout, disconnect, or permission error is not evidence of removal.
                self.status_worktree.available = false;
            }
            Err(error) => {
                self.status_worktree.fallback = true;
                self.status_worktree.available = true;
                if !self.status_worktree.warned
                    && let Some(selection) = &self.status_worktree.saved
                {
                    let message = format!(
                        "Cannot use status worktree `{}`: {error}. Showing session directory `{}`.",
                        selection.root.as_str(),
                        self.status_context_cwd().display(),
                    );
                    self.status_worktree.warned = true;
                    self.on_warning(message);
                }
            }
        }
        let changed_context = previous.0 != self.status_worktree.fallback;
        if changed_context {
            self.invalidate_status_worktree_context();
        } else if previous.1 != self.status_worktree.available {
            // Losing access is not a new selection. Retire obsolete PR results and hide cached
            // metadata while retaining the same-context minimum request spacing across recovery.
            self.status_line_branch = None;
            self.status_line_git_summary = None;
            self.status_local_identity = None;
            self.status_refresh
                .invalidate_pull_request_identity(tokio::time::Instant::now());
        }
        changed_context
    }

    /// Call only after an existing removal operation succeeds, never when it starts or fails.
    pub(crate) fn status_worktree_removed(&mut self, root: &Path) {
        if self.status_worktree.live
            && self
                .status_worktree
                .saved
                .as_ref()
                .is_some_and(|saved| Path::new(saved.root.as_str()) == root)
        {
            self.invalidate_status_worktree_context();
            self.update_status_worktree_validation(Err(StatusWorktreeError::InvalidCandidate(
                "the worktree was removed".into(),
            )));
            self.refresh_status_surfaces();
            self.request_redraw();
        }
    }
}
