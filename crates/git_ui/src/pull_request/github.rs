use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use futures::{AsyncReadExt as _, future::BoxFuture};
use git::{Oid, RemoteUrl};
use gpui::{
    App, AppContext as _, Entity, Global, SharedString, Task,
    http_client::{AsyncBody, HttpClient, Method, Request, StatusCode, http::HeaderMap},
};
use parking_lot::Mutex;
use project::git_store::{LocalRepositoryState, Repository, RepositoryState};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::json;
use time::OffsetDateTime;

const GITHUB_HOST: &str = "github.com";
const API_URL: &str = "https://api.github.com";
const GRAPHQL_URL: &str = "https://api.github.com/graphql";
const REMOTE_NOT_SUPPORTED: &str = "Reviewing pull requests is not supported for remote projects";

/// The GitHub repository behind the remote that pull requests are opened against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitHubRepository {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) remote_name: String,
}

impl GitHubRepository {
    /// Follows `Repository::default_remote_url`: the upstream remote, else origin.
    pub(crate) fn for_repository(repository: &Repository) -> Result<Self> {
        let (remote_name, url) = match (
            &repository.remote_upstream_url,
            &repository.remote_origin_url,
        ) {
            (Some(url), _) => ("upstream", url),
            (None, Some(url)) => ("origin", url),
            (None, None) => bail!("This repository has no origin or upstream remote"),
        };
        Self::parse(remote_name, url).with_context(|| {
            format!("The {remote_name} remote ({url}) isn't a github.com repository")
        })
    }

    fn parse(remote_name: &str, url: &str) -> Option<Self> {
        let url = url.parse::<RemoteUrl>().ok()?;
        if url.host_str()? != GITHUB_HOST {
            return None;
        }
        let mut segments = url.path().trim_matches('/').split('/');
        let owner = segments.next().filter(|owner| !owner.is_empty())?;
        let name = segments.next()?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        if name.is_empty() || segments.next().is_some() {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            name: name.to_string(),
            remote_name: remote_name.to_string(),
        })
    }

    pub(crate) fn name_with_owner(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl CommandOutput {
    pub(crate) fn into_stdout(self, description: &str) -> Result<String> {
        if self.success {
            return Ok(self.stdout);
        }
        let details = self
            .stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_string();
        Err(anyhow!("{description} failed: {details}"))
    }
}

/// Runs `gh` and `git` for pull requests. Tests replace it through [`GlobalCommandRunner`].
pub(crate) trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        working_directory: &Path,
        environment: &HashMap<String, String>,
    ) -> BoxFuture<'static, Result<CommandOutput>>;
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        working_directory: &Path,
        environment: &HashMap<String, String>,
    ) -> BoxFuture<'static, Result<CommandOutput>> {
        let mut command = util::command::new_command(program);
        command
            .args(args)
            .current_dir(working_directory)
            .envs(environment.iter())
            // Credential prompts would wait forever on stdin we never write to.
            .env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(util::command::Stdio::null())
            .kill_on_drop(true);
        let program = program.to_string();
        Box::pin(async move {
            let output = command
                .output()
                .await
                .with_context(|| format!("Couldn't run `{program}`"))?;
            Ok(CommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

#[derive(Clone)]
pub(crate) struct GlobalCommandRunner(pub(crate) Arc<dyn CommandRunner>);

impl Global for GlobalCommandRunner {}

fn command_runner(cx: &App) -> Arc<dyn CommandRunner> {
    cx.try_global::<GlobalCommandRunner>()
        .map(|runner| runner.0.clone())
        .unwrap_or_else(|| Arc::new(SystemCommandRunner))
}

/// Shared by every client so `gh auth token` runs once per session.
#[derive(Default)]
struct GlobalGitHubToken(Arc<Mutex<Option<String>>>);

impl Global for GlobalGitHubToken {}

/// Runs commands in a local repository with its environment (`PATH`, `GITHUB_TOKEN`, …).
#[derive(Clone)]
pub(crate) struct RepositoryCommands {
    runner: Arc<dyn CommandRunner>,
    working_directory: Arc<Path>,
    environment: Arc<HashMap<String, String>>,
}

impl RepositoryCommands {
    pub(crate) fn connect(repository: &Entity<Repository>, cx: &mut App) -> Task<Result<Self>> {
        let runner = command_runner(cx);
        let working_directory = repository.read(cx).work_directory_abs_path.clone();
        let environment = repository.update(cx, |repository, _| {
            repository.send_job("connect pull request review", None, |state, _| async move {
                match state {
                    RepositoryState::Local(LocalRepositoryState { environment, .. }) => {
                        Ok(environment)
                    }
                    RepositoryState::Remote(_) => Err(anyhow!(REMOTE_NOT_SUPPORTED)),
                }
            })
        });
        cx.background_spawn(async move {
            Ok(Self {
                runner,
                working_directory,
                environment: environment.await??,
            })
        })
    }

    pub(crate) async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let args = args.iter().map(ToString::to_string).collect::<Vec<_>>();
        self.runner
            .run(program, &args, &self.working_directory, &self.environment)
            .await
    }

    /// Runs the command in the repository's job queue, so it doesn't race Zed's own git
    /// commands (e.g. for the index lock).
    pub(crate) fn run_as_job(
        &self,
        repository: &Entity<Repository>,
        description: &'static str,
        status: SharedString,
        program: &'static str,
        args: Vec<String>,
        cx: &mut App,
    ) -> Task<Result<CommandOutput>> {
        let this = self.clone();
        let result = repository.update(cx, |repository, _| {
            repository.send_job(description, Some(status), move |_, _| async move {
                this.runner
                    .run(program, &args, &this.working_directory, &this.environment)
                    .await
            })
        });
        cx.background_spawn(async move { result.await? })
    }
}

/// Talks to the GitHub API with the token of the `gh` CLI, or `GITHUB_TOKEN`.
#[derive(Clone)]
pub(crate) struct GitHubClient {
    http_client: Arc<dyn HttpClient>,
    commands: RepositoryCommands,
    token: Arc<Mutex<Option<String>>>,
}

impl GitHubClient {
    pub(crate) fn new(commands: RepositoryCommands, cx: &mut App) -> Self {
        let token = cx.default_global::<GlobalGitHubToken>().0.clone();
        Self {
            http_client: cx.http_client(),
            commands,
            token,
        }
    }

    pub(crate) fn commands(&self) -> &RepositoryCommands {
        &self.commands
    }

    async fn token(&self) -> Result<String> {
        if let Some(token) = self.token.lock().clone() {
            return Ok(token);
        }
        let from_gh = self
            .commands
            .run("gh", &["auth", "token", "--hostname", GITHUB_HOST])
            .await
            .and_then(|output| output.into_stdout("`gh auth token`"))
            .map(|stdout| stdout.trim().to_string());
        let token = match from_gh {
            Ok(token) if !token.is_empty() => token,
            Ok(_) => self.token_from_environment().context(
                "`gh auth token` printed no token; run `gh auth login` or set GITHUB_TOKEN",
            )?,
            Err(error) => self
                .token_from_environment()
                .with_context(|| format!("{error:#}; run `gh auth login` or set GITHUB_TOKEN"))?,
        };
        *self.token.lock() = Some(token.clone());
        Ok(token)
    }

    fn token_from_environment(&self) -> Option<String> {
        self.commands
            .environment
            .get("GITHUB_TOKEN")
            .cloned()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .filter(|token| !token.is_empty())
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
    ) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let mut retried = false;
        loop {
            let token = self.token().await?;
            let mut request = Request::builder()
                .method(method.clone())
                .uri(url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if body.is_some() {
                request = request.header("Content-Type", "application/json");
            }
            let request = request.body(AsyncBody::from(body.clone().unwrap_or_default()))?;
            let mut response = self.http_client.send(request).await?;
            let mut bytes = Vec::new();
            response.body_mut().read_to_end(&mut bytes).await?;
            // The cached token may have been revoked or rotated by `gh`.
            if response.status() == StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                *self.token.lock() = None;
                continue;
            }
            return Ok((response.status(), response.headers().clone(), bytes));
        }
    }

    async fn graphql<T: DeserializeOwned>(
        &self,
        operation_name: &str,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = json!({
            "query": query,
            "operationName": operation_name,
            "variables": variables,
        })
        .to_string();
        let (status, _, bytes) = self.send(Method::POST, GRAPHQL_URL, Some(body)).await?;
        if !status.is_success() {
            bail!(failed_request_message(status, &bytes));
        }
        let response = serde_json::from_slice::<GraphQlResponse<T>>(&bytes)
            .with_context(|| format!("Unexpected response from GitHub to {operation_name}"))?;
        if !response.errors.is_empty() {
            let messages = response
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>();
            bail!("GitHub: {}", messages.join("; "));
        }
        response.data.context("GitHub returned no data")
    }

    async fn rest_get_all<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut url = format!("{API_URL}{path}");
        let mut items = Vec::new();
        loop {
            let (status, headers, bytes) = self.send(Method::GET, &url, None).await?;
            if !status.is_success() {
                bail!(failed_request_message(status, &bytes));
            }
            items.extend(
                serde_json::from_slice::<Vec<T>>(&bytes)
                    .with_context(|| format!("Unexpected response from GitHub to {path}"))?,
            );
            match headers
                .get("link")
                .and_then(|link| link.to_str().ok())
                .and_then(next_page_url)
            {
                Some(next) => url = next,
                None => return Ok(items),
            }
        }
    }

    pub(crate) async fn search_pull_requests(
        &self,
        repository: &GitHubRepository,
        filter: PullRequestFilter,
    ) -> Result<Vec<PullRequestSummary>> {
        let data: SearchData = self
            .graphql(
                "PullRequestSearch",
                SEARCH_QUERY,
                json!({ "searchQuery": filter.search_query(repository) }),
            )
            .await?;
        Ok(data
            .search
            .nodes
            .into_iter()
            .filter_map(|node| serde_json::from_value(node).ok())
            .collect())
    }

    pub(crate) async fn pull_request_details(
        &self,
        repository: &GitHubRepository,
        number: u64,
    ) -> Result<PullRequestDetails> {
        let mut cursor = None::<String>;
        let mut details = None::<PullRequestDetails>;
        loop {
            let data: DetailsData = self
                .graphql(
                    "PullRequestDetails",
                    &details_query(),
                    json!({
                        "owner": repository.owner,
                        "name": repository.name,
                        "number": number,
                        "threadsCursor": cursor,
                    }),
                )
                .await?;
            let pull_request = data
                .repository
                .and_then(|repository| repository.pull_request)
                .with_context(|| format!("Pull request #{number} was not found"))?;
            let page = pull_request.review_threads;
            match details.as_mut() {
                Some(details) => details.threads.extend(page.nodes),
                None => {
                    details = Some(PullRequestDetails {
                        viewer_login: data.viewer.login,
                        id: pull_request.id,
                        number: pull_request.number,
                        title: pull_request.title,
                        url: pull_request.url,
                        is_draft: pull_request.is_draft,
                        viewer_did_author: pull_request.viewer_did_author,
                        author: pull_request.author.map(|author| author.login),
                        base_ref_name: pull_request.base_ref_name,
                        base_ref_oid: pull_request.base_ref_oid,
                        head_ref_name: pull_request.head_ref_name,
                        head_ref_oid: pull_request.head_ref_oid,
                        commits: pull_request
                            .commits
                            .nodes
                            .into_iter()
                            .map(|node| PullRequestCommit {
                                oid: node.commit.oid,
                                short_oid: node.commit.abbreviated_oid,
                                headline: node.commit.message_headline,
                                parent: node
                                    .commit
                                    .parents
                                    .nodes
                                    .into_iter()
                                    .next()
                                    .map(|parent| parent.oid),
                            })
                            .collect(),
                        pending_review: pull_request.pending.nodes.into_iter().next().map(
                            |review| PendingReview {
                                id: review.id,
                                comment_count: review.comments.total_count,
                            },
                        ),
                        last_review_commit: pull_request
                            .submitted
                            .nodes
                            .into_iter()
                            .rev()
                            .find(|review| review.viewer_did_author)
                            .and_then(|review| review.commit)
                            .map(|commit| commit.oid),
                        threads: page.nodes,
                    })
                }
            }
            if page.page_info.has_next_page && page.page_info.end_cursor.is_some() {
                cursor = page.page_info.end_cursor;
            } else {
                break;
            }
        }
        details.context("GitHub returned no pull request")
    }

    pub(crate) async fn pull_request_files(
        &self,
        repository: &GitHubRepository,
        number: u64,
    ) -> Result<Vec<PullRequestFile>> {
        self.rest_get_all(&format!(
            "/repos/{}/{}/pulls/{number}/files?per_page=100",
            repository.owner, repository.name
        ))
        .await
    }

    /// Adds a thread to the viewer's pending review, which GitHub creates when there is none.
    pub(crate) async fn add_thread(
        &self,
        pull_request_id: &str,
        thread: &NewThread,
    ) -> Result<ReviewThread> {
        let data: AddThreadData = self
            .graphql(
                "AddPullRequestReviewThread",
                &with_fragments(ADD_THREAD_MUTATION),
                json!({
                    "pullRequestId": pull_request_id,
                    "path": thread.path,
                    "body": thread.body,
                    "line": thread.line,
                    "side": thread.side,
                    "startLine": thread.start.map(|(_, line)| line),
                    "startSide": thread.start.map(|(side, _)| side),
                }),
            )
            .await?;
        // GitHub answers lines outside the pull request's diff with a null thread, not an error.
        data.add_pull_request_review_thread
            .thread
            .context("GitHub only allows comments within the diff")
    }

    /// Replies in the viewer's pending review, which GitHub creates when there is none.
    pub(crate) async fn reply(&self, thread_id: &str, body: &str) -> Result<ReviewComment> {
        let data: ReplyData = self
            .graphql(
                "AddPullRequestReviewThreadReply",
                &with_fragments(REPLY_MUTATION),
                json!({ "threadId": thread_id, "body": body }),
            )
            .await?;
        data.add_pull_request_review_thread_reply
            .comment
            .context("GitHub didn't add the reply")
    }

    pub(crate) async fn update_comment(&self, comment_id: &str, body: &str) -> Result<()> {
        self.graphql::<serde_json::Value>(
            "UpdatePullRequestReviewComment",
            UPDATE_COMMENT_MUTATION,
            json!({ "commentId": comment_id, "body": body }),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn delete_comment(&self, comment_id: &str) -> Result<()> {
        self.graphql::<serde_json::Value>(
            "DeletePullRequestReviewComment",
            DELETE_COMMENT_MUTATION,
            json!({ "commentId": comment_id }),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<()> {
        let (operation_name, mutation) = if resolved {
            ("ResolveReviewThread", RESOLVE_THREAD_MUTATION)
        } else {
            ("UnresolveReviewThread", UNRESOLVE_THREAD_MUTATION)
        };
        self.graphql::<serde_json::Value>(
            operation_name,
            mutation,
            json!({ "threadId": thread_id }),
        )
        .await?;
        Ok(())
    }

    /// Submits the pending review, or creates and submits one when there is none (e.g. an
    /// approval without comments).
    pub(crate) async fn submit_review(
        &self,
        pull_request_id: &str,
        pending_review_id: Option<&str>,
        event: ReviewVerdict,
        body: &str,
    ) -> Result<()> {
        match pending_review_id {
            Some(review_id) => {
                self.graphql::<serde_json::Value>(
                    "SubmitPullRequestReview",
                    SUBMIT_REVIEW_MUTATION,
                    json!({ "reviewId": review_id, "event": event, "body": body }),
                )
                .await?
            }
            None => {
                self.graphql::<serde_json::Value>(
                    "AddPullRequestReview",
                    ADD_REVIEW_MUTATION,
                    json!({ "pullRequestId": pull_request_id, "event": event, "body": body }),
                )
                .await?
            }
        };
        Ok(())
    }

    pub(crate) async fn delete_review(&self, review_id: &str) -> Result<()> {
        self.graphql::<serde_json::Value>(
            "DeletePullRequestReview",
            DELETE_REVIEW_MUTATION,
            json!({ "reviewId": review_id }),
        )
        .await?;
        Ok(())
    }
}

fn failed_request_message(status: StatusCode, body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: String,
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => format!("GitHub request failed ({status}): {}", error.message),
        Err(_) => format!("GitHub request failed ({status})"),
    }
}

/// Finds the `rel="next"` URL of a `Link` header.
fn next_page_url(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|param| param.trim() == r#"rel="next""#)
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PullRequestFilter {
    ReviewRequested,
    CreatedByMe,
    AssignedToMe,
    AllOpen,
}

impl PullRequestFilter {
    pub(crate) const ALL: [Self; 4] = [
        Self::ReviewRequested,
        Self::CreatedByMe,
        Self::AssignedToMe,
        Self::AllOpen,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReviewRequested => "Review requested",
            Self::CreatedByMe => "Created by me",
            Self::AssignedToMe => "Assigned to me",
            Self::AllOpen => "All open",
        }
    }

    fn search_query(self, repository: &GitHubRepository) -> String {
        let qualifier = match self {
            Self::ReviewRequested => " review-requested:@me",
            Self::CreatedByMe => " author:@me",
            Self::AssignedToMe => " assignee:@me",
            Self::AllOpen => "",
        };
        format!(
            "repo:{} is:pr is:open sort:updated-desc{qualifier}",
            repository.name_with_owner()
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestSummary {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) is_draft: bool,
    #[serde(deserialize_with = "login")]
    pub(crate) author: Option<String>,
    pub(crate) head_ref_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PullRequestDetails {
    pub(crate) viewer_login: String,
    pub(crate) id: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) is_draft: bool,
    pub(crate) viewer_did_author: bool,
    pub(crate) author: Option<String>,
    pub(crate) base_ref_name: String,
    pub(crate) base_ref_oid: Oid,
    pub(crate) head_ref_name: String,
    pub(crate) head_ref_oid: Oid,
    pub(crate) commits: Vec<PullRequestCommit>,
    pub(crate) pending_review: Option<PendingReview>,
    /// The commit the viewer's latest submitted review was made at.
    pub(crate) last_review_commit: Option<Oid>,
    pub(crate) threads: Vec<ReviewThread>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PullRequestCommit {
    pub(crate) oid: Oid,
    pub(crate) short_oid: String,
    pub(crate) headline: String,
    pub(crate) parent: Option<Oid>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingReview {
    pub(crate) id: String,
    pub(crate) comment_count: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum DiffSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SubjectType {
    Line,
    File,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewThread {
    pub(crate) id: String,
    pub(crate) is_resolved: bool,
    pub(crate) is_outdated: bool,
    pub(crate) path: String,
    pub(crate) diff_side: DiffSide,
    pub(crate) start_diff_side: Option<DiffSide>,
    /// The last line, in the pull request head's version of the file (or the base's, on the
    /// left side). `None` once the thread is outdated.
    pub(crate) line: Option<u32>,
    pub(crate) start_line: Option<u32>,
    pub(crate) original_line: Option<u32>,
    pub(crate) original_start_line: Option<u32>,
    pub(crate) subject_type: SubjectType,
    pub(crate) viewer_can_resolve: bool,
    pub(crate) viewer_can_unresolve: bool,
    pub(crate) viewer_can_reply: bool,
    #[serde(deserialize_with = "nodes")]
    pub(crate) comments: Vec<ReviewComment>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewComment {
    pub(crate) id: String,
    pub(crate) body: String,
    #[serde(deserialize_with = "login")]
    pub(crate) author: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) created_at: OffsetDateTime,
    pub(crate) state: String,
    pub(crate) viewer_did_author: bool,
    pub(crate) viewer_can_update: bool,
    pub(crate) viewer_can_delete: bool,
    pub(crate) diff_hunk: String,
    #[serde(deserialize_with = "commit_oid")]
    pub(crate) original_commit: Option<Oid>,
}

impl ReviewComment {
    pub(crate) fn is_pending(&self) -> bool {
        self.state == "PENDING"
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct PullRequestFile {
    pub(crate) filename: String,
    pub(crate) previous_filename: Option<String>,
    pub(crate) status: String,
    pub(crate) patch: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NewThread {
    pub(crate) path: String,
    pub(crate) body: String,
    pub(crate) side: DiffSide,
    pub(crate) line: u32,
    /// The first line of a multi-line comment.
    pub(crate) start: Option<(DiffSide, u32)>,
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

fn nodes<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Nodes::<T>::deserialize(deserializer).map(|nodes| nodes.nodes)
}

#[derive(Deserialize)]
struct Actor {
    login: String,
}

fn login<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<Actor>::deserialize(deserializer).map(|actor| actor.map(|actor| actor.login))
}

#[derive(Deserialize)]
struct GitObject {
    oid: Oid,
}

fn commit_oid<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Oid>, D::Error> {
    Option::<GitObject>::deserialize(deserializer).map(|commit| commit.map(|commit| commit.oid))
}

#[derive(Deserialize)]
struct SearchData {
    search: Nodes<serde_json::Value>,
}

#[derive(Deserialize)]
struct DetailsData {
    viewer: Actor,
    repository: Option<DetailsRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsRepository {
    pull_request: Option<DetailsPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsPullRequest {
    id: String,
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    viewer_did_author: bool,
    author: Option<Actor>,
    base_ref_name: String,
    base_ref_oid: Oid,
    head_ref_name: String,
    head_ref_oid: Oid,
    commits: Nodes<CommitNode>,
    pending: Nodes<PendingReviewNode>,
    submitted: Nodes<SubmittedReviewNode>,
    review_threads: ThreadPage,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitData {
    oid: Oid,
    abbreviated_oid: String,
    message_headline: String,
    parents: Nodes<GitObject>,
}

#[derive(Deserialize)]
struct PendingReviewNode {
    id: String,
    comments: TotalCount,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TotalCount {
    total_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmittedReviewNode {
    viewer_did_author: bool,
    commit: Option<GitObject>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPage {
    page_info: PageInfo,
    nodes: Vec<ReviewThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddThreadData {
    add_pull_request_review_thread: AddThreadPayload,
}

#[derive(Deserialize)]
struct AddThreadPayload {
    thread: Option<ReviewThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyData {
    add_pull_request_review_thread_reply: ReplyPayload,
}

#[derive(Deserialize)]
struct ReplyPayload {
    comment: Option<ReviewComment>,
}

const SEARCH_QUERY: &str = r#"
query PullRequestSearch($searchQuery: String!) {
  search(query: $searchQuery, type: ISSUE, first: 50) {
    nodes {
      ... on PullRequest {
        number
        title
        updatedAt
        isDraft
        author { login }
        headRefName
      }
    }
  }
}
"#;

const COMMENT_FRAGMENT: &str = r#"
fragment CommentFields on PullRequestReviewComment {
  id
  body
  author { login }
  createdAt
  state
  viewerDidAuthor
  viewerCanUpdate
  viewerCanDelete
  diffHunk
  originalCommit { oid }
}
"#;

const THREAD_FRAGMENT: &str = r#"
fragment ThreadFields on PullRequestReviewThread {
  id
  isResolved
  isOutdated
  path
  diffSide
  startDiffSide
  line
  startLine
  originalLine
  originalStartLine
  subjectType
  viewerCanResolve
  viewerCanUnresolve
  viewerCanReply
  comments(first: 100) { nodes { ...CommentFields } }
}
"#;

const DETAILS_QUERY: &str = r#"
query PullRequestDetails($owner: String!, $name: String!, $number: Int!, $threadsCursor: String) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      number
      title
      url
      isDraft
      viewerDidAuthor
      author { login }
      baseRefName
      baseRefOid
      headRefName
      headRefOid
      commits(last: 100) {
        nodes {
          commit {
            oid
            abbreviatedOid
            messageHeadline
            parents(first: 1) { nodes { oid } }
          }
        }
      }
      pending: reviews(first: 1, states: [PENDING]) {
        nodes { id comments { totalCount } }
      }
      submitted: reviews(last: 50, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
        nodes { viewerDidAuthor commit { oid } }
      }
      reviewThreads(first: 100, after: $threadsCursor) {
        pageInfo { hasNextPage endCursor }
        nodes { ...ThreadFields }
      }
    }
  }
}
"#;

const ADD_THREAD_MUTATION: &str = r#"
mutation AddPullRequestReviewThread($pullRequestId: ID!, $path: String!, $body: String!, $line: Int!, $side: DiffSide!, $startLine: Int, $startSide: DiffSide) {
  addPullRequestReviewThread(input: {pullRequestId: $pullRequestId, path: $path, body: $body, line: $line, side: $side, startLine: $startLine, startSide: $startSide, subjectType: LINE}) {
    thread { ...ThreadFields }
  }
}
"#;

const REPLY_MUTATION: &str = r#"
mutation AddPullRequestReviewThreadReply($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $threadId, body: $body}) {
    comment { ...CommentFields }
  }
}
"#;

const UPDATE_COMMENT_MUTATION: &str = r#"
mutation UpdatePullRequestReviewComment($commentId: ID!, $body: String!) {
  updatePullRequestReviewComment(input: {pullRequestReviewCommentId: $commentId, body: $body}) {
    pullRequestReviewComment { id }
  }
}
"#;

const DELETE_COMMENT_MUTATION: &str = r#"
mutation DeletePullRequestReviewComment($commentId: ID!) {
  deletePullRequestReviewComment(input: {id: $commentId}) {
    pullRequestReview { id }
  }
}
"#;

const RESOLVE_THREAD_MUTATION: &str = r#"
mutation ResolveReviewThread($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) { thread { id } }
}
"#;

const UNRESOLVE_THREAD_MUTATION: &str = r#"
mutation UnresolveReviewThread($threadId: ID!) {
  unresolveReviewThread(input: {threadId: $threadId}) { thread { id } }
}
"#;

const SUBMIT_REVIEW_MUTATION: &str = r#"
mutation SubmitPullRequestReview($reviewId: ID!, $event: PullRequestReviewEvent!, $body: String) {
  submitPullRequestReview(input: {pullRequestReviewId: $reviewId, event: $event, body: $body}) {
    pullRequestReview { id }
  }
}
"#;

const ADD_REVIEW_MUTATION: &str = r#"
mutation AddPullRequestReview($pullRequestId: ID!, $event: PullRequestReviewEvent!, $body: String) {
  addPullRequestReview(input: {pullRequestId: $pullRequestId, event: $event, body: $body}) {
    pullRequestReview { id }
  }
}
"#;

const DELETE_REVIEW_MUTATION: &str = r#"
mutation DeletePullRequestReview($reviewId: ID!) {
  deletePullRequestReview(input: {pullRequestReviewId: $reviewId}) {
    pullRequestReview { id }
  }
}
"#;

fn details_query() -> String {
    with_fragments(DETAILS_QUERY)
}

fn with_fragments(operation: &str) -> String {
    let mut query = operation.to_string();
    if operation.contains("...ThreadFields") {
        query.push_str(THREAD_FRAGMENT);
    }
    if operation.contains("...ThreadFields") || operation.contains("...CommentFields") {
        query.push_str(COMMENT_FRAGMENT);
    }
    query
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_parse_github_remote() {
        let expected = Some(GitHubRepository {
            owner: "zed-industries".into(),
            name: "zed".into(),
            remote_name: "origin".into(),
        });
        for url in [
            "git@github.com:zed-industries/zed.git",
            "git@github.com:zed-industries/zed",
            "https://github.com/zed-industries/zed.git",
            "https://github.com/zed-industries/zed",
            "ssh://git@github.com/zed-industries/zed.git",
        ] {
            assert_eq!(GitHubRepository::parse("origin", url), expected, "{url}");
        }
        for url in [
            "git@gitlab.com:zed-industries/zed.git",
            "https://github.com/zed-industries",
            "https://github.com/a/b/c",
        ] {
            assert_eq!(GitHubRepository::parse("origin", url), None, "{url}");
        }
    }

    #[test]
    fn test_next_page_url() {
        assert_eq!(
            next_page_url(
                r#"<https://api.github.com/repositories/1/pulls/2/files?page=2>; rel="next", <https://api.github.com/repositories/1/pulls/2/files?page=5>; rel="last""#
            )
            .as_deref(),
            Some("https://api.github.com/repositories/1/pulls/2/files?page=2")
        );
        assert_eq!(
            next_page_url(r#"<https://api.github.com/x?page=1>; rel="prev""#),
            None
        );
    }

    #[test]
    fn test_search_query() {
        let repository = GitHubRepository {
            owner: "owner".into(),
            name: "repo".into(),
            remote_name: "origin".into(),
        };
        assert_eq!(
            PullRequestFilter::ReviewRequested.search_query(&repository),
            "repo:owner/repo is:pr is:open sort:updated-desc review-requested:@me"
        );
        assert_eq!(
            PullRequestFilter::AllOpen.search_query(&repository),
            "repo:owner/repo is:pr is:open sort:updated-desc"
        );
    }

    #[test]
    fn test_with_fragments() {
        let query = with_fragments(REPLY_MUTATION);
        assert!(query.contains("fragment CommentFields"));
        assert!(!query.contains("fragment ThreadFields"));
        let query = details_query();
        assert!(query.contains("fragment CommentFields"));
        assert!(query.contains("fragment ThreadFields"));
    }

    #[test]
    fn test_deserialize_thread() {
        let thread = serde_json::from_value::<ReviewThread>(json!({
            "id": "PRRT_1",
            "isResolved": false,
            "isOutdated": false,
            "path": "alpha.txt",
            "diffSide": "RIGHT",
            "startDiffSide": null,
            "line": 13,
            "startLine": 13,
            "originalLine": 11,
            "originalStartLine": null,
            "subjectType": "LINE",
            "viewerCanResolve": true,
            "viewerCanUnresolve": false,
            "viewerCanReply": true,
            "comments": { "nodes": [{
                "id": "PRRC_1",
                "body": "right side line 11",
                "author": { "login": "dmitryshur" },
                "createdAt": "2026-10-09T21:15:00Z",
                "state": "PENDING",
                "viewerDidAuthor": true,
                "viewerCanUpdate": true,
                "viewerCanDelete": true,
                "diffHunk": "@@ -7,9 +7,9 @@ line 6 of alpha",
                "originalCommit": { "oid": "ea6f52069f09a6a1051261554041c9f28c223c8d" }
            }]}
        }))
        .unwrap();
        assert_eq!(thread.diff_side, DiffSide::Right);
        assert_eq!(thread.line, Some(13));
        assert_eq!(thread.comments.len(), 1);
        assert!(thread.comments[0].is_pending());
        assert_eq!(thread.comments[0].author.as_deref(), Some("dmitryshur"));
        assert_eq!(
            thread.comments[0].original_commit,
            Some("ea6f52069f09a6a1051261554041c9f28c223c8d".parse().unwrap())
        );
    }
}
