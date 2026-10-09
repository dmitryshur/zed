use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::lock::Mutex as AsyncMutex;
use git::{Oid, repository::RepoPath};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Task};
use project::{Project, git_store::Repository};

use super::{
    github::{
        GitHubClient, GitHubRepository, NewThread, PullRequestDetails, PullRequestFile,
        ReviewThread, ReviewVerdict,
    },
    line_mapping::CommentableLines,
    uncommitted_changes_error,
};
use crate::branch_compare::LocalGitObjects;

/// Refreshing on window focus is skipped when the last refresh is this recent.
const FOCUS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Which of the pull request's changes the Compare tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewRange {
    All,
    SinceLastReview(Oid),
    Commit { sha: Oid, parent: Oid },
}

pub(crate) struct ReviewFile {
    pub(crate) previous_path: Option<RepoPath>,
    /// `None` when GitHub has no diff for the file (binary or too large).
    pub(crate) commentable: Option<CommentableLines>,
}

pub(crate) enum PullRequestReviewEvent {
    Updated,
}

/// A GitHub pull request checked out for review, with its comment threads. Comments are added
/// to the viewer's pending review, which GitHub keeps until it's submitted or discarded.
pub(crate) struct PullRequestReview {
    client: GitHubClient,
    github_repository: GitHubRepository,
    repository: Entity<Repository>,
    project: Entity<Project>,
    git_objects: LocalGitObjects,
    details: PullRequestDetails,
    files: HashMap<RepoPath, ReviewFile>,
    /// The local branch `gh pr checkout` checked out, e.g. `refs/heads/feature`.
    local_branch: SharedString,
    /// GitHub keeps one pending review per user, so mutations run one at a time.
    mutation_lock: Arc<AsyncMutex<()>>,
    refresh_task: Task<()>,
    last_refreshed: Instant,
    refresh_error: Option<SharedString>,
}

impl EventEmitter<PullRequestReviewEvent> for PullRequestReview {}

impl PullRequestReview {
    pub(crate) fn new(
        client: GitHubClient,
        github_repository: GitHubRepository,
        repository: Entity<Repository>,
        project: Entity<Project>,
        git_objects: LocalGitObjects,
        details: PullRequestDetails,
        files: Vec<PullRequestFile>,
        local_branch: SharedString,
    ) -> Self {
        Self {
            client,
            github_repository,
            repository,
            project,
            git_objects,
            details,
            files: review_files(files),
            local_branch,
            mutation_lock: Arc::default(),
            refresh_task: Task::ready(()),
            last_refreshed: Instant::now(),
            refresh_error: None,
        }
    }

    pub(crate) fn details(&self) -> &PullRequestDetails {
        &self.details
    }

    pub(crate) fn refresh_error(&self) -> Option<&SharedString> {
        self.refresh_error.as_ref()
    }

    pub(crate) fn file(&self, path: &RepoPath) -> Option<&ReviewFile> {
        self.files.get(path)
    }

    /// New paths of renamed files, mapped to their old paths.
    pub(crate) fn renames(&self) -> HashMap<RepoPath, RepoPath> {
        self.files
            .iter()
            .filter_map(|(path, file)| Some((path.clone(), file.previous_path.clone()?)))
            .collect()
    }

    pub(crate) fn threads_for_path<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> impl Iterator<Item = &'a ReviewThread> {
        self.details
            .threads
            .iter()
            .filter(move |thread| thread.path == path.as_unix_str())
    }

    /// The number of threads and unresolved threads per path.
    pub(crate) fn thread_counts(&self) -> HashMap<String, (usize, usize)> {
        let mut counts = HashMap::<String, (usize, usize)>::default();
        for thread in &self.details.threads {
            let count = counts.entry(thread.path.clone()).or_default();
            count.0 += 1;
            if !thread.is_resolved {
                count.1 += 1;
            }
        }
        counts
    }

    pub(crate) fn pending_comment_count(&self) -> usize {
        self.details
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .filter(|comment| comment.is_pending())
            .count()
    }

    pub(crate) fn is_on_branch(&self, cx: &App) -> bool {
        self.repository
            .read(cx)
            .branch
            .as_ref()
            .is_some_and(|branch| branch.ref_name == self.local_branch)
    }

    pub(crate) fn local_head(&self, cx: &App) -> Option<Oid> {
        self.repository
            .read(cx)
            .head_commit
            .as_ref()
            .and_then(|commit| commit.sha.parse().ok())
    }

    /// How many commits the checkout is behind the pull request, when it's behind.
    pub(crate) fn new_commit_count(&self, cx: &App) -> Option<usize> {
        let local_head = self.local_head(cx)?;
        if local_head == self.details.head_ref_oid {
            return None;
        }
        let position = self
            .details
            .commits
            .iter()
            .position(|commit| commit.oid == local_head)?;
        Some(self.details.commits.len() - position - 1)
    }

    pub(crate) fn load_text(&self, commit: Oid, path: &RepoPath, cx: &App) -> Task<Option<String>> {
        let git_objects = self.git_objects.clone();
        let path = path.clone();
        cx.background_spawn(async move {
            git_objects
                .load_text(commit, &path)
                .await
                .inspect_err(|error| log::debug!("Couldn't load {path:?} at {commit}: {error:#}"))
                .ok()
                .flatten()
        })
    }

    pub(crate) fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.last_refreshed.elapsed() >= FOCUS_REFRESH_INTERVAL {
            self.refresh(cx);
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.last_refreshed = Instant::now();
        let client = self.client.clone();
        let github_repository = self.github_repository.clone();
        let number = self.details.number;
        let repository = self.repository.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            let result = async {
                let (details, files) = futures::try_join!(
                    client.pull_request_details(&github_repository, number),
                    client.pull_request_files(&github_repository, number),
                )?;
                ensure_pull_request_head(&client, &github_repository, &repository, &details, cx)
                    .await?;
                anyhow::Ok((details, files))
            }
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((details, files)) => {
                        this.details = details;
                        this.files = review_files(files);
                        this.refresh_error = None;
                    }
                    Err(error) => {
                        log::error!("Couldn't refresh pull request #{number}: {error:#}");
                        this.refresh_error = Some(format!("{error:#}").into());
                    }
                }
                cx.emit(PullRequestReviewEvent::Updated);
                cx.notify();
            })
            .ok();
        });
    }

    /// Runs a mutation, applies its result locally, then refetches the canonical state (e.g. the
    /// pending review GitHub created).
    fn mutate<R: 'static>(
        &mut self,
        mutation: impl AsyncFnOnce(GitHubClient) -> Result<R> + 'static,
        apply: impl FnOnce(&mut Self, R) + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = self.client.clone();
        let lock = self.mutation_lock.clone();
        cx.spawn(async move |this, cx| {
            let result = {
                let _guard = lock.lock().await;
                mutation(client).await
            };
            this.update(cx, |this, cx| {
                let result = result.map(|value| apply(this, value));
                this.refresh(cx);
                cx.emit(PullRequestReviewEvent::Updated);
                cx.notify();
                result
            })?
        })
    }

    pub(crate) fn add_thread(
        &mut self,
        thread: NewThread,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let pull_request_id = self.details.id.clone();
        self.mutate(
            async move |client| client.add_thread(&pull_request_id, &thread).await,
            |this, thread| this.details.threads.push(thread),
            cx,
        )
    }

    pub(crate) fn reply(
        &mut self,
        thread_id: String,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let request_thread_id = thread_id.clone();
        self.mutate(
            async move |client| client.reply(&request_thread_id, &body).await,
            move |this, comment| {
                if let Some(thread) = this.thread_mut(&thread_id) {
                    thread.comments.push(comment);
                }
            },
            cx,
        )
    }

    pub(crate) fn edit_comment(
        &mut self,
        comment_id: String,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let request_comment_id = comment_id.clone();
        let request_body = body.clone();
        self.mutate(
            async move |client| {
                client
                    .update_comment(&request_comment_id, &request_body)
                    .await
            },
            move |this, ()| {
                if let Some(comment) = this
                    .details
                    .threads
                    .iter_mut()
                    .flat_map(|thread| &mut thread.comments)
                    .find(|comment| comment.id == comment_id)
                {
                    comment.body = body;
                }
            },
            cx,
        )
    }

    pub(crate) fn delete_comment(
        &mut self,
        comment_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let request_comment_id = comment_id.clone();
        self.mutate(
            async move |client| client.delete_comment(&request_comment_id).await,
            move |this, ()| {
                for thread in &mut this.details.threads {
                    thread.comments.retain(|comment| comment.id != comment_id);
                }
                this.details
                    .threads
                    .retain(|thread| !thread.comments.is_empty());
            },
            cx,
        )
    }

    pub(crate) fn set_thread_resolved(
        &mut self,
        thread_id: String,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let request_thread_id = thread_id.clone();
        self.mutate(
            async move |client| {
                client
                    .set_thread_resolved(&request_thread_id, resolved)
                    .await
            },
            move |this, ()| {
                if let Some(thread) = this.thread_mut(&thread_id) {
                    thread.is_resolved = resolved;
                }
            },
            cx,
        )
    }

    pub(crate) fn submit(
        &mut self,
        verdict: ReviewVerdict,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let pull_request_id = self.details.id.clone();
        let pending_review_id = self
            .details
            .pending_review
            .as_ref()
            .map(|review| review.id.clone());
        self.mutate(
            async move |client| {
                client
                    .submit_review(
                        &pull_request_id,
                        pending_review_id.as_deref(),
                        verdict,
                        &body,
                    )
                    .await
            },
            |this, ()| this.details.pending_review = None,
            cx,
        )
    }

    pub(crate) fn discard_pending_review(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(review_id) = self
            .details
            .pending_review
            .as_ref()
            .map(|review| review.id.clone())
        else {
            return Task::ready(Ok(()));
        };
        self.mutate(
            async move |client| client.delete_review(&review_id).await,
            |this, ()| {
                this.details.pending_review = None;
                for thread in &mut this.details.threads {
                    thread.comments.retain(|comment| !comment.is_pending());
                }
                this.details
                    .threads
                    .retain(|thread| !thread.comments.is_empty());
            },
            cx,
        )
    }

    /// Fast-forwards the checkout to the pull request's head with `gh pr checkout`.
    pub(crate) fn update_checkout(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let number = self.details.number;
        if let Some(error) = uncommitted_changes_error(&self.repository, &self.project, number, cx)
        {
            return Task::ready(Err(anyhow::anyhow!(error)));
        }
        let checkout = self.client.commands().run_as_job(
            &self.repository,
            "gh pr checkout",
            format!("Updating #{number}…").into(),
            "gh",
            vec![
                "pr".into(),
                "checkout".into(),
                number.to_string(),
                "-R".into(),
                self.github_repository.name_with_owner(),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            checkout
                .await?
                .into_stdout(&format!("`gh pr checkout {number}`"))?;
            this.update(cx, |this, cx| this.refresh(cx))?;
            Ok(())
        })
    }

    fn thread_mut(&mut self, thread_id: &str) -> Option<&mut ReviewThread> {
        self.details
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
    }
}

fn review_files(files: Vec<PullRequestFile>) -> HashMap<RepoPath, ReviewFile> {
    files
        .into_iter()
        .filter_map(|file| {
            let path = RepoPath::new(&file.filename)
                .inspect_err(|error| log::debug!("Skipping {}: {error:#}", file.filename))
                .ok()?;
            let previous_path = (file.status == "renamed")
                .then(|| file.previous_filename.as_deref())
                .flatten()
                .and_then(|previous| RepoPath::new(previous).ok());
            let commentable = file.patch.as_deref().map(CommentableLines::from_patch);
            Some((
                path,
                ReviewFile {
                    previous_path,
                    commentable,
                },
            ))
        })
        .collect()
}

/// Comments are placed by the pull request head's lines, so its commit must exist locally even
/// when the checkout is behind.
pub(crate) async fn ensure_pull_request_head(
    client: &GitHubClient,
    github_repository: &GitHubRepository,
    repository: &Entity<Repository>,
    details: &PullRequestDetails,
    cx: &mut AsyncApp,
) -> Result<()> {
    let commands = client.commands();
    let exists = commands
        .run(
            "git",
            &[
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", details.head_ref_oid),
            ],
        )
        .await?
        .success;
    if exists {
        return Ok(());
    }
    let fetch = cx.update(|cx| {
        commands.run_as_job(
            repository,
            "fetch pull request head",
            format!("Fetching #{}…", details.number).into(),
            "git",
            vec![
                "fetch".into(),
                github_repository.remote_name.clone(),
                format!("pull/{}/head", details.number),
            ],
            cx,
        )
    });
    fetch
        .await?
        .into_stdout("Fetching the pull request")
        .context("Couldn't fetch the pull request's latest commits")?;
    Ok(())
}
