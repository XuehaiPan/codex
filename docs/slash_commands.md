# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

`/pwd` describes the thread's execution directory. The status-line directory can instead describe the selected implementation worktree. Updating that reporting selection does not require `/cd`, interrupt an active turn, or change when `/cd` is available.

Reporting selection and refresh also work while another agent or a background terminal remains running. Those activities can still block `/cd` after `Esc` interrupts the foreground turn; they do not need to be stopped to follow the implementation worktree in the footer.

`/subagents` switches the visible thread. The footer then uses that thread's own reporting selection without changing its execution directory. Selecting a worktree for a subagent does not change the main agent's directory or selection. Parent-owned subagent views retain their existing command restrictions, including blocking `/pwd`. Background commands and replayed history do not automatically select a worktree; use the explicit selection tool for an agent working in the background.
