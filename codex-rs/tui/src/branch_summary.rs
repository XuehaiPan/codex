//! Branch and pull-request metadata for TUI status-line items.
//!
//! This module owns the git and GitHub probes behind the TUI `git-branch`, `pull-request-number`,
//! and `branch-changes` status-line items. It deliberately talks only to a
//! `WorkspaceCommandExecutor`, not to `tokio::process::Command`, so the same lookup logic works
//! when the TUI is connected to either an embedded or remote app-server.
//!
//! All lookups are best-effort. A failed command, missing `git` or `gh`, unauthenticated GitHub
//! CLI, non-git directory, or ambiguous repository state should result in absent optional metadata
//! rather than a user-visible error. The status line can then render whichever pieces are available
//! without blocking the rest of the UI.

#[path = "branch_summary_cache.rs"]
mod cache;

use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use futures::StreamExt;

use serde::Deserialize;

use crate::workspace_command::WorkspaceCommand;
#[cfg(test)]
use crate::workspace_command::WorkspaceCommandError;
use crate::workspace_command::WorkspaceCommandExecutor;
use crate::workspace_command::WorkspaceCommandOutput;

/// Net text-line changes from the default-branch merge base to current checkout contents.
///
/// Includes staged, unstaged and non-ignored untracked files. Binary files and submodule pointers
/// contribute no text lines, so a zero count does not imply a clean index or working tree.
/// Renames count as removed and added contents, independent of Git's index-based rename detection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitBranchDiffStats {
    /// Total added text lines in current contents.
    pub(crate) additions: u64,
    /// Total deleted text lines in current contents.
    pub(crate) deletions: u64,
}

/// Combined git metadata cached by the status line for one working directory.
///
/// A summary may contain only one of the fields when the other probe fails. Renderers should treat
/// missing fields as omitted optional UI rather than as a hard lookup failure.
#[derive(Clone, Debug, Default)]
pub(crate) struct StatusLineGitSummary {
    /// Open pull request associated with the current branch or HEAD commit.
    pub(crate) pull_request: Option<StatusLinePullRequest>,
    /// Net current-content changes relative to the repository default-branch merge base.
    pub(crate) branch_change_stats: Option<GitBranchDiffStats>,
}

/// Open GitHub pull request shown by the `pull-request-number` status-line item.
///
/// The URL is kept with the number so clickable renderers can open the same PR represented by the
/// compact label. Callers should only construct this for open PRs; closed or merged PRs are filtered
/// out by this module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusLinePullRequest {
    /// GitHub pull request number.
    pub(crate) number: u64,
    /// Browser URL for the pull request.
    pub(crate) url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DefaultBranch {
    /// Git ref used for merge-base comparison.
    ///
    /// This may be a remote-tracking ref such as `refs/remotes/origin/main`, which avoids
    /// comparing against a stale or absent local `main` branch.
    merge_ref: String,
}

#[derive(Deserialize)]
struct GhPullRequestView {
    number: u64,
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct GhPullRequestApiItem {
    number: u64,
    #[serde(rename = "html_url")]
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct GhRepoView {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: Option<String>,
    parent: Option<GhRepoParent>,
}

#[derive(Deserialize)]
struct GhRepoParent {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// Returns the checked-out branch name for one status-line working directory.
///
/// Detached HEADs, non-git directories, and command failures return `None` so the renderer can
/// omit the branch item without surfacing a background lookup error.
pub(crate) async fn current_branch_name(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<String> {
    let output = run_git_command(runner, cwd, &["branch", "--show-current"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }

    Some(output.stdout.trim().to_string()).filter(|name| !name.is_empty())
}

/// One checkout identity, sampled before and after a local snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitCheckoutIdentity {
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
}

/// Local metadata publishes independently of the slower GitHub lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusLineLocalGit {
    pub(crate) identity: GitCheckoutIdentity,
    /// Absent when any content probe is incomplete; never publish a partial total.
    pub(crate) branch_change_stats: Option<GitBranchDiffStats>,
}

pub(crate) const LOCAL_REFRESH_BUDGET: Duration = Duration::from_secs(5);
const STATUS_OUTPUT_BYTES_CAP: usize = 1_048_576;

/// Samples branch, HEAD and optional current contents within one total five-second budget.
///
/// External commits/checkouts invalidate an inconsistent snapshot. Working-tree edits themselves
/// are not transactional; the next refresh observes edits made while Git was reading files.
pub(crate) async fn local_git_snapshot(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    include_changes: bool,
) -> Option<StatusLineLocalGit> {
    tokio::time::timeout(LOCAL_REFRESH_BUDGET, async {
        let identity = checkout_identity(runner, cwd).await?;
        let branch_change_stats = if include_changes && identity.head.is_some() {
            branch_diff_stats_to_default_branch(runner, cwd).await
        } else {
            None
        };
        (checkout_identity(runner, cwd).await? == identity).then_some(StatusLineLocalGit {
            identity,
            branch_change_stats,
        })
    })
    .await
    .ok()
    .flatten()
}

pub(crate) async fn checkout_identity(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<GitCheckoutIdentity> {
    let branch = run_git_command(runner, cwd, &["branch", "--show-current"])
        .await
        .ok()?;
    if !branch.success() {
        return None;
    }
    let head = current_head_sha(runner, cwd).await;
    Some(GitCheckoutIdentity {
        branch: Some(branch.stdout.trim().to_owned()).filter(|branch| !branch.is_empty()),
        head,
    })
}

/// Counts the net difference from the merge base, without summing overlapping index/diff layers.
async fn branch_diff_stats_to_default_branch(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<GitBranchDiffStats> {
    // `ls-files` is cwd-relative whereas `diff` is repository-wide. Resolve on the workspace
    // host so sessions started in a subdirectory still include sibling edits and conflicts.
    let root = run_git_command(runner, cwd, &["rev-parse", "--show-toplevel"])
        .await
        .ok()?;
    if !root.success() || root.stdout.trim_end_matches(['\r', '\n']).is_empty() {
        return None;
    }
    let cwd = Path::new(root.stdout.trim_end_matches(['\r', '\n']));
    let conflicts = run_git_command(runner, cwd, &["ls-files", "-u", "-z"])
        .await
        .ok()?;
    if !conflicts.success() || !conflicts.stdout.is_empty() {
        return None;
    }
    let default_branch = get_default_branch(runner, cwd).await?;
    let merge_base = run_git_command(
        runner,
        cwd,
        &["merge-base", "HEAD", &default_branch.merge_ref],
    )
    .await
    .ok()?;
    if !merge_base.success() || merge_base.stdout.trim().is_empty() {
        return None;
    }
    // Ignore gitlink changes: their numstat entries describe pointers, not text-file contents.
    let tracked = run_git_command(
        runner,
        cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--no-renames",
            "--numstat",
            "-z",
            merge_base.stdout.trim(),
            "--",
        ],
    )
    .await
    .ok()?;
    if !tracked.success() {
        return None;
    }
    let ParsedNumstat {
        mut total,
        paths: tracked_paths,
    } = parse_numstat(&tracked.stdout)?;
    let untracked = run_git_command(
        runner,
        cwd,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await
    .ok()?;
    if !untracked.success() {
        return None;
    }
    let paths = nul_records(&untracked.stdout)?
        .into_iter()
        .map(str::to_owned);
    let merge_base = merge_base.stdout.trim();
    let mut probes = futures::stream::iter(paths.map(|path| {
        let previous = tracked_paths.get(path.as_str()).cloned();
        async move {
            // A staged deletion can have a new untracked file at the same path. Replace its
            // deletion count with a blob-to-file diff so staging alone cannot change the total.
            let blob = format!("{merge_base}:{path}");
            // Git recognizes `/dev/null` itself, including Git for Windows. Prefix a relative
            // path so a file named `-` cannot be interpreted as stdin by no-index diff.
            let path = format!("./{path}");
            let args = if previous.is_some() {
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--numstat",
                    "-z",
                    &blob,
                    "--",
                    &path,
                ]
            } else {
                vec![
                    "diff",
                    "--no-index",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--numstat",
                    "-z",
                    "--",
                    "/dev/null",
                    &path,
                ]
            };
            let output = run_git_command(runner, cwd, &args).await.ok()?;
            if !matches!(output.exit_code, 0 | 1) {
                return None;
            }
            Some((previous, parse_numstat(&output.stdout)?.total))
        }
    }))
    .buffer_unordered(4);
    while let Some(result) = probes.next().await {
        let (previous, stats) = result?;
        if let Some(previous) = previous {
            total.additions = total.additions.checked_sub(previous.additions)?;
            total.deletions = total.deletions.checked_sub(previous.deletions)?;
        }
        total.additions = total.additions.checked_add(stats.additions)?;
        total.deletions = total.deletions.checked_add(stats.deletions)?;
    }
    Some(total)
}

fn nul_records(stdout: &str) -> Option<Vec<&str>> {
    if stdout.is_empty() {
        return Some(Vec::new());
    }
    let records: Vec<_> = stdout.strip_suffix('\0')?.split('\0').collect();
    records
        .iter()
        .all(|record| !record.is_empty())
        .then_some(records)
}

#[derive(Default)]
struct ParsedNumstat<'a> {
    total: GitBranchDiffStats,
    paths: HashMap<&'a str, GitBranchDiffStats>,
}

/// Parses NUL-delimited numstat, including the extra old/new path fields for renames.
/// Tab/newline characters inside a path must never be mistaken for record separators.
fn parse_numstat(stdout: &str) -> Option<ParsedNumstat<'_>> {
    let mut parsed = ParsedNumstat::default();
    if stdout.is_empty() {
        return Some(parsed);
    }
    let mut records = stdout.strip_suffix('\0')?.split('\0');
    while let Some(record) = records.next() {
        let mut columns = record.splitn(3, '\t');
        let additions = columns.next()?;
        let deletions = columns.next()?;
        let mut path = columns.next()?;
        if path.is_empty() {
            let old = records.next()?;
            path = records.next()?;
            if old.is_empty() || path.is_empty() {
                return None;
            }
        }
        let stats = if additions == "-" && deletions == "-" {
            GitBranchDiffStats::default()
        } else {
            GitBranchDiffStats {
                additions: additions.parse().ok()?,
                deletions: deletions.parse().ok()?,
            }
        };
        parsed.total.additions = parsed.total.additions.checked_add(stats.additions)?;
        parsed.total.deletions = parsed.total.deletions.checked_add(stats.deletions)?;
        parsed.paths.insert(path, stats);
    }
    Some(parsed)
}

/// Returns git remotes in the order used for default-branch discovery.
///
/// `origin` is prioritized because most repositories use it as the canonical upstream. Other
/// remotes are still tried so fork or enterprise layouts with a differently named upstream can
/// produce branch-change stats when their remote HEAD is configured.
async fn get_git_remotes(runner: &dyn WorkspaceCommandExecutor, cwd: &Path) -> Option<Vec<String>> {
    let output = run_git_command(runner, cwd, &["remote"]).await.ok()?;
    if !output.success() {
        return None;
    }

    let mut remotes: Vec<String> = output.stdout.lines().map(str::to_string).collect();
    if let Some(pos) = remotes.iter().position(|remote| remote == "origin") {
        let origin = remotes.remove(pos);
        remotes.insert(0, origin);
    }
    Some(remotes)
}

/// Resolves the default branch ref that should be used for branch-change comparisons.
///
/// The lookup prefers remote-tracking refs over local branches so feature-only clones and stale
/// local `main` branches do not inflate the status-line diff. When no remote default is available,
/// local `main` or `master` is used as a last resort.
async fn get_default_branch(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<DefaultBranch> {
    let remotes = get_git_remotes(runner, cwd).await.unwrap_or_default();
    for remote in remotes {
        if let Some(branch) =
            get_remote_default_branch_from_symbolic_ref(runner, cwd, &remote).await
        {
            return Some(branch);
        }

        if let Some(branch) = get_remote_default_branch_from_remote_show(runner, cwd, &remote).await
        {
            return Some(branch);
        }
    }

    get_default_branch_local(runner, cwd).await
}

/// Resolves a remote's symbolic HEAD into a concrete remote-tracking ref.
///
/// The returned ref is verified before use. Without that check, a symbolic `origin/HEAD` left over
/// from an old fetch could point at a ref that no longer exists, causing the later merge-base probe
/// to fail in a less obvious place.
async fn get_remote_default_branch_from_symbolic_ref(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    remote: &str,
) -> Option<DefaultBranch> {
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let output = run_git_command(runner, cwd, &["symbolic-ref", "--quiet", &remote_head])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }

    let trimmed = output.stdout.trim();
    let remote_ref_prefix = format!("refs/remotes/{remote}/");
    trimmed.strip_prefix(&remote_ref_prefix)?;
    if !git_ref_exists(runner, cwd, trimmed).await {
        return None;
    }

    Some(DefaultBranch {
        merge_ref: trimmed.to_string(),
    })
}

/// Parses `git remote show` output to discover a remote's default branch ref.
///
/// This is a fallback for repositories where `refs/remotes/<remote>/HEAD` is not configured but
/// `git remote show` can still report the upstream HEAD branch. The concrete remote-tracking ref
/// must already exist locally before it is accepted.
async fn get_remote_default_branch_from_remote_show(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    remote: &str,
) -> Option<DefaultBranch> {
    let key = if let Some(connection) = runner.cache_namespace() {
        let common = run_git_command(
            runner,
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await
        .ok()?;
        let url = run_git_command(runner, cwd, &["remote", "get-url", remote])
            .await
            .ok()?;
        if !common.success() || !url.success() {
            return None;
        }
        Some(cache::RemoteDefaultKey {
            connection,
            common_dir: common.stdout.trim_end_matches(['\r', '\n']).to_owned(),
            remote: remote.to_owned(),
            url: url.stdout.trim_end_matches(['\r', '\n']).to_owned(),
        })
    } else {
        None
    };
    let cached = key.as_ref().and_then(|key| {
        cache::REMOTE_DEFAULTS
            .lock()
            .ok()?
            .get(key, tokio::time::Instant::now())
    });
    let branch = if let Some(cached) = cached {
        cached
    } else {
        // Reserve a negative entry before awaiting: the total local-refresh timeout may cancel
        // discovery, and must not cause another network attempt on every five-second refresh.
        if let Some(key) = key.as_ref()
            && let Ok(mut cache) = cache::REMOTE_DEFAULTS.lock()
        {
            cache.insert(key.clone(), None, tokio::time::Instant::now());
        }
        let output = run_git_command(runner, cwd, &["remote", "show", remote]).await;
        let branch = output
            .ok()
            .filter(WorkspaceCommandOutput::success)
            .and_then(|output| {
                output.stdout.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("HEAD branch:")
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                })
            });
        if let Some(key) = key
            && let Ok(mut cache) = cache::REMOTE_DEFAULTS.lock()
        {
            cache.insert(key, branch.clone(), tokio::time::Instant::now());
        }
        branch
    }?;
    // Cache only discovery. Ref existence is checked on each use, without fetching objects.
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    git_ref_exists(runner, cwd, &remote_ref)
        .await
        .then_some(DefaultBranch {
            merge_ref: remote_ref,
        })
}

/// Falls back to local `main` or `master` when no remote default branch can be found.
async fn get_default_branch_local(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<DefaultBranch> {
    for candidate in ["main", "master"] {
        let local_ref = format!("refs/heads/{candidate}");
        if git_ref_exists(runner, cwd, &local_ref).await {
            return Some(DefaultBranch {
                merge_ref: local_ref,
            });
        }
    }

    None
}

/// Checks whether a git ref exists in the status-line working directory.
async fn git_ref_exists(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    reference: &str,
) -> bool {
    run_git_command(
        runner,
        cwd,
        &["rev-parse", "--verify", "--quiet", reference],
    )
    .await
    .is_ok_and(|output| output.success())
}

/// Resolves the open PR associated with the current checkout.
///
/// Branch-based lookup is attempted first because it is cheap and mirrors `gh pr view`. Commit-based
/// lookup is used as a fallback so fork workflows can still find a PR opened against the upstream
/// repository even when `gh` infers the fork from the current checkout.
pub(crate) async fn open_pull_request(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    if let Some(pull_request) = open_pull_request_for_current_branch(runner, cwd).await {
        return Some(pull_request);
    }

    open_pull_request_for_head_commit(runner, cwd).await
}

/// Uses GitHub CLI's current-branch PR lookup.
async fn open_pull_request_for_current_branch(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    let output = run_gh_command(runner, cwd, &["pr", "view", "--json", "number,url,state"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    pull_request_from_view_output(&output.stdout)
}

/// Looks up open PRs for `HEAD` across the upstream/fork repository search order.
async fn open_pull_request_for_head_commit(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    let head_sha = current_head_sha(runner, cwd).await?;
    for repo in gh_repo_search_order(runner, cwd).await? {
        let endpoint = format!("repos/{repo}/commits/{head_sha}/pulls");
        let output = run_gh_command(
            runner,
            cwd,
            &[
                "api",
                "-H",
                "Accept: application/vnd.github+json",
                &endpoint,
            ],
        )
        .await
        .ok()?;
        if output.success()
            && let Some(pull_request) = pull_request_from_api_output(&output.stdout)
        {
            return Some(pull_request);
        }
    }

    None
}

/// Returns the current `HEAD` SHA for commit-based PR lookup.
async fn current_head_sha(runner: &dyn WorkspaceCommandExecutor, cwd: &Path) -> Option<String> {
    let output = run_git_command(runner, cwd, &["rev-parse", "HEAD"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }

    Some(output.stdout.trim().to_string()).filter(|sha| !sha.is_empty())
}

/// Returns repositories to query for commit-associated PRs, with parent before fork.
async fn gh_repo_search_order(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<Vec<String>> {
    let output = run_gh_command(
        runner,
        cwd,
        &["repo", "view", "--json", "nameWithOwner,parent"],
    )
    .await
    .ok()?;
    if !output.success() {
        return None;
    }

    repo_search_order_from_output(&output.stdout)
}

/// Parses `gh pr view --json number,url,state` output for an open PR.
fn pull_request_from_view_output(stdout: &str) -> Option<StatusLinePullRequest> {
    let pull_request = serde_json::from_str::<GhPullRequestView>(stdout).ok()?;
    pull_request
        .state
        .eq_ignore_ascii_case("open")
        .then_some(StatusLinePullRequest {
            number: pull_request.number,
            url: pull_request.url,
        })
}

/// Parses the GitHub REST commit-to-PR response and returns the first open PR.
fn pull_request_from_api_output(stdout: &str) -> Option<StatusLinePullRequest> {
    serde_json::from_str::<Vec<GhPullRequestApiItem>>(stdout)
        .ok()?
        .into_iter()
        .find(|pull_request| pull_request.state.eq_ignore_ascii_case("open"))
        .map(|pull_request| StatusLinePullRequest {
            number: pull_request.number,
            url: pull_request.url,
        })
}

/// Parses `gh repo view` output into the repository search order for fallback PR lookup.
///
/// Parent-first ordering matches upstream PR workflows: a branch may be checked out from a fork
/// while the open PR lives on the parent repository.
fn repo_search_order_from_output(stdout: &str) -> Option<Vec<String>> {
    let repo = serde_json::from_str::<GhRepoView>(stdout).ok()?;
    let mut repos = Vec::new();
    if let Some(parent) = repo.parent {
        repos.push(parent.name_with_owner);
    }
    if let Some(name_with_owner) = repo.name_with_owner
        && !repos.iter().any(|repo| repo == &name_with_owner)
    {
        repos.push(name_with_owner);
    }
    if repos.is_empty() {
        return None;
    }

    Some(repos)
}

/// Runs a git command through the workspace-command abstraction.
async fn run_git_command(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    args: &[&str],
) -> Result<WorkspaceCommandOutput, crate::workspace_command::WorkspaceCommandError> {
    let mut argv = Vec::with_capacity(args.len() + 3);
    argv.push("git".to_string());
    argv.push("-c".to_string());
    argv.push(codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let mut command = WorkspaceCommand::new(argv, cwd.to_path_buf())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    command.output_bytes_cap = Some(STATUS_OUTPUT_BYTES_CAP);
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env.insert(variable.to_owned(), None);
    }
    let output = runner.run(command).await?;
    // Buffered command responses have no truncation flag. Even exactly-at-cap output is ambiguous.
    if output.stdout.len() >= STATUS_OUTPUT_BYTES_CAP {
        return Err(crate::workspace_command::WorkspaceCommandError::new(
            "Git status output reached its byte limit",
        ));
    }
    Ok(output)
}

/// Runs a GitHub CLI command through the workspace-command abstraction.
///
/// Prompting is disabled because status-line probes are background UI work. A command that needs
/// authentication or user input should fail and leave the optional PR item hidden.
async fn run_gh_command(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    args: &[&str],
) -> Result<WorkspaceCommandOutput, crate::workspace_command::WorkspaceCommandError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("gh".to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    runner
        .run(
            WorkspaceCommand::new(argv, cwd.to_path_buf())
                .env("GH_PROMPT_DISABLED", "1")
                .env("GIT_TERMINAL_PROMPT", "0"),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_command::WorkspaceCommand;
    use pretty_assertions::assert_eq;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    #[tokio::test]
    async fn branch_changes_disabled_samples_only_checkout_identity() {
        let runner = FakeRunner::new(
            (0..2)
                .flat_map(|_| {
                    [
                        response(&["git", "branch", "--show-current"], 0, "feature\n"),
                        response(&["git", "rev-parse", "HEAD"], 0, "head-sha\n"),
                    ]
                })
                .collect(),
        );
        assert_eq!(
            local_git_snapshot(&runner, Path::new("/repo"), false).await,
            Some(StatusLineLocalGit {
                identity: GitCheckoutIdentity {
                    branch: Some("feature".into()),
                    head: Some("head-sha".into()),
                },
                branch_change_stats: None,
            })
        );
        assert!(runner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn branch_diff_stats_prefers_remote_default_ref_over_stale_local_branch() {
        let runner = FakeRunner::new(vec![
            response(&["git", "rev-parse", "--show-toplevel"], 0, "/repo\n"),
            response(&["git", "ls-files", "-u", "-z"], /*exit_code*/ 0, ""),
            response(&["git", "remote"], /*exit_code*/ 0, "origin\n"),
            response(
                &["git", "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                /*exit_code*/ 0,
                "refs/remotes/origin/main\n",
            ),
            response(
                &[
                    "git",
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main",
                ],
                /*exit_code*/ 0,
                "remote-main-sha\n",
            ),
            response(
                &["git", "merge-base", "HEAD", "refs/remotes/origin/main"],
                /*exit_code*/ 0,
                "base-sha\n",
            ),
            response(
                &[
                    "git",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=all",
                    "--no-renames",
                    "--numstat",
                    "-z",
                    "base-sha",
                    "--",
                ],
                /*exit_code*/ 0,
                "1\t0\tfile\0",
            ),
            response(
                &["git", "ls-files", "--others", "--exclude-standard", "-z"],
                0,
                "",
            ),
        ]);

        let stats = branch_diff_stats_to_default_branch(&runner, Path::new("/repo"))
            .await
            .expect("branch diff stats");

        assert_eq!(
            stats,
            GitBranchDiffStats {
                additions: 1,
                deletions: 0,
            }
        );
        assert!(runner.saw(&["git", "merge-base", "HEAD", "refs/remotes/origin/main"]));
    }

    #[tokio::test]
    async fn open_pull_request_uses_current_branch_view_first() {
        let runner = FakeRunner::new(vec![response(
            &["gh", "pr", "view", "--json", "number,url,state"],
            /*exit_code*/ 0,
            r#"{"number":20252,"url":"https://github.com/openai/codex/pull/20252","state":"OPEN"}"#,
        )]);

        let pull_request = open_pull_request(&runner, Path::new("/repo"))
            .await
            .expect("pull request");

        assert_eq!(
            pull_request,
            StatusLinePullRequest {
                number: 20_252,
                url: "https://github.com/openai/codex/pull/20252".to_string(),
            }
        );
        assert!(!runner.saw(&["git", "rev-parse", "HEAD"]));
    }

    #[tokio::test]
    async fn open_pull_request_falls_back_to_parent_repo_commit_lookup() {
        let runner = FakeRunner::new(vec![
            response(
                &["gh", "pr", "view", "--json", "number,url,state"],
                /*exit_code*/ 1,
                "",
            ),
            response(
                &["git", "rev-parse", "HEAD"],
                /*exit_code*/ 0,
                "head-sha\n",
            ),
            response(
                &["gh", "repo", "view", "--json", "nameWithOwner,parent"],
                /*exit_code*/ 0,
                r#"{"nameWithOwner":"fcoury/codex","parent":{"nameWithOwner":"openai/codex"}}"#,
            ),
            response(
                &[
                    "gh",
                    "api",
                    "-H",
                    "Accept: application/vnd.github+json",
                    "repos/openai/codex/commits/head-sha/pulls",
                ],
                /*exit_code*/ 0,
                r#"[{"number":20252,"html_url":"https://github.com/openai/codex/pull/20252","state":"open"}]"#,
            ),
        ]);

        let pull_request = open_pull_request(&runner, Path::new("/repo"))
            .await
            .expect("pull request");

        assert_eq!(
            pull_request,
            StatusLinePullRequest {
                number: 20_252,
                url: "https://github.com/openai/codex/pull/20252".to_string(),
            }
        );
        assert!(runner.saw(&[
            "gh",
            "api",
            "-H",
            "Accept: application/vnd.github+json",
            "repos/openai/codex/commits/head-sha/pulls",
        ]));
    }

    #[test]
    fn status_line_pr_view_parser_requires_open_pr() {
        assert_eq!(
            pull_request_from_view_output(
                r#"{"number":20252,"url":"https://github.com/openai/codex/pull/20252","state":"OPEN"}"#
            ),
            Some(StatusLinePullRequest {
                number: 20_252,
                url: "https://github.com/openai/codex/pull/20252".to_string(),
            })
        );

        assert_eq!(
            pull_request_from_view_output(
                r#"{"number":20252,"url":"https://github.com/openai/codex/pull/20252","state":"MERGED"}"#
            ),
            None
        );
    }

    #[test]
    fn status_line_pr_fallback_searches_parent_repo_first() {
        assert_eq!(
            repo_search_order_from_output(
                r#"{"nameWithOwner":"fcoury/codex","parent":{"nameWithOwner":"openai/codex"}}"#
            ),
            Some(vec!["openai/codex".to_string(), "fcoury/codex".to_string()])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn branch_changes_remote_discovery_uses_cache_but_rechecks_local_ref() {
        let mut responses = Vec::new();
        for _ in 0..3 {
            responses.extend([
                response(
                    &[
                        "git",
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-common-dir",
                    ],
                    0,
                    "/repo/.git\n",
                ),
                response(
                    &["git", "remote", "get-url", "origin"],
                    0,
                    "https://example.invalid/repo\n",
                ),
                response(
                    &[
                        "git",
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "refs/remotes/origin/main",
                    ],
                    0,
                    "abc\n",
                ),
            ]);
        }
        for _ in 0..2 {
            responses.push(response(
                &["git", "remote", "show", "origin"],
                0,
                "  HEAD branch: main\n",
            ));
        }
        let mut runner = FakeRunner::new(responses);
        runner.cache_namespace = Some(uuid::Uuid::new_v4());
        for elapsed in [
            Duration::ZERO,
            cache::REMOTE_DEFAULT_TTL - Duration::from_nanos(1),
            Duration::from_nanos(1),
        ] {
            tokio::time::advance(elapsed).await;
            assert_eq!(
                get_remote_default_branch_from_remote_show(&runner, Path::new("/repo"), "origin")
                    .await,
                Some(DefaultBranch {
                    merge_ref: "refs/remotes/origin/main".into()
                })
            );
        }
        assert!(runner.responses.lock().unwrap().is_empty());
        assert!(
            !runner
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|argv| argv.iter().any(|arg| arg == "fetch"))
        );
    }

    fn response(argv: &[&str], exit_code: i32, stdout: &str) -> FakeResponse {
        let mut argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        if argv.first().map(String::as_str) == Some("git") {
            argv.splice(
                1..1,
                [
                    "-c".to_string(),
                    codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string(),
                ],
            );
        }
        FakeResponse {
            argv,
            output: WorkspaceCommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        }
    }

    struct FakeResponse {
        argv: Vec<String>,
        output: WorkspaceCommandOutput,
    }

    struct FakeRunner {
        cache_namespace: Option<uuid::Uuid>,
        responses: Mutex<VecDeque<FakeResponse>>,
        seen: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<FakeResponse>) -> Self {
            Self {
                cache_namespace: None,
                responses: Mutex::new(responses.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn saw(&self, argv: &[&str]) -> bool {
            let mut argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
            if argv.first().map(String::as_str) == Some("git") {
                argv.splice(
                    1..1,
                    [
                        "-c".to_string(),
                        codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string(),
                    ],
                );
            }
            self.seen
                .lock()
                .expect("seen lock")
                .iter()
                .any(|seen| seen == &argv)
        }
    }

    impl WorkspaceCommandExecutor for FakeRunner {
        fn cache_namespace(&self) -> Option<uuid::Uuid> {
            self.cache_namespace
        }

        fn run(
            &self,
            command: WorkspaceCommand,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                    + Send
                    + '_,
            >,
        > {
            self.seen
                .lock()
                .expect("seen lock")
                .push(command.argv.clone());
            Box::pin(async move {
                let mut responses = self.responses.lock().expect("responses lock");
                let index = responses
                    .iter()
                    .position(|response| response.argv == command.argv)
                    .unwrap_or_else(|| panic!("missing fake response for {:?}", command.argv));
                let response = responses.remove(index).expect("fake response");
                Ok(response.output)
            })
        }
    }
}

#[cfg(test)]
#[path = "branch_summary_tests.rs"]
mod content_tests;
