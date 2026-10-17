//! Isolated local Git fixtures shared by reporting-context and branch-content tests.
use crate::workspace_command::*;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;

pub(crate) struct GitFixture {
    _home: tempfile::TempDir,
    pub(crate) root: PathBuf,
    pub(crate) worktree: PathBuf,
    pub(crate) runner: GitRunner,
}

#[derive(Clone)]
pub(crate) struct GitRunner {
    config: PathBuf,
}

impl GitFixture {
    pub(crate) fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let home_path = dunce::canonicalize(home.path()).unwrap();
        let root = home_path.join("repo");
        std::fs::create_dir(&root).unwrap();
        let config = home_path.join("empty-config");
        std::fs::write(&config, "").unwrap();
        std::fs::create_dir(home_path.join("empty-hooks")).unwrap();
        let fixture = Self {
            _home: home,
            worktree: home_path.join("feature checkout"),
            root,
            runner: GitRunner { config },
        };
        fixture.git(&["init", "--initial-branch=main"]);
        std::fs::write(fixture.root.join("file.txt"), "base\n").unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-m", "base"]);
        fixture.git(&[
            "worktree",
            "add",
            "-b",
            "feature",
            fixture.worktree.to_str().unwrap(),
        ]);
        fixture
    }

    /// Run in the linked checkout with the same isolated configuration as root commands.
    pub(crate) fn worktree_git(&self, args: &[&str]) {
        let mut command = vec!["-C", self.worktree.to_str().unwrap()];
        command.extend_from_slice(args);
        self.git(&command);
    }

    pub(crate) fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", &self.runner.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "init.templateDir=",
            ])
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                self._home.path().join("empty-hooks").display()
            ))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl WorkspaceCommandExecutor for GitRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> Pin<
        Box<dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>> + Send + '_>,
    > {
        Box::pin(async move {
            if command.argv.first().map(String::as_str) != Some("git") {
                return Err(WorkspaceCommandError::new(
                    "fixture only permits local Git commands",
                ));
            }
            let mut process = tokio::process::Command::new(&command.argv[0]);
            process
                .args(&command.argv[1..])
                .env("GIT_CONFIG_GLOBAL", &self.config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_INDEX_FILE")
                .kill_on_drop(true);
            process.current_dir(command.cwd);
            for (key, value) in command.env {
                if let Some(value) = value {
                    process.env(key, value);
                } else {
                    process.env_remove(key);
                }
            }
            let output = process
                .output()
                .await
                .map_err(|error| WorkspaceCommandError::new(error.to_string()))?;
            Ok(WorkspaceCommandOutput {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8(output.stdout).unwrap(),
                stderr: String::from_utf8(output.stderr).unwrap(),
            })
        })
    }
}
