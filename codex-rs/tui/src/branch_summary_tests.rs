use super::*;
use crate::status_test_support::GitFixture;
use pretty_assertions::assert_eq;

fn repository() -> GitFixture {
    let repo = GitFixture::new();
    std::fs::write(repo.root.join("file.txt"), "one\ntwo\n").unwrap();
    repo.git(&["add", "file.txt"]);
    repo.git(&["commit", "-m", "two-line base"]);
    repo.worktree_git(&["reset", "--hard", "main"]);
    repo
}

fn commit_contents(repo: &GitFixture, contents: &str) {
    std::fs::write(repo.worktree.join("file.txt"), contents).unwrap();
    repo.worktree_git(&["add", "file.txt"]);
    repo.worktree_git(&["commit", "-m", "feature"]);
}

#[tokio::test]
async fn branch_changes_include_current_contents() {
    let repo = repository();
    commit_contents(&repo, "one\ntwo\ncommitted\n");
    std::fs::write(
        repo.worktree.join("file.txt"),
        "one\ntwo\ncommitted\nunstaged\n",
    )
    .unwrap();
    let expected = Some(GitBranchDiffStats {
        additions: 2,
        deletions: 0,
    });
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &repo.worktree).await,
        expected
    );
    repo.worktree_git(&["add", "file.txt"]);
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &repo.worktree).await,
        expected
    );
    repo.worktree_git(&["commit", "-m", "same contents"]);
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &repo.worktree).await,
        expected
    );
}

#[tokio::test]
async fn branch_changes_dirty_revert_cancels_committed_change() {
    let repo = repository();
    commit_contents(&repo, "one\ntwo\ncommitted\n");
    std::fs::write(repo.worktree.join("file.txt"), "one\ntwo\n").unwrap();
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &repo.worktree).await,
        Some(GitBranchDiffStats {
            additions: 0,
            deletions: 0
        })
    );
}

async fn counts(repo: &GitFixture) -> Option<GitBranchDiffStats> {
    local_git_snapshot(&repo.runner, &repo.worktree, true)
        .await?
        .branch_change_stats
}

#[tokio::test]
async fn branch_changes_mixed_staged_unstaged_and_untracked() {
    let repo = repository();
    std::fs::write(repo.worktree.join("file.txt"), "one\nstaged\n").unwrap();
    repo.worktree_git(&["add", "file.txt"]);
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 1,
            deletions: 1
        })
    );
    std::fs::write(repo.worktree.join("file.txt"), "one\nstaged\nunstaged\n").unwrap();
    std::fs::write(repo.worktree.join("new text"), "new\ntext").unwrap();
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 4,
            deletions: 1
        })
    );
    repo.worktree_git(&["add", "."]);
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 4,
            deletions: 1
        })
    );
    std::fs::remove_file(repo.worktree.join("file.txt")).unwrap();
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 2,
            deletions: 2
        })
    );
}

#[tokio::test]
async fn branch_changes_renames_and_unusual_filenames() {
    let repo = repository();
    // Tabs/newlines are legal Unix filenames; spaces and leading dashes are portable.
    let mut names = vec!["space name", "-"];
    if !cfg!(windows) {
        names.extend(["tab\tname", "line\nname"]);
    }
    repo.worktree_git(&["mv", "file.txt", "renamed file"]);
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 2,
            deletions: 2
        })
    );
    for name in &names {
        std::fs::write(repo.worktree.join(name), "one line").unwrap();
    }
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: names.len() as u64 + 2,
            deletions: 2
        })
    );
    repo.worktree_git(&["add", "."]);
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: names.len() as u64 + 2,
            deletions: 2
        })
    );
}

#[tokio::test]
async fn branch_changes_empty_binary_ignored_and_submodule_files() {
    let repo = repository();
    std::fs::create_dir_all(repo.root.join(".git/info")).unwrap();
    std::fs::write(repo.root.join(".git/info/exclude"), "ignored\n").unwrap();
    std::fs::write(repo.worktree.join("ignored"), "not counted\n").unwrap();
    std::fs::write(repo.worktree.join("empty"), "").unwrap();
    std::fs::write(repo.worktree.join("binary"), b"binary\0contents\n").unwrap();
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 0,
            deletions: 0
        })
    );
    repo.worktree_git(&["add", "empty", "binary"]);
    let head = checkout_identity(&repo.runner, &repo.worktree)
        .await
        .unwrap()
        .head
        .unwrap();
    repo.worktree_git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{head},submodule"),
    ]);
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 0,
            deletions: 0
        })
    );
}

#[tokio::test]
async fn branch_changes_detached_head_and_missing_comparison_ref() {
    let repo = repository();
    commit_contents(&repo, "one\ntwo\nfeature\n");
    repo.worktree_git(&["checkout", "--detach"]);
    let snapshot = local_git_snapshot(&repo.runner, &repo.worktree, true)
        .await
        .unwrap();
    assert_eq!(snapshot.identity.branch, None);
    assert!(snapshot.identity.head.is_some());
    assert_eq!(
        snapshot.branch_change_stats,
        Some(GitBranchDiffStats {
            additions: 1,
            deletions: 0
        })
    );
    repo.git(&["branch", "-m", "main", "trunk"]);
    assert_eq!(counts(&repo).await, None);
}

#[tokio::test]
async fn branch_changes_conflicts_are_incomplete() {
    let repo = repository();
    commit_contents(&repo, "feature\n");
    std::fs::write(repo.root.join("file.txt"), "main\n").unwrap();
    repo.git(&["add", "file.txt"]);
    repo.git(&["commit", "-m", "conflicting main"]);
    let result = run_git_command(&repo.runner, &repo.worktree, &["merge", "main"])
        .await
        .unwrap();
    assert_eq!(result.exit_code, 1);
    assert_eq!(counts(&repo).await, None);
}

#[test]
fn branch_changes_numstat_rejects_malformed_or_partial_records() {
    for value in [
        "1\t0\tfile",
        "1\t0",
        "oops\t0\tfile\0",
        "-\t1\tfile\0",
        "1\t0\t\0old\0",
        "1\t0\t\0\0new\0",
        concat!("18446744073709551615\t0\ta\0", "1\t0\tb\0"),
    ] {
        assert_eq!(
            parse_numstat(value).map(|parsed| parsed.total),
            None,
            "{value:?}"
        );
    }
    assert_eq!(
        parse_numstat("1\t2\t\0old\tname\0new\nname\0-\t-\tbinary\0").map(|parsed| parsed.total),
        Some(GitBranchDiffStats {
            additions: 1,
            deletions: 2
        })
    );
}

struct OutputRunner {
    stdout: String,
}
impl WorkspaceCommandExecutor for OutputRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                + Send
                + '_,
        >,
    > {
        assert_eq!(command.output_bytes_cap, Some(STATUS_OUTPUT_BYTES_CAP));
        assert!(!command.disable_output_cap);
        Box::pin(async {
            Ok(WorkspaceCommandOutput {
                exit_code: 0,
                stdout: self.stdout.clone(),
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test]
async fn branch_changes_output_limit_rejects_exact_and_over_cap() {
    for size in [
        STATUS_OUTPUT_BYTES_CAP - 1,
        STATUS_OUTPUT_BYTES_CAP,
        STATUS_OUTPUT_BYTES_CAP + 1,
    ] {
        let runner = OutputRunner {
            stdout: "x".repeat(size),
        };
        assert_eq!(
            run_git_command(&runner, Path::new("/repo"), &["status"])
                .await
                .is_ok(),
            size < STATUS_OUTPUT_BYTES_CAP
        );
    }
}

struct ChangingRunner<'a> {
    repo: &'a GitFixture,
    changed: std::sync::atomic::AtomicBool,
    checkout: bool,
}
impl WorkspaceCommandExecutor for ChangingRunner<'_> {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let is_diff = command.argv.iter().any(|arg| arg == "--numstat");
            let result = self.repo.runner.run(command).await;
            if is_diff && !self.changed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if self.checkout {
                    self.repo.worktree_git(&["checkout", "--detach"]);
                } else {
                    self.repo
                        .worktree_git(&["commit", "--allow-empty", "-m", "external commit"]);
                }
            }
            result
        })
    }
}

#[tokio::test]
async fn branch_changes_rejects_external_commit_or_checkout_during_probe() {
    for checkout in [false, true] {
        let repo = repository();
        let runner = ChangingRunner {
            repo: &repo,
            changed: false.into(),
            checkout,
        };
        assert_eq!(
            local_git_snapshot(&runner, &repo.worktree, true).await,
            None
        );
        assert!(runner.changed.load(std::sync::atomic::Ordering::SeqCst));
    }
}

struct UntrackedRunner {
    active: std::sync::atomic::AtomicUsize,
    maximum: std::sync::atomic::AtomicUsize,
    completed: std::sync::atomic::AtomicUsize,
    permits: tokio::sync::Semaphore,
    entered: tokio::sync::Notify,
    fail_after_first: bool,
}
impl UntrackedRunner {
    fn new(fail_after_first: bool) -> Self {
        Self {
            active: 0.into(),
            maximum: 0.into(),
            completed: 0.into(),
            permits: tokio::sync::Semaphore::new(0),
            entered: tokio::sync::Notify::new(),
            fail_after_first,
        }
    }
}
struct ActiveProbe<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for ActiveProbe<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
impl WorkspaceCommandExecutor for UntrackedRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            let args = &command.argv[3..];
            let stdout = match args.first().map(String::as_str) {
                Some("branch") => "feature\n".to_owned(),
                Some("remote") => String::new(),
                Some("rev-parse" | "merge-base") => "abc\n".to_owned(),
                Some("ls-files") if args.iter().any(|arg| arg == "--others") => {
                    (0..12).map(|i| format!("file{i}\0")).collect()
                }
                Some("ls-files") => String::new(),
                Some("diff") if args.iter().any(|arg| arg == "--no-index") => {
                    let active = self.active.fetch_add(1, SeqCst) + 1;
                    let _active = ActiveProbe(&self.active);
                    self.maximum.fetch_max(active, SeqCst);
                    self.entered.notify_one();
                    self.permits.acquire().await.unwrap().forget();
                    let completed = self.completed.fetch_add(1, SeqCst);
                    if self.fail_after_first && completed > 0 {
                        return Err(WorkspaceCommandError::new("untracked file disappeared"));
                    }
                    return Ok(WorkspaceCommandOutput {
                        exit_code: 1,
                        stdout: "1\t0\tfile\0".to_owned(),
                        stderr: String::new(),
                    });
                }
                Some("diff") => String::new(),
                _ => panic!("unexpected probe {args:?}"),
            };
            Ok(WorkspaceCommandOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn branch_changes_untracked_concurrency_and_total_timeout() {
    use std::sync::atomic::Ordering::SeqCst;
    let runner = std::sync::Arc::new(UntrackedRunner::new(false));
    let probe_runner = runner.clone();
    let probe =
        tokio::spawn(
            async move { local_git_snapshot(&*probe_runner, Path::new("/repo"), true).await },
        );
    while runner.active.load(SeqCst) < 4 {
        runner.entered.notified().await;
    }
    assert_eq!(runner.maximum.load(SeqCst), 4);
    runner.permits.add_permits(1);
    while runner.completed.load(SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(LOCAL_REFRESH_BUDGET - Duration::from_millis(1)).await;
    assert!(!probe.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(probe.await.unwrap(), None);
    assert_eq!(runner.active.load(SeqCst), 0);
    assert_eq!(runner.maximum.load(SeqCst), 4);
}

#[tokio::test]
async fn branch_changes_untracked_failure_never_publishes_partial_count() {
    use std::sync::atomic::Ordering::SeqCst;
    let runner = UntrackedRunner::new(true);
    runner.permits.add_permits(12);
    let result = local_git_snapshot(&runner, Path::new("/repo"), true)
        .await
        .unwrap();
    assert!(runner.completed.load(SeqCst) >= 2);
    assert_eq!(result.branch_change_stats, None);
    assert_eq!(runner.active.load(SeqCst), 0);
}

#[tokio::test]
async fn branch_changes_untracked_bounded_success() {
    use std::sync::atomic::Ordering::SeqCst;
    let runner = std::sync::Arc::new(UntrackedRunner::new(false));
    let probe_runner = runner.clone();
    let probe =
        tokio::spawn(
            async move { local_git_snapshot(&*probe_runner, Path::new("/repo"), true).await },
        );
    while runner.active.load(SeqCst) < 4 {
        runner.entered.notified().await;
    }
    runner.permits.add_permits(12);
    assert_eq!(
        probe.await.unwrap().unwrap().branch_change_stats,
        Some(GitBranchDiffStats {
            additions: 12,
            deletions: 0
        })
    );
    assert_eq!(runner.completed.load(SeqCst), 12);
    assert_eq!(runner.maximum.load(SeqCst), 4);
    assert_eq!(runner.active.load(SeqCst), 0);
}

#[tokio::test]
async fn branch_changes_probes_preserve_index_objects_and_refs() {
    fn object_paths(root: &Path) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                paths.extend(object_paths(&path));
            } else {
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }
    let repo = repository();
    std::fs::write(repo.worktree.join("file.txt"), "one\ntwo\nunstaged\n").unwrap();
    std::fs::write(repo.worktree.join("untracked"), "new\n").unwrap();
    let index = run_git_command(
        &repo.runner,
        &repo.worktree,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .await
    .unwrap()
    .stdout;
    let index = Path::new(index.trim_end());
    let index_before = std::fs::read(index).unwrap();
    let objects_before = object_paths(&repo.root.join(".git/objects"));
    let refs_before = run_git_command(&repo.runner, &repo.root, &["show-ref"])
        .await
        .unwrap();
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 2,
            deletions: 0
        })
    );
    assert_eq!(std::fs::read(index).unwrap(), index_before);
    assert_eq!(
        object_paths(&repo.root.join(".git/objects")),
        objects_before
    );
    assert_eq!(
        run_git_command(&repo.runner, &repo.root, &["show-ref"])
            .await
            .unwrap(),
        refs_before
    );
}

#[tokio::test]
async fn branch_changes_nested_cwd_includes_sibling_untracked_contents() {
    let repo = repository();
    let nested = repo.worktree.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(repo.worktree.join("sibling.txt"), "sibling\n").unwrap();
    let expected = Some(GitBranchDiffStats {
        additions: 1,
        deletions: 0,
    });
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &nested).await,
        expected
    );
    repo.worktree_git(&["add", "."]);
    assert_eq!(
        branch_diff_stats_to_default_branch(&repo.runner, &nested).await,
        expected
    );
}

#[tokio::test]
async fn branch_changes_filesystem_rename_is_stable_across_staging_and_commit() {
    let repo = repository();
    std::fs::rename(
        repo.worktree.join("file.txt"),
        repo.worktree.join("renamed.txt"),
    )
    .unwrap();
    let before = counts(&repo).await.unwrap();
    repo.worktree_git(&["add", "-A"]);
    assert_eq!(counts(&repo).await, Some(before.clone()));
    repo.worktree_git(&["commit", "-m", "rename"]);
    assert_eq!(counts(&repo).await, Some(before));
}

#[tokio::test]
async fn branch_changes_recreated_staged_deletion_compares_current_contents() {
    let repo = repository();
    repo.worktree_git(&["rm", "file.txt"]);
    std::fs::write(repo.worktree.join("file.txt"), "one\ntwo\n").unwrap();
    assert_eq!(
        counts(&repo).await,
        Some(GitBranchDiffStats {
            additions: 0,
            deletions: 0
        })
    );
    std::fs::write(repo.worktree.join("file.txt"), "one\nnew\n").unwrap();
    let expected = Some(GitBranchDiffStats {
        additions: 1,
        deletions: 1,
    });
    assert_eq!(counts(&repo).await, expected);
    repo.worktree_git(&["add", "file.txt"]);
    assert_eq!(counts(&repo).await, expected);
}

#[tokio::test]
async fn branch_changes_nested_cwd_rejects_sibling_conflict() {
    let repo = repository();
    commit_contents(&repo, "feature\n");
    std::fs::write(repo.root.join("file.txt"), "main\n").unwrap();
    repo.git(&["add", "file.txt"]);
    repo.git(&["commit", "-m", "conflicting main"]);
    assert_eq!(
        run_git_command(&repo.runner, &repo.worktree, &["merge", "main"])
            .await
            .unwrap()
            .exit_code,
        1
    );
    let nested = repo.worktree.join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert_eq!(
        local_git_snapshot(&repo.runner, &nested, true)
            .await
            .unwrap()
            .branch_change_stats,
        None
    );
}

#[tokio::test(start_paused = true)]
async fn branch_changes_cancelled_remote_discovery_is_cached() {
    struct SlowRemote {
        namespace: uuid::Uuid,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl WorkspaceCommandExecutor for SlowRemote {
        fn cache_namespace(&self) -> Option<uuid::Uuid> {
            Some(self.namespace)
        }
        fn run(
            &self,
            command: WorkspaceCommand,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let args: Vec<_> = command.argv[3..].iter().map(String::as_str).collect();
                let (exit_code, stdout) = match args.as_slice() {
                    ["branch", "--show-current"] => (0, "feature\n"),
                    ["rev-parse", "HEAD"] | ["merge-base", ..] => (0, "abc\n"),
                    ["rev-parse", "--show-toplevel"] => (0, "/repo\n"),
                    ["ls-files", ..] | ["diff", ..] => (0, ""),
                    ["remote"] => (0, "origin\n"),
                    ["symbolic-ref", ..] => (1, ""),
                    ["rev-parse", "--path-format=absolute", "--git-common-dir"] => {
                        (0, "/repo/.git\n")
                    }
                    ["remote", "get-url", "origin"] => (0, "https://example.invalid/repo\n"),
                    ["remote", "show", "origin"] => {
                        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        return std::future::pending().await;
                    }
                    ["rev-parse", "--verify", "--quiet", "refs/heads/main"] => (0, "abc\n"),
                    _ => panic!("unexpected probe {args:?}"),
                };
                Ok(WorkspaceCommandOutput {
                    exit_code,
                    stdout: stdout.into(),
                    stderr: String::new(),
                })
            })
        }
    }
    let runner = SlowRemote {
        namespace: uuid::Uuid::new_v4(),
        calls: 0.into(),
    };
    let start = tokio::time::Instant::now();
    assert_eq!(
        local_git_snapshot(&runner, Path::new("/repo"), true).await,
        None
    );
    assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        local_git_snapshot(&runner, Path::new("/repo"), true)
            .await
            .is_some()
    );
    tokio::time::advance(cache::REMOTE_DEFAULT_TTL - start.elapsed() - Duration::from_nanos(1))
        .await;
    assert!(
        local_git_snapshot(&runner, Path::new("/repo"), true)
            .await
            .is_some()
    );
    assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert_eq!(
        local_git_snapshot(&runner, Path::new("/repo"), true).await,
        None
    );
    assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
