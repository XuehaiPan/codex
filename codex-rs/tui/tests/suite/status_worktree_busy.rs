//! Reporting must remain usable while the execution-directory guards refuse `/cd`.

use super::*;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

/// Release the shell's FIFO read on success or failure without waiting for a reader.
struct BlockedTerminal(PathBuf);

impl BlockedTerminal {
    fn new(path: PathBuf) -> Result<Self> {
        ensure!(Command::new("mkfifo").arg(&path).status()?.success());
        Ok(Self(path))
    }
}

impl Drop for BlockedTerminal {
    fn drop(&mut self) {
        if let Ok(mut pipe) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.0)
        {
            let _ = pipe.write_all(b"done\n");
        }
    }
}

fn prepare_worktree(fixture: &Fixture) -> Result<()> {
    fixture.git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "-qb",
            "feature",
            fixture.worktree.to_str().context("UTF-8 worktree")?,
        ],
    )?;
    fs::write(fixture.worktree.join("file.txt"), "base\nadded\n")?;
    Ok(())
}

fn select_event(fixture: &Fixture) -> Value {
    responses::ev_function_call_with_namespace(
        "select",
        "mcp__codex_worktrees",
        "set_status_line_worktree",
        &json!({"path": fixture.worktree}).to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_worktree_selection_and_refresh_with_background_terminal() -> Result<()> {
    let fixture = Fixture::new()?;
    prepare_worktree(&fixture)?;
    let blocked = BlockedTerminal::new(fixture._temp.path().join("release-terminal"))?;
    let (select_tx, select) = gated(select_event(&fixture), "selected");
    let (_finish_tx, finish) = gated(responses::ev_completed("done"), "done");
    let (server, _) = start_streaming_sse_server(vec![
        stream(
            responses::ev_function_call(
                "background",
                "exec_command",
                &json!({
                    "cmd": format!("read -r signal < {}", shell_path(&blocked.0)?),
                    "workdir": fixture.root,
                    "shell": "/bin/sh",
                    "login": false,
                    "yield_time_ms": 1000,
                })
                .to_string(),
            ),
            "background-started",
        ),
        select,
        finish,
    ])
    .await;
    let (mut terminal, home) = start_fixture(&fixture, &server)?;
    submit(
        &mut terminal,
        "Keep a terminal running and implement in the worktree",
    )?;
    request(&server, &mut terminal, 2).await?;
    ensure!(
        String::from_utf8_lossy(&server.requests().await[1])
            .contains("Process running with session ID"),
        "the command must yield a live background terminal"
    );
    let original = active_context(&home)?;
    fixture.set_pr("OPEN")?;
    select_tx.send(()).unwrap();
    request(&server, &mut terminal, 3).await?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "+1 -0", "PR #17"],
        &["main"],
    )?;
    assert_eq!(active_context(&home)?, original);
    assert_eq!(original.1["cwd"], json!(fixture.root));
    assert_eq!(original.1["approval_policy"], "never");
    assert_eq!(original.1["sandbox_policy"]["type"], "danger-full-access");

    fs::write(fixture.worktree.join("file.txt"), "base\nadded\nexternal\n")?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "+2 -0", "PR #17"],
        &["main"],
    )?;
    assert_eq!(active_context(&home)?, original);
    terminal.write_input(b"\x1b")?;
    terminal.wait_for_screen("Conversation interrupted")?;
    submit(
        &mut terminal,
        &format!("/cd {}", fixture.worktree.display()),
    )?;
    // This checkout's composer rejects a known local terminal before the app-server guard
    // that reports `Active background terminals block /cd.` for other tracked terminals.
    terminal.wait_for_screen("Changing directories requires an idle primary session")?;
    terminal.wait_for_screen("1 background terminal running")?;

    // The same reporting context keeps refreshing after interruption, while `/cd` is blocked.
    fixture.git(&fixture.worktree, &["checkout", "-qb", "outside"])?;
    wait_footer(&mut terminal, &["feature · outside", "+2 -0"], &["main"])?;
    assert_eq!(active_context(&home)?, original);
    drop(blocked);
    drop(terminal);
    ensure!(
        fixture.worktree.is_dir(),
        "reporting must not clean up the worktree"
    );
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_worktree_selection_and_refresh_with_running_agent() -> Result<()> {
    let fixture = Fixture::new()?;
    prepare_worktree(&fixture)?;
    let (spawn_tx, spawn) = gated(
        responses::ev_function_call_with_namespace(
            "spawn",
            "agents",
            "spawn_agent",
            &json!({"message": "Hold the background agent open", "task_name": "background", "fork_turns": "none"})
                .to_string(),
        ),
        "spawned",
    );
    // Parent and child may request their next responses in either order. Hold both, identify
    // the parent by its spawn result, and release only its selection. The child stays running.
    let (first_tx, first) = gated(select_event(&fixture), "selected");
    let (second_tx, second) = gated(select_event(&fixture), "selected");
    let (_finish_tx, finish) = gated(responses::ev_completed("done"), "done");
    let (server, _) = start_streaming_sse_server(vec![spawn, first, second, finish]).await;
    let (mut terminal, home) = start_fixture_with_config(
        &fixture,
        &server,
        "features.multi_agent_v2 = { enabled = true, tool_namespace = \"agents\" }",
    )?;
    submit(
        &mut terminal,
        "Spawn an agent, then implement in the worktree",
    )?;
    request(&server, &mut terminal, 1).await?;
    let original = active_context(&home)?;
    spawn_tx.send(()).unwrap();
    request(&server, &mut terminal, 3).await?;
    let requests = server.requests().await;
    let is_parent = |body: &[u8]| -> Result<bool> {
        let body: Value = serde_json::from_slice(body)?;
        Ok(body["input"]
            .as_array()
            .context("model input")?
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "spawn"))
    };
    let first_is_parent = is_parent(&requests[1])?;
    assert_ne!(first_is_parent, is_parent(&requests[2])?);
    let (parent_tx, _child_tx) = if first_is_parent {
        (first_tx, second_tx)
    } else {
        (second_tx, first_tx)
    };
    let records = rollout_records(&home)?;
    let child_id = records
        .iter()
        .find(|record| record["type"] == "session_meta" && record["payload"]["id"] != original.0)
        .context("child rollout")?["payload"]["id"]
        .as_str()
        .context("child ID")?;
    let child_context = active_thread_context(&home, Some(child_id))?;
    fixture.set_pr("OPEN")?;
    parent_tx.send(()).unwrap();
    request(&server, &mut terminal, 4).await?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "+1 -0", "PR #17"],
        &["main"],
    )?;
    assert_eq!(active_thread_context(&home, Some(&original.0))?, original);
    assert_eq!(original.1["cwd"], json!(fixture.root));
    assert_eq!(original.1["approval_policy"], "never");
    assert_eq!(original.1["sandbox_policy"]["type"], "danger-full-access");

    terminal.write_input(b"\x1b")?;
    terminal.wait_for_screen("Conversation interrupted")?;
    submit(
        &mut terminal,
        &format!("/cd {}", fixture.worktree.display()),
    )?;
    terminal.wait_for_screen("Cannot change: another agent is running.")?;
    fs::write(fixture.worktree.join("file.txt"), "base\nadded\nexternal\n")?;
    wait_footer(
        &mut terminal,
        &["feature · feature", "+2 -0", "PR #17"],
        &["main"],
    )?;
    assert_eq!(active_thread_context(&home, Some(&original.0))?, original);
    assert_eq!(active_thread_context(&home, Some(child_id))?, child_context);
    ensure!(
        !thread_rollout_records(&home, Some(child_id))?
            .iter()
            .any(|record| {
                record["type"] == "event_msg" && record["payload"]["type"] == "turn_aborted"
            }),
        "the background agent must remain running"
    );
    drop(terminal);
    ensure!(
        fixture.worktree.is_dir(),
        "reporting must not clean up the worktree"
    );
    server.shutdown().await;
    Ok(())
}
