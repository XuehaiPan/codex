//! Reporting-only checkout selection. All path inspection runs on the workspace host.
//!
//! Intent is reserved before validation. Per-thread write gates serialize metadata updates;
//! checking intent identity before writing prevents older automatic candidates from replacing
//! an explicit choice. UI events follow the last successful selection instead: a later failed
//! intent must not discard an already committed update. This service does not manage checkouts.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::workspace_command::AppServerWorkspaceCommandRunner;
use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandExecutor;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolFunctionSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadMetadataUpdateParams;
use codex_app_server_protocol::ThreadMetadataUpdateResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_protocol::StatusLineWorktree;
use codex_protocol::ThreadId;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use uuid::Uuid;

pub(crate) const TOOL_NAME: &str = "set_status_line_worktree";
pub(crate) const GUIDANCE: &str = "When you start implementing in a linked Git worktree, call set_status_line_worktree with its absolute workspace-host path. This updates reporting only; command cwd and permissions stay unchanged. Creation or attachment alone does not select it. Select explicitly for git -C or absolute-path edits and when intentionally switching checkouts. A result with persisted: false lasts only for this connection.";

pub(crate) fn tool_spec() -> DynamicToolFunctionSpec {
    DynamicToolFunctionSpec {
        name: TOOL_NAME.into(),
        description: GUIDANCE.into(),
        input_schema: json!({"type":"object", "additionalProperties":false,
            "properties":{"path":{"type":"string","minLength":1,"maxLength":4096}}, "required":["path"]}),
        defer_loading: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StatusWorktreeError {
    #[error("{0}")]
    InvalidCandidate(String),
    #[error("the checkout belongs to a different repository")]
    DifferentRepository,
    #[error("{0}")]
    Unavailable(String),
}

/// Resolve both paths on `runner`'s host; callers must bind it to `environment_id` first.
/// Return the selection and source checkout root so automatic adoption can exclude the session
/// checkout. Git canonicalizes nested and symlinked paths; registration alone never proves availability.
async fn resolve_status_line_worktree(
    runner: &dyn WorkspaceCommandExecutor,
    environment_id: &str,
    session_cwd: &Path,
    candidate: &Path,
) -> Result<(StatusLineWorktree, LegacyAppPathString), StatusWorktreeError> {
    if environment_id.is_empty() {
        return Err(StatusWorktreeError::Unavailable(
            "workspace environment is unavailable".into(),
        ));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        let (source, target) =
            tokio::try_join!(checkout(runner, session_cwd), checkout(runner, candidate))?;
        if source.git_common_dir != target.git_common_dir {
            return Err(StatusWorktreeError::DifferentRepository);
        }
        Ok((
            StatusLineWorktree {
                environment_id: environment_id.into(),
                ..target
            },
            source.root,
        ))
    })
    .await
    .map_err(|_| StatusWorktreeError::Unavailable("checkout validation timed out".into()))?
}

/// Revalidate a saved path against its repository even if the session checkout has disappeared.
pub(crate) async fn validate_saved_selection(
    runner: &dyn WorkspaceCommandExecutor,
    selection: &StatusLineWorktree,
    session_cwd: &Path,
) -> Result<(), StatusWorktreeError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (found, session) = tokio::join!(
            checkout(runner, Path::new(selection.root.as_str())),
            checkout(runner, session_cwd),
        );
        let found = found?;
        if found.git_common_dir != selection.git_common_dir || found.root != selection.root {
            return Err(StatusWorktreeError::DifferentRepository);
        }
        match session {
            Ok(session) if session.git_common_dir != selection.git_common_dir => {
                Err(StatusWorktreeError::DifferentRepository)
            }
            // A missing original checkout does not make a valid selected worktree unusable.
            Err(StatusWorktreeError::InvalidCandidate(_)) | Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    })
    .await
    .map_err(|_| StatusWorktreeError::Unavailable("checkout validation timed out".into()))?
}

async fn checkout(
    runner: &dyn WorkspaceCommandExecutor,
    path: &Path,
) -> Result<StatusLineWorktree, StatusWorktreeError> {
    let path = LegacyAppPathString::from_path(path);
    let Some(mut anchor) = path
        .to_inferred_path_uri()
        .filter(|_| !path.as_str().contains('\0'))
    else {
        return Err(StatusWorktreeError::InvalidCandidate(
            "use an absolute workspace-host path".into(),
        ));
    };
    // Use the host path's filesystem root as the process cwd. The session directory may
    // itself have vanished; Git must still run so `-C` can positively diagnose a missing path.
    while let Some(parent) = anchor.parent() {
        anchor = parent;
    }
    let query = |flag: &'static str| async {
        let mut command = WorkspaceCommand::new(
            [
                "git",
                "-C",
                path.as_str(),
                "rev-parse",
                "--path-format=absolute",
                flag,
            ],
            anchor.to_path_buf(),
        )
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            command.env.insert(variable.into(), None);
        }
        let output = runner
            .run(command)
            .await
            .map_err(|error| StatusWorktreeError::Unavailable(error.to_string()))?;
        if !output.success() {
            // Only affirmative absence/non-repository errors cause lifecycle fallback. A lost
            // connection, permission denial, lock, or unknown Git error is a retryable failure.
            let reason = output.stderr.trim().chars().take(512).collect::<String>();
            return Err(
                if reason.contains("No such file or directory")
                    || reason.contains("not a git repository")
                    || reason.contains("must be run in a work tree")
                    || reason.contains("Not a directory")
                {
                    StatusWorktreeError::InvalidCandidate(reason)
                } else {
                    StatusWorktreeError::Unavailable(reason)
                },
            );
        }
        let value = output.stdout.strip_suffix('\n').unwrap_or(&output.stdout);
        let value = value.strip_suffix('\r').unwrap_or(value);
        let result = LegacyAppPathString::from_string(value);
        if value.len() >= 16_384 || result.to_inferred_path_uri().is_none() {
            return Err(StatusWorktreeError::Unavailable(
                "Git returned an incomplete checkout path".into(),
            ));
        }
        Ok(result)
    };
    let (root, git_common_dir) =
        tokio::try_join!(query("--show-toplevel"), query("--git-common-dir"))?;
    Ok(StatusLineWorktree {
        environment_id: String::new(),
        root,
        git_common_dir,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusSelectionToken {
    pub(crate) thread_id: ThreadId,
    connection: Uuid,
    generation: u64,
}

#[derive(Default)]
struct SelectionEntry {
    generation: u64,
    committed_generation: Option<u64>,
    pending: bool,
    automatic_pending: bool,
    automatic_candidates: VecDeque<String>,
    selected: Option<StatusLineWorktree>,
    transient: bool,
    writes: Arc<tokio::sync::Mutex<()>>,
}

struct Registry {
    connection: Uuid,
    next_generation: u64,
    threads: HashMap<ThreadId, SelectionEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            connection: Uuid::new_v4(),
            next_generation: 0,
            threads: HashMap::new(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct StatusLineWorktreeTools {
    registry: Arc<Mutex<Registry>>,
}

impl StatusLineWorktreeTools {
    pub(crate) fn invalidate_connection(&self) {
        *self.registry.lock().unwrap_or_else(PoisonError::into_inner) = Registry::default();
    }

    /// Unsupported servers retain reporting metadata only until this connection is replaced.
    pub(crate) fn transient_selection(&self, thread_id: ThreadId) -> Option<StatusLineWorktree> {
        self.registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .threads
            .get(&thread_id)
            .filter(|entry| entry.transient)
            .and_then(|entry| entry.selected.clone())
    }

    /// Retire pending selection intent when its foreground session is replaced.
    /// Keep completed in-memory selections for unsupported servers on this connection.
    pub(crate) fn cancel_pending(&self, thread_id: ThreadId) {
        let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        state.next_generation += 1;
        let generation = state.next_generation;
        if let Some(entry) = state.threads.get_mut(&thread_id)
            && entry.pending
        {
            entry.generation = generation;
            entry.pending = false;
            entry.automatic_candidates.clear();
        }
    }

    pub(crate) fn forget_thread(&self, thread_id: ThreadId) {
        self.registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .threads
            .remove(&thread_id);
    }

    pub(crate) fn accepts(&self, token: &StatusSelectionToken) -> bool {
        let state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        state.connection == token.connection
            && state
                .threads
                .get(&token.thread_id)
                .is_some_and(|entry| entry.generation == token.generation)
    }

    /// A queued UI update remains valid while a newer intent is pending or fails validation.
    /// Only a successful replacement, thread removal, or connection change retires it.
    pub(crate) fn accepts_selection(&self, token: &StatusSelectionToken) -> bool {
        let state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        state.connection == token.connection
            && state
                .threads
                .get(&token.thread_id)
                .is_some_and(|entry| entry.committed_generation == Some(token.generation))
    }

    /// Explicit intent replaces pending work. Automatic candidates share one reservation and
    /// queue in launch order, including the first path, until one succeeds or intent changes.
    fn reserve(
        &self,
        thread_id: ThreadId,
        automatic_candidate: Option<String>,
    ) -> Option<StatusSelectionToken> {
        let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let automatic = automatic_candidate.is_some();
        let entry = state.threads.entry(thread_id).or_default();
        if let Some(path) = automatic_candidate {
            if entry.selected.is_some() || (entry.pending && !entry.automatic_pending) {
                return None;
            }
            entry.automatic_candidates.push_back(path);
            if entry.pending {
                return None;
            }
        } else {
            entry.automatic_candidates.clear();
        }
        state.next_generation += 1;
        let token = StatusSelectionToken {
            thread_id,
            connection: state.connection,
            generation: state.next_generation,
        };
        let entry = state.threads.entry(thread_id).or_default();
        entry.generation = token.generation;
        entry.pending = true;
        entry.automatic_pending = automatic;
        Some(token)
    }

    fn seed(&self, token: &StatusSelectionToken, saved: Option<StatusLineWorktree>) {
        let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if state.connection == token.connection
            && let Some(entry) = state.threads.get_mut(&token.thread_id)
            && entry.generation == token.generation
            && entry.selected.is_none()
        {
            entry.selected = saved;
        }
    }

    fn finish(&self, token: &StatusSelectionToken) {
        let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if state.connection == token.connection
            && let Some(entry) = state.threads.get_mut(&token.thread_id)
            && entry.generation == token.generation
        {
            entry.pending = false;
            entry.automatic_candidates.clear();
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "This per-thread gate must serialize awaited metadata writes, including stale-write compensation."
    )]
    async fn publish<F, Fut>(
        &self,
        token: &StatusSelectionToken,
        selection: StatusLineWorktree,
        persist: F,
    ) -> Result<bool, String>
    where
        F: Fn(Option<StatusLineWorktree>) -> Fut,
        Fut: Future<Output = Result<bool, String>>,
    {
        let (writes, previous) = {
            let state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = state
                .threads
                .get(&token.thread_id)
                .ok_or("selection superseded")?;
            (entry.writes.clone(), entry.selected.clone())
        };
        let _write = writes.lock().await;
        if !self.accepts(token) {
            return Err("selection superseded".into());
        }
        let persisted = persist(Some(selection.clone())).await?;
        let (accepted, same_connection) = {
            let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
            let same_connection = state.connection == token.connection;
            let entry = state.threads.get_mut(&token.thread_id);
            let accepted = same_connection
                && entry
                    .as_ref()
                    .is_some_and(|entry| entry.generation == token.generation);
            let still_owned = same_connection && entry.is_some();
            if accepted && let Some(entry) = entry {
                entry.selected = Some(selection);
                entry.committed_generation = Some(token.generation);
                entry.transient = !persisted;
            }
            (accepted, still_owned)
        };
        if !accepted {
            // A newer intent can arrive while a write is on the wire. Compensate before
            // releasing the gate, so even a rejected candidate preserves the prior value.
            // Never start a compensating write for a deleted thread or obsolete connection.
            if persisted && same_connection {
                persist(previous).await?;
            }
            return Err("selection superseded".into());
        }
        Ok(persisted)
    }

    pub(crate) async fn execute(
        &self,
        handle: AppServerRequestHandle,
        thread_id: String,
        arguments: Value,
        events: &AppEventSender,
    ) -> DynamicToolCallResponse {
        let result = self
            .execute_inner(handle, thread_id, arguments, events)
            .await;
        DynamicToolCallResponse {
            success: result.is_ok(),
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: match result {
                    Ok(value) => value.to_string(),
                    Err(error) => json!({"error":error}).to_string(),
                },
            }],
        }
    }

    async fn execute_inner(
        &self,
        handle: AppServerRequestHandle,
        thread_id: String,
        arguments: Value,
        events: &AppEventSender,
    ) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            path: String,
        }
        let args: Arguments =
            serde_json::from_value(arguments).map_err(|error| error.to_string())?;
        if args.path.len() > 4096 {
            return Err("worktree path is too long".into());
        }
        let id = ThreadId::from_string(&thread_id).map_err(|error| error.to_string())?;
        let token = self
            .reserve(id, None)
            .ok_or("a reporting checkout is already selected")?;
        let result = self
            .execute_reserved(handle, token.clone(), args.path, true, events)
            .await;
        self.finish(&token);
        result
    }

    fn next_candidate(&self, token: &StatusSelectionToken) -> Option<String> {
        let mut state = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if state.connection != token.connection {
            return None;
        }
        let entry = state.threads.get_mut(&token.thread_id)?;
        if entry.generation != token.generation || !entry.automatic_pending {
            return None;
        }
        if entry.selected.is_none()
            && let Some(path) = entry.automatic_candidates.pop_front()
        {
            return Some(path);
        }
        entry.pending = false;
        entry.automatic_candidates.clear();
        None
    }

    pub(crate) fn observe_candidate(
        &self,
        handle: AppServerRequestHandle,
        thread_id: ThreadId,
        path: String,
        events: AppEventSender,
    ) {
        let Some(token) = self.reserve(thread_id, Some(path)) else {
            return;
        };
        let service = self.clone();
        tokio::spawn(async move {
            while let Some(path) = service.next_candidate(&token) {
                let _ = service
                    .execute_reserved(handle.clone(), token.clone(), path, false, &events)
                    .await;
            }
        });
    }

    async fn execute_reserved(
        &self,
        handle: AppServerRequestHandle,
        token: StatusSelectionToken,
        path: String,
        explicit: bool,
        events: &AppEventSender,
    ) -> Result<Value, String> {
        let thread_id = token.thread_id.to_string();
        if !self.accepts(&token) {
            return Err("selection superseded".into());
        }
        let response: ThreadReadResponse = handle
            .request_typed(ClientRequest::ThreadRead {
                request_id: request_id(),
                params: ThreadReadParams {
                    thread_id: thread_id.clone(),
                    include_turns: false,
                },
            })
            .await
            .map_err(|error| error.to_string())?;
        let thread = response.thread;
        let primary = thread
            .environments
            .as_ref()
            .and_then(|environments| environments.first())
            .ok_or("cannot verify the thread's workspace environment")?;
        // command/exec runs on this app-server's local workspace host (which may itself be
        // remote from the TUI). Never send a different executor's paths to that host.
        if primary.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID {
            return Err("reporting commands are unavailable for this workspace environment".into());
        }
        self.seed(&token, thread.status_line_worktree.clone());
        if !explicit && thread.status_line_worktree.is_some() {
            return Err("a reporting checkout is already selected".into());
        }
        let runner = AppServerWorkspaceCommandRunner::new(handle.clone());
        let (selection, source_root) = resolve_status_line_worktree(
            &runner,
            &primary.environment_id,
            Path::new(primary.cwd.as_str()),
            Path::new(&path),
        )
        .await
        .map_err(|error| error.to_string())?;
        if !explicit && selection.root == source_root {
            return Err("command is in the session checkout".into());
        }
        let persisted = self
            .publish(&token, selection.clone(), |value| {
                let handle = handle.clone();
                let thread_id = thread_id.clone();
                async move {
                    if thread.ephemeral {
                        return Ok(false);
                    }
                    persist_selection(&handle, thread_id, value).await
                }
            })
            .await?;
        if !self.accepts_selection(&token) {
            return Err("selection superseded".into());
        }
        events.send(AppEvent::StatusWorktreeSelected {
            token: token.clone(),
            selection: selection.clone(),
            persisted,
        });
        Ok(json!({"root":selection.root,"persisted":persisted}))
    }
}

async fn persist_selection(
    handle: &AppServerRequestHandle,
    thread_id: String,
    selection: Option<StatusLineWorktree>,
) -> Result<bool, String> {
    let result = handle
        .request_typed::<ThreadMetadataUpdateResponse>(ClientRequest::ThreadMetadataUpdate {
            request_id: request_id(),
            params: ThreadMetadataUpdateParams {
                thread_id,
                status_line_worktree: Some(selection.clone()),
                project_id: None,
                daybreak_enabled: None,
                git_info: None,
            },
        })
        .await;
    match result {
        Ok(response) if response.thread.status_line_worktree == selection => Ok(true),
        Ok(_) => Ok(false),
        Err(TypedRequestError::Server { source, .. })
            if source.code == -32601
                || ((source.code == -32600 || source.code == -32602)
                    && ((source.message.contains("statusLineWorktree")
                        && (source.message.contains("unknown field")
                            || source.message.contains("unrecognized field")))
                        || source.message
                            == "thread metadata update must include at least one field")) =>
        {
            Ok(false)
        }
        Err(error) => Err(format!("cannot persist status worktree: {error}")),
    }
}

fn request_id() -> RequestId {
    RequestId::String(format!("status-worktree-{}", Uuid::new_v4()))
}

#[cfg(test)]
#[path = "status_line_worktree_tests.rs"]
mod tests;
