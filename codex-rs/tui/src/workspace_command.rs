//! App-server-backed workspace command execution for TUI-owned background lookups.
//!
//! This module is the TUI boundary for non-interactive commands that need to run wherever
//! the active workspace lives. Callers describe a command in terms of argv, cwd, environment
//! overrides and timeout; the runner translates that request to app-server
//! `command/exec`. Keeping this as a TUI-local abstraction lets status surfaces avoid knowing
//! whether the current app-server is embedded or remote.
//!
//! Commands sent through this path should not prompt for stdin. Most callers should keep output
//! bounded so metadata refreshes cannot grow into unbounded background processes; callers that own a
//! full user-visible payload, such as `/diff`, can explicitly opt out of output capping.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecParams;
use codex_app_server_protocol::CommandExecResponse;
use codex_app_server_protocol::CommandExecTerminateParams;
use codex_app_server_protocol::CommandExecTerminateResponse;
use codex_app_server_protocol::RequestId;
use uuid::Uuid;

/// Shared handle for running workspace commands from TUI components.
pub(crate) type WorkspaceCommandRunner = Arc<dyn WorkspaceCommandExecutor>;

/// Describes a bounded non-interactive command to execute in the active workspace.
///
/// The command is intentionally argv-based rather than shell-based so callers do not need to quote
/// user or repository data. `cwd` is interpreted by app-server relative to the workspace rules for
/// the active session, which is what makes the same request shape work for embedded and remote
/// app-server instances.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceCommand {
    /// Program and arguments to execute without shell interpolation.
    pub(crate) argv: Vec<String>,
    /// Explicit working directory on the host that executes the command.
    pub(crate) cwd: PathBuf,
    /// Environment overrides where `None` removes a variable.
    pub(crate) env: HashMap<String, Option<String>>,
    /// Maximum wall-clock duration before app-server cancels the command.
    pub(crate) timeout: Duration,
    /// Explicit stdout/stderr byte limit, or the server default when omitted.
    pub(crate) output_bytes_cap: Option<usize>,
    /// Whether app-server should return uncapped stdout/stderr.
    pub(crate) disable_output_cap: bool,
}

impl WorkspaceCommand {
    /// Creates a workspace command with conservative defaults for metadata probes.
    pub(crate) fn new(
        argv: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            env: HashMap::new(),
            timeout: Duration::from_secs(/*secs*/ 5),
            output_bytes_cap: None,
            disable_output_cap: false,
        }
    }

    /// Adds or replaces one environment variable override.
    pub(crate) fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), Some(value.into()));
        self
    }

    /// Sets the maximum wall-clock duration before app-server cancels the command.
    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Requests uncapped stdout/stderr capture from app-server.
    pub(crate) fn disable_output_cap(mut self) -> Self {
        self.disable_output_cap = true;
        self
    }
}

/// Captured result from a completed workspace command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceCommandOutput {
    /// Process exit status code reported by app-server.
    pub(crate) exit_code: i32,
    /// Captured stdout after app-server output capping.
    pub(crate) stdout: String,
    /// Captured stderr after app-server output capping.
    pub(crate) stderr: String,
}

impl WorkspaceCommandOutput {
    /// Returns whether the process exited successfully.
    pub(crate) fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Transport or protocol failure before a command result was available.
///
/// Non-zero process exits are represented as `WorkspaceCommandOutput` so callers can distinguish
/// a normal probe miss from an app-server request failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceCommandError {
    message: String,
}

impl WorkspaceCommandError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WorkspaceCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkspaceCommandError {}

/// Executes non-interactive workspace commands through the active TUI app-server session.
///
/// Implementations decide where the workspace lives. Callers provide argv/cwd/env and should not
/// branch on local versus remote execution.
pub(crate) trait WorkspaceCommandExecutor: Send + Sync {
    /// Optional connection namespace for cached remote metadata. Never shared across hosts.
    fn cache_namespace(&self) -> Option<Uuid> {
        None
    }

    /// Runs a workspace command and returns captured output or an app-server request error.
    ///
    /// Callers should treat errors as infrastructure failures and should treat successful output
    /// with a non-zero exit code as ordinary command failure. Returning a boxed future keeps the
    /// trait object-safe.
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> Pin<
        Box<dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>> + Send + '_>,
    >;
}

/// Workspace command runner that forwards every request to the active app-server.
#[derive(Clone)]
pub(crate) struct AppServerWorkspaceCommandRunner {
    request_handle: AppServerRequestHandle,
    cache_namespace: Uuid,
}

impl AppServerWorkspaceCommandRunner {
    /// Creates a runner from an app-server request handle owned by the current TUI session.
    pub(crate) fn new(request_handle: AppServerRequestHandle) -> Self {
        Self {
            request_handle,
            cache_namespace: Uuid::new_v4(),
        }
    }
}

/// Dropping a probe also requests termination on its workspace host. A transport disconnect or
/// unsupported termination (notably the Windows restricted-token backend) still has the command's
/// server-side timeout as a bound. Cleanup is a single best-effort request, never a retry loop.
struct WorkspaceCommandCancellation {
    request_handle: AppServerRequestHandle,
    process_id: Option<String>,
}

impl Drop for WorkspaceCommandCancellation {
    fn drop(&mut self) {
        let Some(process_id) = self.process_id.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let handle = self.request_handle.clone();
        runtime.spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                handle.request_typed::<CommandExecTerminateResponse>(
                    ClientRequest::CommandExecTerminate {
                        request_id: RequestId::String(format!(
                            "workspace-cancel-{}",
                            Uuid::new_v4()
                        )),
                        params: CommandExecTerminateParams { process_id },
                    },
                ),
            )
            .await;
        });
    }
}

impl WorkspaceCommandExecutor for AppServerWorkspaceCommandRunner {
    fn cache_namespace(&self) -> Option<Uuid> {
        Some(self.cache_namespace)
    }

    /// Sends the command as a one-off app-server `command/exec` request.
    ///
    /// The request is non-tty, does not stream stdin/stdout/stderr, and uses the caller's timeout.
    /// Bounded commands use the explicit byte cap when supplied, otherwise the server default. It leaves sandbox and permission
    /// profile selection to app-server so the same runner follows the active session's embedded or
    /// remote execution policy.
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> Pin<
        Box<dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>> + Send + '_>,
    > {
        Box::pin(async move {
            let timeout_ms = i64::try_from(command.timeout.as_millis()).unwrap_or(i64::MAX);
            let env = if command.env.is_empty() {
                None
            } else {
                Some(command.env)
            };
            let process_id = format!("workspace-process-{}", Uuid::new_v4());
            let mut cancellation = WorkspaceCommandCancellation {
                request_handle: self.request_handle.clone(),
                process_id: Some(process_id.clone()),
            };
            let response: CommandExecResponse = self
                .request_handle
                .request_typed(ClientRequest::OneOffCommandExec {
                    request_id: RequestId::String(format!("workspace-command-{}", Uuid::new_v4())),
                    params: CommandExecParams {
                        command: command.argv,
                        process_id: Some(process_id),
                        tty: false,
                        stream_stdin: false,
                        stream_stdout_stderr: false,
                        output_bytes_cap: command.output_bytes_cap,
                        disable_output_cap: command.disable_output_cap,
                        disable_timeout: false,
                        timeout_ms: Some(timeout_ms),
                        cwd: Some(command.cwd),
                        env,
                        size: None,
                        sandbox_policy: None,
                        permission_profile: None,
                    },
                })
                .await
                .map_err(|err| WorkspaceCommandError::new(err.to_string()))?;

            cancellation.process_id = None;
            Ok(WorkspaceCommandOutput {
                exit_code: response.exit_code,
                stdout: response.stdout,
                stderr: response.stderr,
            })
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use color_eyre::eyre::WrapErr;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn status_refresh_cancel_terminates_workspace_process() -> color_eyre::Result<()> {
        crate::test_support::boxed_future!(cancel_workspace_process()).await
    }

    async fn cancel_workspace_process() -> color_eyre::Result<()> {
        let home = tempfile::tempdir()?;
        let config = crate::legacy_core::config::ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .harness_overrides(crate::legacy_core::config::ConfigOverrides {
                cwd: Some(home.path().to_path_buf()),
                permission_profile: Some(codex_protocol::models::PermissionProfile::Disabled),
                ..Default::default()
            })
            .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await?;
        let server = crate::start_embedded_app_server_for_picker(&config).await?;
        let runner = AppServerWorkspaceCommandRunner::new(server.request_handle());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let script = format!(
            "import socket; s=socket.create_connection(('127.0.0.1', {})); s.sendall(b'R'); s.recv(1)",
            listener.local_addr()?.port()
        );
        let cwd = home.path().to_path_buf();
        let task = tokio::spawn(async move {
            runner
                .run(
                    WorkspaceCommand::new(["python3", "-c", &script], cwd)
                        .timeout(Duration::from_secs(30)),
                )
                .await
        });
        let (mut socket, _) =
            tokio::time::timeout(Duration::from_secs(10), listener.accept()).await??;
        let mut ready = [0_u8];
        socket.read_exact(&mut ready).await?;
        assert_eq!(ready, [b'R']);
        // The socket handshake is the readiness barrier. EOF must follow cancellation, well before
        // the command's thirty-second server deadline; no timing sleeps or filesystem polling.
        task.abort();
        assert!(task.await.is_err_and(|error| error.is_cancelled()));
        let result = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut ready)).await;
        // Release the fixture even when the regression fails, avoiding an abandoned child process.
        if result.is_err() {
            let _ = socket.write_all(b"x").await;
        }
        assert_eq!(
            result.wrap_err("cancelling the probe must terminate its workspace process")??,
            0
        );
        Ok(())
    }
}
