# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Worktree reporting context

The `current-dir`, `git-branch`, `branch-changes`, and `pull-request-number` status items report the selected implementation checkout. Matching terminal-title items use the same context. This selection only changes reporting: the thread's execution directory, command defaults, permissions, and configuration remain unchanged.

The agent can select a checkout with `set_status_line_worktree({"path": "/absolute/workspace-host/path"})`. Selection validates that it belongs to the session's Git repository. With no selection, the first live foreground command in another linked worktree may select it automatically. Creating or attaching a checkout alone does not select it. Once selected, incidental commands in the original directory or another checkout do not replace it. Select the original root explicitly to return the footer there. Explicit selection is needed for `git -C` and edits through absolute paths, since those operations do not identify their reporting checkout through command cwd.

The selection is saved per thread and validated on resume. A rejected selection attempt preserves the last successful selection, including when its footer update is still queued. New and forked threads start without a selection. Servers without metadata support can retain a selection only in memory for the current connection; the tool reports `persisted: false`. A metadata write failure is an error. Validation runs on the app-server workspace host; an unsupported attached executor environment is rejected instead of resolving its paths on the TUI machine.

Subagents have independent reporting selections. A subagent creating or selecting a worktree does not change its parent's selection or execution directory. Switching with `/subagents` displays the selected thread's reporting context; switching back restores the parent's context. A child inherits the parent's execution context, not its footer directory. Commands from a background thread and replayed history do not trigger automatic selection, so an agent working in the background must select its reporting worktree explicitly. Switching away cancels selection requests still in flight for the previously visible thread; completed selections remain saved.

If the saved path disappears or belongs to another repository, the footer falls back to the session directory, clears old Git/PR values, and warns once for that occurrence. The saved path remains selected. After successful revalidation of that path in the same repository, fresh probes restore its reporting context. Recovery permits a new warning if it disappears again. Timeouts, permission errors, and connection failures omit unavailable Git/PR values and retry without treating the checkout as deleted. If the session directory is also unavailable, its configured path remains visible without Git/PR values.

Selection follows the saved path: a new checkout from the same repository at that path is followed after validation. Codex does not search for a moved checkout or add worktree, branch, or PR cleanup when a thread ends or is deleted.

`branch-changes` counts net added/deleted text lines between the default-branch merge base and current checkout contents, including committed, staged, unstaged, and non-ignored untracked files. With an unchanged comparison base, staging or committing unchanged contents preserves the count; an uncommitted reversal can cancel a committed change. Renames count as removed and added file contents, with rename detection disabled so staging alone does not change the total. Recreating a file after staging its deletion compares its current contents with the original blob. Sessions started in a subdirectory still count the whole checkout. Existing remote-default ref precedence is preserved, and probes never fetch. Binary files and submodule pointers contribute no text lines: `No changes` means zero counted text-line changes, not necessarily a clean working tree. Conflicts, failures, timeouts, inconsistent branch/HEAD snapshots, and output reaching the 1 MiB limit omit the total instead of displaying a partial result. Local snapshots have a five-second total budget, with at most four concurrent untracked-file probes.

While the TUI is visible, local status is refreshed every five seconds, during work and while idle. PR information is refreshed independently every sixty seconds, even if the commit has not changed. Selection changes and validated restores refresh immediately. Command completion, turn completion or interruption, and focus return coalesce local refreshes for 250 ms; PR requests maintain at least ten seconds between attempts in the same reporting context. These intervals bound the next attempt, not network completion. Slow or unavailable GitHub responses do not delay local validation, branch names, or change counts. Remote default-branch discovery is cached for ten minutes per workspace connection, repository, and remote URL; local refs are still checked without fetching. Items disabled in both the status line and terminal title do not trigger unrelated probes, and a directory-only footer validates the saved path without querying GitHub.
