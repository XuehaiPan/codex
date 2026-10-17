//! Production CLI coverage of reporting-only selection during an uninterrupted turn.
//! Git and GitHub state are disposable and isolated from personal configuration and credentials.

use super::focus_palette::PtyCodex;
use super::focus_palette::write_test_config;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

#[path = "status_worktree_busy.rs"]
mod busy;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    pr: PathBuf,
    env: Vec<(OsString, Option<OsString>)>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir_in("/tmp")?;
        let base = temp.path().canonicalize()?;
        let root = base.join("repo");
        let bin = base.join("bin");
        let hooks = base.join("hooks");
        fs::create_dir_all(&root)?;
        fs::create_dir(&bin)?;
        fs::create_dir(&hooks)?;
        let config = base.join("gitconfig");
        fs::write(
            &config,
            format!(
                "[user]\nname = Test\nemail = test@example.invalid\n[commit]\ngpgSign = false\n[init]\ntemplateDir =\n[core]\nhooksPath = {}\n",
                hooks.display()
            ),
        )?;
        let pr = base.join("pr.json");
        fs::write(
            &pr,
            r#"{"number":17,"url":"https://example.invalid/pr/17","state":"CLOSED"}"#,
        )?;
        let gh = bin.join("gh");
        fs::write(
            &gh,
            "#!/usr/bin/env python3\nimport os, sys\nif sys.argv[1:3] != ['pr', 'view']:\n    sys.exit(1)\nwith open(os.environ['STATUS_TEST_PR']) as source:\n    print(source.read())\n",
        )?;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))?;
        let mut env = vec![
            ("PATH".into(), Some(path)),
            ("GIT_CONFIG_GLOBAL".into(), Some(config.into_os_string())),
            ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
            ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
            ("STATUS_TEST_PR".into(), Some(pr.clone().into_os_string())),
        ];
        for key in [
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GH_TOKEN",
            "GITHUB_TOKEN",
        ] {
            env.push((key.into(), None));
        }
        let fixture = Self {
            _temp: temp,
            worktree: base.join("feature"),
            root,
            pr,
            env,
        };
        fixture.git(&fixture.root, &["init", "-q", "-b", "main"])?;
        fs::write(fixture.root.join("file.txt"), "base\n")?;
        fixture.git(&fixture.root, &["add", "."])?;
        fixture.git(&fixture.root, &["commit", "-qm", "base"])?;
        Ok(fixture)
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let mut command = Command::new("git");
        for (key, value) in &self.env {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        let output = command.arg("-C").arg(cwd).args(args).output()?;
        ensure!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn command(&self, id: &str, cmd: String, cwd: &Path) -> Value {
        responses::ev_function_call(
            id,
            "exec_command",
            &json!({"cmd": cmd, "workdir": cwd, "shell": "/bin/sh", "login": false}).to_string(),
        )
    }

    fn set_pr(&self, state: &str) -> Result<()> {
        fs::write(
            &self.pr,
            json!({"number": 17, "url": "https://example.invalid/pr/17", "state": state})
                .to_string(),
        )?;
        Ok(())
    }
}

fn shell_path(path: &Path) -> Result<String> {
    Ok(
        shlex::try_quote(path.to_str().context("UTF-8 fixture path")?)
            .context("shell-quotable fixture path")?
            .into_owned(),
    )
}

fn stream(event: Value, id: &str) -> Vec<StreamingSseChunk> {
    vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![event, responses::ev_completed(id)]),
    }]
}

fn gated(event: Value, id: &str) -> (oneshot::Sender<()>, Vec<StreamingSseChunk>) {
    let (tx, rx) = oneshot::channel();
    (
        tx,
        vec![StreamingSseChunk {
            gate: Some(rx),
            body: responses::sse(vec![event, responses::ev_completed(id)]),
        }],
    )
}

fn submit(terminal: &mut PtyCodex, text: &str) -> Result<()> {
    terminal.write_input(text.as_bytes())?;
    // Burst input is intentionally held by the composer before Enter becomes submission.
    let ready = Instant::now() + Duration::from_millis(250);
    while Instant::now() < ready {
        terminal.read_output(ready.saturating_duration_since(Instant::now()))?;
    }
    terminal.write_input(b"\r")
}

fn footer(terminal: &PtyCodex) -> String {
    terminal
        .screen_contents()
        .lines()
        .find(|line| line.contains("StatusTest") && line.contains(" · "))
        .unwrap_or_default()
        .to_string()
}

fn wait_footer(terminal: &mut PtyCodex, expected: &[&str], absent: &[&str]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        terminal.read_output(Duration::from_millis(50))?;
        terminal.ensure_running()?;
        let line = footer(terminal);
        if expected.iter().all(|text| line.contains(text))
            && absent.iter().all(|text| !line.contains(text))
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "footer expected {expected:?}, absent {absent:?}; screen:\n{}",
            terminal.screen_contents()
        );
    }
}

fn check_worktree_warning(terminal: &mut PtyCodex, fixture: &Fixture) -> Result<()> {
    terminal.write_input(b"\x1bOQ")?;
    terminal.wait_for_screen("Warnings ·")?;
    loop {
        let screen = terminal.screen_contents();
        if screen.contains("Cannot use status worktree") {
            ensure!(
                screen.contains(
                    fixture
                        .worktree
                        .to_str()
                        .context("UTF-8 worktree fixture path")?
                ),
                "missing selected path: {screen}"
            );
            ensure!(
                screen.contains("Showing session directory"),
                "missing fallback explanation: {screen}"
            );
            ensure!(
                screen.contains(
                    fixture
                        .root
                        .to_str()
                        .context("UTF-8 repository fixture path")?
                ),
                "missing fallback path: {screen}"
            );
            terminal.write_input(b"\x1bOQ")?;
            return Ok(());
        }
        let header = screen
            .lines()
            .find(|line| line.contains("Warnings ·"))
            .context("warning header")?;
        let page = header.split('·').nth(1).context("warning page")?;
        let values: Vec<_> = page.split_whitespace().collect();
        let current: usize = values[0].parse()?;
        let total: usize = values[2].parse()?;
        ensure!(current < total, "missing lifecycle warning: {screen}");
        terminal.write_input(b"\x1b[C")?;
        terminal.wait_for_screen(&format!("Warnings · {} of", current + 1))?;
    }
}

async fn request(server: &StreamingSseServer, terminal: &mut PtyCodex, count: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let received = server.requests().await.len();
        if received >= count {
            return Ok(());
        }
        // Keep consuming PTY output while the model barrier is closed. Otherwise animated
        // output can fill the terminal buffer and block the very command we are awaiting.
        terminal.read_output(Duration::from_millis(20))?;
        terminal.ensure_running()?;
        ensure!(
            Instant::now() < deadline,
            "model request barrier {count}, received {received}; screen:\n{}",
            terminal.screen_contents()
        );
    }
}

fn rollout_records(home: &Path) -> Result<Vec<Value>> {
    thread_rollout_records(home, None)
}

fn thread_rollout_records(home: &Path, thread_id: Option<&str>) -> Result<Vec<Value>> {
    fn visit(path: &Path, thread_id: Option<&str>, records: &mut Vec<Value>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(&path, thread_id, records)?;
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                // Ignore an unfinished final record while the recorder is appending it.
                let session: Vec<Value> = fs::read_to_string(path)?
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                if thread_id.is_none_or(|id| {
                    session.iter().any(|record| {
                        record["type"] == "session_meta" && record["payload"]["id"] == id
                    })
                }) {
                    records.extend(session);
                }
            }
        }
        Ok(())
    }
    let mut records = Vec::new();
    visit(&home.join("sessions"), thread_id, &mut records)?;
    Ok(records)
}

fn active_context(home: &Path) -> Result<(String, Value)> {
    active_thread_context(home, None)
}

fn active_thread_context(home: &Path, thread_id: Option<&str>) -> Result<(String, Value)> {
    let records = thread_rollout_records(home, thread_id)?;
    let sessions: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "session_meta")
        .collect();
    ensure!(
        sessions.len() == 1,
        "selection must retain the original thread"
    );
    let context = records
        .iter()
        .rev()
        .find(|record| record["type"] == "turn_context")
        .context("missing turn context")?["payload"]
        .clone();
    let turn_id = context["turn_id"].as_str().context("missing turn ID")?;
    ensure!(
        !records.iter().any(|record| record["type"] == "event_msg"
            && record["payload"]["type"] == "task_complete"
            && record["payload"]["turn_id"] == turn_id),
        "implementation turn unexpectedly completed"
    );
    Ok((
        sessions[0]["payload"]["id"]
            .as_str()
            .context("thread ID")?
            .to_string(),
        context,
    ))
}

fn start_fixture(fixture: &Fixture, server: &StreamingSseServer) -> Result<(PtyCodex, PathBuf)> {
    start_fixture_with_config(fixture, server, "")
}

fn start_fixture_with_config(
    fixture: &Fixture,
    server: &StreamingSseServer,
    extra_config: &str,
) -> Result<(PtyCodex, PathBuf)> {
    let home = tempfile::tempdir_in("/tmp")?;
    let home_path = home.path().canonicalize()?;
    write_test_config(home.path(), &fixture.root)?;
    let config_path = home.path().join("config.toml");
    let config = fs::read_to_string(&config_path)?
        .replace("model_provider = \"openai\"", "model_provider = \"test\"");
    fs::write(
        config_path,
        format!(
            "sandbox_mode = \"danger-full-access\"\napproval_policy = \"never\"\n{extra_config}\n{config}\n[tui]\nstatus_line = [\"current-dir\", \"git-branch\", \"branch-changes\", \"pull-request-number\", \"thread-name\"]\n[model_providers.test]\nname = \"Mock\"\nbase_url = \"{}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = false\n",
            server.uri()
        ),
    )?;
    let mut terminal =
        PtyCodex::start_cli_with_env(&fixture.root, home, &["--no-alt-screen"], &fixture.env)?;
    terminal.wait_for_startup()?;
    submit(&mut terminal, "/rename StatusTest")?;
    wait_footer(&mut terminal, &["repo", "main", "No changes"], &["PR #"])?;
    Ok((terminal, home_path))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_worktree_plan_to_implementation_without_interrupt() -> Result<()> {
    let fixture = Fixture::new()?;
    let worktree = shell_path(&fixture.worktree)?;
    let (select_tx, select) = gated(
        responses::ev_function_call_with_namespace(
            "select",
            "mcp__codex_worktrees",
            "set_status_line_worktree",
            &json!({"path":fixture.worktree}).to_string(),
        ),
        "selected",
    );
    let (edit_tx, edit) = gated(fixture.command("edit", format!("printf 'base\\nadded\\n' > {worktree}/file.txt; printf 'new\\n' > {worktree}/new.txt"), &fixture.root), "edited");
    let (stage_tx, stage) = gated(
        fixture.command("stage", format!("git -C {worktree} add ."), &fixture.root),
        "staged",
    );
    let (commit_tx, commit) = gated(
        fixture.command(
            "commit",
            format!("git -C {worktree} commit -qm feature"),
            &fixture.root,
        ),
        "committed",
    );
    let (inspect_tx, inspect) = gated(
        fixture.command("inspect", "git status --short".to_string(), &fixture.root),
        "inspected",
    );
    let (finish_tx, finish_rx) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        stream(responses::ev_assistant_message("plan", "<proposed_plan>\n# Worktree feature\nImplement in a linked worktree.\n</proposed_plan>"), "planned"),
        stream(fixture.command("create", format!("git worktree add -b feature {worktree}"), &fixture.root), "created"),
        select, edit, stage, commit, inspect,
        vec![StreamingSseChunk { gate: None, body: responses::sse(vec![responses::ev_response_created("held")]) }, StreamingSseChunk { gate: Some(finish_rx), body: responses::sse(vec![responses::ev_assistant_message("done", "WORKTREE_FLOW_DONE"), responses::ev_completed("held")]) }],
    ]).await;
    let (mut terminal, home_path) = start_fixture(&fixture, &server)?;
    submit(&mut terminal, "/plan Implement a feature in a worktree")?;
    terminal.wait_for_screen("Implement this plan?")?;
    terminal.write_input(b"\r")?;
    request(&server, &mut terminal, 3).await?;
    // Creation alone does not select, including shell-created worktrees. The remaining model
    // commands deliberately use the root cwd, exercising explicit selection for absolute paths.
    wait_footer(&mut terminal, &["repo", "main", "No changes"], &["feature"])?;
    let original = active_context(&home_path)?;
    assert_eq!(original.1["cwd"], json!(fixture.root));
    assert_eq!(original.1["approval_policy"], "never");
    select_tx.send(()).unwrap();
    request(&server, &mut terminal, 4).await?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "No changes"],
        &["main"],
    )?;
    assert_eq!(active_context(&home_path)?, original);
    edit_tx.send(()).unwrap();
    request(&server, &mut terminal, 5).await?;
    wait_footer(&mut terminal, &["feature · feature", "+2 -0"], &["main"])?;
    stage_tx.send(()).unwrap();
    request(&server, &mut terminal, 6).await?;
    wait_footer(&mut terminal, &["feature · feature", "+2 -0"], &["main"])?;
    commit_tx.send(()).unwrap();
    request(&server, &mut terminal, 7).await?;
    wait_footer(&mut terminal, &["feature · feature", "+2 -0"], &["main"])?;
    inspect_tx.send(()).unwrap();
    request(&server, &mut terminal, 8).await?;
    wait_footer(&mut terminal, &["feature · feature", "+2 -0"], &["main"])?;
    assert_eq!(active_context(&home_path)?, original);
    let requests = server.requests().await;
    ensure!(
        requests
            .iter()
            .take(3)
            .any(|body| String::from_utf8_lossy(body).contains("set_status_line_worktree")),
        "model never received the selection tool"
    );
    ensure!(
        String::from_utf8_lossy(&requests[3]).contains("persisted"),
        "selection result missing from model history"
    );
    let state = StateRuntime::init(
        SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(home_path.clone())?),
        "test".to_string(),
    )
    .await?;
    let thread_id = ThreadId::from_string(&original.0)?;
    let metadata = state
        .get_thread(thread_id)
        .await?
        .context("persisted thread")?;
    assert_eq!(metadata.cwd.as_path(), fixture.root);
    assert_eq!(
        metadata
            .status_line_worktree
            .as_ref()
            .unwrap()
            .root
            .as_str(),
        fixture
            .worktree
            .to_str()
            .context("UTF-8 worktree fixture path")?
    );

    // The implementation response stays open. PR creation/closure needs neither a commit nor a
    // user turn; focus return is an existing refresh trigger, respecting the ten-second minimum.
    fixture.set_pr("OPEN")?;
    terminal.write_input(b"\x1b[I")?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "+2 -0", "PR #17"],
        &["main"],
    )?;
    fixture.git(&fixture.worktree, &["checkout", "-qb", "outside"])?;
    fs::write(fixture.worktree.join("outside.txt"), "outside\n")?;
    fixture.git(&fixture.worktree, &["add", "."])?;
    fixture.git(&fixture.worktree, &["commit", "-qm", "outside"])?;
    wait_footer(&mut terminal, &["outside", "+3 -0"], &["main"])?;
    wait_footer(
        &mut terminal,
        &["feature · outside", "+3 -0", "PR #17"],
        &["main"],
    )?;
    fixture.set_pr("CLOSED")?;
    terminal.write_input(b"\x1b[I")?;
    wait_footer(&mut terminal, &["outside", "+3 -0"], &["PR #17"])?;
    assert_eq!(active_context(&home_path)?, original);

    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "remove",
            fixture
                .worktree
                .to_str()
                .context("UTF-8 worktree fixture path")?,
        ],
    )?;
    wait_footer(
        &mut terminal,
        &["repo", "main", "No changes"],
        &["PR #", "outside"],
    )?;
    check_worktree_warning(&mut terminal, &fixture)?;
    assert_eq!(
        state
            .get_thread(thread_id)
            .await?
            .unwrap()
            .status_line_worktree,
        metadata.status_line_worktree
    );
    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "add",
            fixture
                .worktree
                .to_str()
                .context("UTF-8 worktree fixture path")?,
            "outside",
        ],
    )?;
    wait_footer(&mut terminal, &["feature", "outside", "+3 -0"], &["main"])?;
    assert_eq!(active_context(&home_path)?, original);
    finish_tx.send(()).unwrap();
    terminal.wait_for_screen("WORKTREE_FLOW_DONE")?;
    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "remove",
            fixture
                .worktree
                .to_str()
                .context("UTF-8 worktree fixture path")?,
        ],
    )?;
    terminal.restart_cli(
        &fixture.root,
        &["--no-alt-screen", "resume", &original.0],
        &fixture.env,
    )?;
    terminal.wait_for_startup()?;
    wait_footer(
        &mut terminal,
        &["repo", "main", "No changes"],
        &["outside", "PR #"],
    )?;
    check_worktree_warning(&mut terminal, &fixture)?;
    assert_eq!(
        state
            .get_thread(thread_id)
            .await?
            .unwrap()
            .status_line_worktree,
        metadata.status_line_worktree
    );
    fixture.git(
        &fixture.root,
        &["show-ref", "--verify", "refs/heads/outside"],
    )?;
    drop(terminal);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_worktree_shell_first_use_without_managed_creation() -> Result<()> {
    let fixture = Fixture::new()?;
    let (use_tx, first_use) = gated(
        fixture.command(
            "first-use",
            "printf 'automatic\\n' > new.txt".to_string(),
            &fixture.worktree,
        ),
        "used",
    );
    let (finish_tx, finish_rx) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        stream(
            fixture.command(
                "create",
                format!(
                    "git worktree add -b feature {}",
                    shell_path(&fixture.worktree)?
                ),
                &fixture.root,
            ),
            "created",
        ),
        first_use,
        vec![StreamingSseChunk {
            gate: Some(finish_rx),
            body: responses::sse_completed("done"),
        }],
    ])
    .await;
    let (mut terminal, home_path) = start_fixture(&fixture, &server)?;
    submit(&mut terminal, "Implement in a worktree")?;
    request(&server, &mut terminal, 2).await?;
    wait_footer(&mut terminal, &["repo", "main", "No changes"], &["feature"])?;
    let original = active_context(&home_path)?;
    use_tx.send(()).unwrap();
    request(&server, &mut terminal, 3).await?;
    wait_footer(&mut terminal, &["feature · feature", "+1 -0"], &["main"])?;
    assert_eq!(active_context(&home_path)?, original);
    assert_eq!(original.1["cwd"], json!(fixture.root));
    // No selection tool call appears in model output; a real foreground command event adopted it.
    let records = rollout_records(&home_path)?;
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "response_item"
                && record["payload"]["name"] == "set_status_line_worktree")
    );
    finish_tx.send(()).unwrap();
    drop(terminal);
    ensure!(
        fixture.worktree.is_dir(),
        "session exit must not clean up a worktree"
    );
    server.shutdown().await;
    Ok(())
}
