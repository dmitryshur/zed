//! Reviewing GitHub pull requests: pick one of the repository's pull requests, check it out, and
//! browse its changes in the git panel's Compare tab, with its review threads drawn inline.
//! Comments go to the viewer's pending review on GitHub, which is submitted from Zed.

mod github;
mod line_mapping;
mod picker;
mod review;
mod submit_modal;
#[cfg(test)]
mod tests;
mod threads;

use std::time::Duration;

use util::paths::PathExt as _;

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, SharedString, Window, actions,
};
use language::LocalFile as _;
use project::{Project, git_store::Repository};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    branch_compare::{BranchComparison, LocalGitObjects},
    git_panel::GitPanel,
};
use github::{GitHubClient, GitHubRepository, PullRequestSummary, RepositoryCommands};
use picker::{ClientTask, PullRequestPicker};
pub(crate) use review::{PullRequestReview, PullRequestReviewEvent, ReviewRange};
use submit_modal::SubmitReviewModal;
pub(crate) use threads::{ReviewEditorBinding, ReviewFileContext};

actions!(
    git,
    [
        /// Picks one of the repository's open GitHub pull requests, checks it out, and shows its
        /// changes and review threads in the git panel's Compare tab.
        ReviewPullRequest,
        /// Comments on the current line or selected lines of a pull request under review. On a
        /// line with a thread, replies to it.
        AddPullRequestComment,
        /// Submits the pending review of the pull request under review.
        SubmitPullRequestReview,
        /// Resolves or unresolves the review thread on the current line.
        TogglePullRequestThreadResolved,
        /// Edits your latest comment in the review thread on the current line.
        EditPullRequestComment,
        /// Deletes your latest comment in the review thread on the current line, after
        /// confirming.
        DeletePullRequestComment,
        /// Moves to the next review thread of the shown file.
        NextPullRequestThread,
        /// Moves to the previous review thread of the shown file.
        PreviousPullRequestThread,
        /// Fetches the latest state of the pull request under review.
        RefreshPullRequest,
    ]
);

const REMOTE_NOT_SUPPORTED: &str = "Reviewing pull requests is not supported for remote projects";

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(review_pull_request);
    workspace.register_action(|workspace, _: &SubmitPullRequestReview, window, cx| {
        let Some(review) = active_review(workspace, cx) else {
            workspace.show_error("No pull request is under review", cx);
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            SubmitReviewModal::new(review, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &RefreshPullRequest, _, cx| {
        match active_review(workspace, cx) {
            Some(review) => review.update(cx, |review, cx| review.refresh(cx)),
            None => workspace.show_error("No pull request is under review", cx),
        }
    });
}

fn active_review(workspace: &Workspace, cx: &App) -> Option<Entity<PullRequestReview>> {
    let panel = workspace.panel::<GitPanel>(cx)?;
    panel
        .read(cx)
        .compare_list()
        .read(cx)
        .pull_request_review(cx)
}

/// Checking out a pull request over uncommitted work would carry it along or fail, so reviews
/// require a clean working tree.
pub(crate) fn uncommitted_changes_error(
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    number: u64,
    cx: &App,
) -> Option<String> {
    let repository = repository.read(cx);
    let summary = repository.status_summary();
    let has_changes = summary.count > summary.untracked;
    let work_directory = repository.work_directory_abs_path.clone();
    let has_unsaved_buffers = project
        .read(cx)
        .buffer_store()
        .read(cx)
        .buffers()
        .any(|buffer| {
            let buffer = buffer.read(cx);
            buffer.is_dirty()
                && project::File::from_dyn(buffer.file())
                    .is_some_and(|file| file.abs_path(cx).starts_with(&work_directory))
        });
    (has_changes || has_unsaved_buffers)
        .then(|| format!("Commit or stash your changes before reviewing #{number}"))
}

fn review_pull_request(
    workspace: &mut Workspace,
    _: &ReviewPullRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().read(cx);
    if !project.is_local() {
        workspace.show_error(REMOTE_NOT_SUPPORTED, cx);
        return;
    }
    let Some(repository) = project.active_repository(cx) else {
        workspace.show_error("No active repository", cx);
        return;
    };
    let github_repository = match GitHubRepository::for_repository(repository.read(cx)) {
        Ok(github_repository) => github_repository,
        Err(error) => {
            workspace.show_error(error, cx);
            return;
        }
    };
    let commands = RepositoryCommands::connect(&repository, cx);
    let client: ClientTask = cx
        .spawn(async move |_, cx| match commands.await {
            Ok(commands) => Ok(cx.update(|cx| GitHubClient::new(commands, cx))),
            Err(error) => Err(SharedString::from(format!("{error:#}"))),
        })
        .shared();
    let workspace_handle = workspace.weak_handle();
    let picker_repository = github_repository.clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        PullRequestPicker::new(
            client.clone(),
            picker_repository,
            move |pull_request, window, cx| {
                workspace_handle
                    .update(cx, |workspace, cx| {
                        start_review(
                            workspace,
                            repository.clone(),
                            github_repository.clone(),
                            client.clone(),
                            pull_request,
                            window,
                            cx,
                        )
                    })
                    .log_err();
            },
            window,
            cx,
        )
    });
}

struct CheckedOutPullRequest {
    client: GitHubClient,
    details: github::PullRequestDetails,
    files: Vec<github::PullRequestFile>,
    branch: String,
    git_objects: LocalGitObjects,
}

fn start_review(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    github_repository: GitHubRepository,
    client: ClientTask,
    pull_request: PullRequestSummary,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let number = pull_request.number;
    if let Some(error) = uncommitted_changes_error(&repository, &project, number, cx) {
        workspace.show_error(error, cx);
        return;
    }
    let Some(panel) = workspace.panel::<GitPanel>(cx) else {
        workspace.show_error("The git panel is not available", cx);
        return;
    };
    let git_objects = LocalGitObjects::connect(&repository, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = check_out(
            client,
            &github_repository,
            &repository,
            &pull_request,
            git_objects,
            cx,
        )
        .await;
        let checked_out = match result {
            Ok(checked_out) => checked_out,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| workspace.show_error(error, cx))
                    .log_err();
                return;
            }
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                let review = cx.new(|_| {
                    PullRequestReview::new(
                        checked_out.client,
                        github_repository,
                        repository.clone(),
                        project.clone(),
                        checked_out.git_objects,
                        checked_out.details,
                        checked_out.files,
                        checked_out.branch.into(),
                    )
                });
                let comparison = cx
                    .new(|cx| BranchComparison::for_pull_request(project, repository, review, cx));
                workspace.focus_panel::<GitPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.show_comparison(comparison, window, cx)
                });
            })
            .log_err();
    })
    .detach();
}

async fn check_out(
    client: ClientTask,
    github_repository: &GitHubRepository,
    repository: &Entity<Repository>,
    pull_request: &PullRequestSummary,
    git_objects: gpui::Task<Result<LocalGitObjects>>,
    cx: &mut AsyncWindowContext,
) -> Result<CheckedOutPullRequest> {
    let number = pull_request.number;
    let client = client.await.map_err(|error| anyhow!(error))?;
    // Git checks a branch out in only one worktree, and its refusal is cryptic once `gh`
    // wraps it.
    let work_directory = repository.read_with(cx, |repository, _| {
        repository.work_directory_abs_path.clone()
    });
    let worktrees = client
        .commands()
        .run("git", &["worktree", "list", "--porcelain"])
        .await?
        .into_stdout("Listing git worktrees")?;
    if let Some(other_worktree) =
        worktree_with_branch(&worktrees, &pull_request.head_ref_name, &work_directory)
    {
        bail!(
            "#{number}'s branch {} is checked out in {}. Open that folder to review it",
            pull_request.head_ref_name,
            other_worktree.compact().display()
        );
    }
    let checkout = cx.update(|_, cx| {
        client.commands().run_as_job(
            repository,
            "gh pr checkout",
            format!("Checking out #{number}…").into(),
            "gh",
            vec![
                "pr".into(),
                "checkout".into(),
                number.to_string(),
                "-R".into(),
                github_repository.name_with_owner(),
            ],
            cx,
        )
    })?;
    let (details, files, checkout) = futures::join!(
        client.pull_request_details(github_repository, number),
        client.pull_request_files(github_repository, number),
        checkout,
    );
    checkout?.into_stdout(&format!("`gh pr checkout {number}`"))?;
    let details = details?;
    let files = files?;
    let branch = client
        .commands()
        .run("git", &["symbolic-ref", "-q", "HEAD"])
        .await?
        .into_stdout("Reading the checked-out branch")?
        .trim()
        .to_string();

    let base_exists = client
        .commands()
        .run(
            "git",
            &[
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", details.base_ref_oid),
            ],
        )
        .await?
        .success;
    if !base_exists {
        let fetch = cx.update(|_, cx| {
            client.commands().run_as_job(
                repository,
                "fetch pull request base",
                format!("Fetching {}…", details.base_ref_name).into(),
                "git",
                vec![
                    "fetch".into(),
                    github_repository.remote_name.clone(),
                    details.base_ref_name.clone(),
                ],
                cx,
            )
        })?;
        fetch
            .await?
            .into_stdout(&format!("Fetching {}", details.base_ref_name))?;
    }
    review::ensure_pull_request_head(&client, github_repository, repository, &details, cx).await?;
    wait_for_branch(repository, &branch, cx).await;
    let git_objects = git_objects.await?;
    Ok(CheckedOutPullRequest {
        client,
        details,
        files,
        branch,
        git_objects,
    })
}

/// The worktree other than `current` that has `branch` checked out, from the output of
/// `git worktree list --porcelain`.
fn worktree_with_branch<'a>(porcelain: &'a str, branch: &str, current: &Path) -> Option<&'a Path> {
    let branch_ref = format!("refs/heads/{branch}");
    porcelain.split("\n\n").find_map(|entry| {
        let mut path = None;
        let mut has_branch = false;
        for line in entry.lines() {
            if let Some(worktree_path) = line.strip_prefix("worktree ") {
                path = Some(Path::new(worktree_path));
            } else if line.strip_prefix("branch ") == Some(branch_ref.as_str()) {
                has_branch = true;
            }
        }
        path.filter(|path| has_branch && *path != current)
    })
}

/// Zed notices the checkout through its file watcher; the comparison should start from the
/// checked-out state rather than flip modes once it's noticed.
async fn wait_for_branch(
    repository: &Entity<Repository>,
    branch: &str,
    cx: &mut AsyncWindowContext,
) {
    for _ in 0..100 {
        let checked_out = repository.read_with(cx, |repository, _| {
            repository
                .branch
                .as_ref()
                .is_some_and(|current| current.ref_name.as_ref() == branch)
        });
        if checked_out {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    log::warn!("Zed didn't notice the checkout of {branch} in time");
}
