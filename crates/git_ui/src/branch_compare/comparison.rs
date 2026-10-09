use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use buffer_diff::BufferDiff;
use collections::HashMap;
use file_content::decode_text;
use git::{
    Oid,
    repository::{
        Branch, CommitDetails, CommitFileStatus, GitRepository, RepoPath, is_binary_content,
    },
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus},
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription,
    Task, WeakEntity, Window,
};
use language::{Buffer, Capability};
use project::{
    Project,
    git_store::{
        CommitDiff, CommitFile, LocalRepositoryState, Repository, RepositoryEvent, RepositoryState,
    },
};
use util::{ResultExt as _, paths::PathStyle};

use crate::{
    commit_view::{GitBlob, build_buffer, build_buffer_diff, worktree_id_for_repo_path},
    pull_request::{PullRequestReview, PullRequestReviewEvent, ReviewFileContext, ReviewRange},
};

pub(crate) const REMOTE_NOT_SUPPORTED: &str =
    "Comparing branches is not supported for remote projects";

const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Reads git objects of a local repository directly. These reads are content-addressed, so they
/// don't need to go through the repository's serial job queue.
#[derive(Clone)]
pub(crate) struct LocalGitObjects {
    backend: Arc<dyn GitRepository>,
}

pub(crate) enum FileText {
    Text(String),
    Binary,
}

pub(crate) struct FileTexts {
    pub(crate) old: Option<FileText>,
    pub(crate) new: Option<FileText>,
}

impl LocalGitObjects {
    pub(crate) fn connect(repository: &Entity<Repository>, cx: &mut App) -> Task<Result<Self>> {
        let backend = repository.update(cx, |repository, _| {
            repository.send_job("connect branch comparison", None, |state, _| async move {
                match state {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => Ok(backend),
                    RepositoryState::Remote(_) => Err(anyhow!(REMOTE_NOT_SUPPORTED)),
                }
            })
        });
        cx.background_spawn(async move {
            Ok(Self {
                backend: backend.await??,
            })
        })
    }

    async fn resolve_commits(&self, branches: [&Branch; 2]) -> Result<[Oid; 2]> {
        let shas = self
            .backend
            .revparse_batch(
                branches
                    .iter()
                    .map(|branch| branch.ref_name.to_string())
                    .collect(),
            )
            .await?;
        let mut commits = Vec::with_capacity(branches.len());
        for (branch, sha) in branches.iter().zip(shas) {
            let sha =
                sha.with_context(|| format!("Branch “{}” no longer exists", branch.name()))?;
            commits.push(sha.parse::<Oid>()?);
        }
        commits
            .try_into()
            .map_err(|_| anyhow!("unexpected number of resolved commits"))
    }

    /// The text of a file at a commit, or `None` when it doesn't exist there or is binary.
    pub(crate) async fn load_text(&self, commit: Oid, path: &RepoPath) -> Result<Option<String>> {
        let contents = self
            .backend
            .load_revisions(vec![format!("{commit}:{}", path.as_unix_str())])
            .await?
            .into_iter()
            .next()
            .flatten();
        Ok(match contents.map(decode_file_text).transpose()? {
            Some(FileText::Text(text)) => Some(text),
            Some(FileText::Binary) | None => None,
        })
    }

    async fn diff_tree(&self, request: DiffTreeType) -> Result<TreeDiff> {
        self.backend.diff_tree(request).await
    }

    async fn load_texts(
        &self,
        old_blob: Option<Oid>,
        new_revision: Option<String>,
    ) -> Result<FileTexts> {
        let old = match old_blob {
            Some(oid) => Some(decode_file_text(
                self.backend.load_blob_content(oid).await?,
            )?),
            None => None,
        };
        let new = match new_revision {
            Some(revision) => {
                let contents = self
                    .backend
                    .load_revisions(vec![revision.clone()])
                    .await?
                    .into_iter()
                    .next()
                    .flatten()
                    .with_context(|| format!("{revision} does not exist"))?;
                Some(decode_file_text(contents)?)
            }
            None => None,
        };
        Ok(FileTexts { old, new })
    }
}

fn decode_file_text(bytes: Vec<u8>) -> Result<FileText> {
    let text = decode_text(bytes)?.text;
    Ok(if is_binary_content(text.as_bytes()) {
        FileText::Binary
    } else {
        FileText::Text(text)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComparisonMode {
    /// The compared branch is checked out, so its side is the working tree.
    WorkingTree,
    Committed,
}

pub(crate) enum ComparisonState {
    Loading,
    Loaded(Arc<[ComparisonEntry]>),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComparisonEntry {
    pub(crate) repo_path: RepoPath,
    pub(crate) status: FileStatus,
    pub(crate) old_side: OldSide,
    /// The path the old side had, for renamed files.
    pub(crate) old_path: Option<RepoPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OldSide {
    Absent,
    Blob(Oid),
    /// No old blob ID is available for conflicted paths or preloaded commit snapshots.
    Unknown,
}

pub(crate) struct CompareLocation {
    pub(crate) repo_path: RepoPath,
    pub(crate) row: u32,
}

enum ComparisonSource {
    Branches {
        base: Branch,
        compared: Branch,
    },
    Commit {
        sha: Oid,
        title: SharedString,
        files: HashMap<RepoPath, Arc<CommitFile>>,
    },
    PullRequest {
        review: Entity<PullRequestReview>,
        range: ReviewRange,
        /// The base and head the pull request had at the last refresh.
        heads: (Oid, Oid),
        title: SharedString,
    },
}

/// What a pull request comparison diffs: `base` and `compared` identify the loaded state.
struct PullRequestDiffPlan {
    mode: ComparisonMode,
    base: Oid,
    compared: Oid,
    request: DiffTreeType,
}

fn pull_request_diff_plan(
    range: ReviewRange,
    base_ref: Oid,
    remote_head: Oid,
    local_head: Option<Oid>,
    on_branch: bool,
) -> PullRequestDiffPlan {
    let working_tree = |base: Oid, local_head: Oid| PullRequestDiffPlan {
        mode: ComparisonMode::WorkingTree,
        base,
        compared: local_head,
        request: DiffTreeType::MergeBaseWithWorktree {
            base: base.to_string().into(),
        },
    };
    let local_head = local_head.filter(|_| on_branch);
    match (range, local_head) {
        (ReviewRange::All, Some(local_head)) => working_tree(base_ref, local_head),
        (ReviewRange::All, None) => PullRequestDiffPlan {
            mode: ComparisonMode::Committed,
            base: base_ref,
            compared: remote_head,
            request: DiffTreeType::MergeBase {
                base: base_ref.to_string().into(),
                head: remote_head.to_string().into(),
            },
        },
        (ReviewRange::SinceLastReview(reviewed), Some(local_head)) => {
            working_tree(reviewed, local_head)
        }
        (ReviewRange::SinceLastReview(reviewed), None) => PullRequestDiffPlan {
            mode: ComparisonMode::Committed,
            base: reviewed,
            compared: remote_head,
            request: DiffTreeType::Since {
                base: reviewed.to_string().into(),
                head: remote_head.to_string().into(),
            },
        },
        (ReviewRange::Commit { sha, parent }, Some(local_head)) if local_head == sha => {
            working_tree(parent, local_head)
        }
        (ReviewRange::Commit { sha, parent }, _) => PullRequestDiffPlan {
            mode: ComparisonMode::Committed,
            base: parent,
            compared: sha,
            request: DiffTreeType::Since {
                base: parent.to_string().into(),
                head: sha.to_string().into(),
            },
        },
    }
}

pub(crate) enum ComparisonEvent {
    EntriesChanged,
    ModeChanged,
}

pub(crate) struct LoadedCompareFile {
    pub(crate) repo_path: RepoPath,
    pub(crate) buffer: Entity<Buffer>,
    pub(crate) diff: Entity<BufferDiff>,
    pub(crate) read_only: bool,
}

/// The files changed on one branch since it split off from another (`git diff base...compared`).
pub(crate) struct BranchComparison {
    project: Entity<Project>,
    repository: Entity<Repository>,
    source: ComparisonSource,
    git_objects: Option<LocalGitObjects>,
    mode: ComparisonMode,
    state: ComparisonState,
    /// The mode and commits the loaded entries were computed for.
    loaded_for: Option<(ComparisonMode, [Oid; 2])>,
    refresh_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComparisonEvent> for BranchComparison {}

impl BranchComparison {
    pub(crate) fn new(
        project: Entity<Project>,
        repository: Entity<Repository>,
        base: Branch,
        compared: Branch,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_subscription = cx.subscribe(
            &repository,
            |this, _, event: &RepositoryEvent, cx| match event {
                RepositoryEvent::StatusesChanged => {
                    if this.mode == ComparisonMode::WorkingTree {
                        this.schedule_refresh(REFRESH_DEBOUNCE, cx);
                    }
                }
                RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                    this.schedule_refresh(REFRESH_DEBOUNCE, cx);
                }
                _ => {}
            },
        );
        let mode = Self::mode_for(&repository, &compared, cx);
        let mut this = Self {
            project,
            repository,
            source: ComparisonSource::Branches { base, compared },
            git_objects: None,
            mode,
            state: ComparisonState::Loading,
            loaded_for: None,
            refresh_task: Task::ready(()),
            _subscriptions: vec![repository_subscription],
        };
        this.schedule_refresh(Duration::ZERO, cx);
        this
    }

    pub(crate) fn for_commit(
        project: Entity<Project>,
        repository: Entity<Repository>,
        sha: Oid,
        details: CommitDetails,
        diff: CommitDiff,
    ) -> Self {
        let title = format!(
            "{}: {}",
            sha.display_short(),
            details.message.lines().next().unwrap_or_default()
        )
        .into();
        let mut entries = Vec::with_capacity(diff.files.len());
        let files = diff
            .files
            .into_iter()
            .map(|file| {
                let status_code = match file.status() {
                    CommitFileStatus::Added => StatusCode::Added,
                    CommitFileStatus::Modified => StatusCode::Modified,
                    CommitFileStatus::Deleted => StatusCode::Deleted,
                };
                entries.push(ComparisonEntry {
                    repo_path: file.path.clone(),
                    status: FileStatus::Tracked(TrackedStatus {
                        index_status: status_code,
                        worktree_status: status_code,
                    }),
                    old_side: if file.old_text.is_some() {
                        OldSide::Unknown
                    } else {
                        OldSide::Absent
                    },
                    old_path: None,
                });
                (file.path.clone(), Arc::new(file))
            })
            .collect();
        entries.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
        Self {
            project,
            repository,
            source: ComparisonSource::Commit { sha, title, files },
            git_objects: None,
            mode: ComparisonMode::Committed,
            state: ComparisonState::Loaded(entries.into()),
            loaded_for: None,
            refresh_task: Task::ready(()),
            _subscriptions: Vec::new(),
        }
    }

    pub(crate) fn for_pull_request(
        project: Entity<Project>,
        repository: Entity<Repository>,
        review: Entity<PullRequestReview>,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_subscription = cx.subscribe(
            &repository,
            |this, _, event: &RepositoryEvent, cx| match event {
                RepositoryEvent::StatusesChanged => {
                    if this.mode == ComparisonMode::WorkingTree {
                        this.schedule_refresh(REFRESH_DEBOUNCE, cx);
                    }
                }
                RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                    this.schedule_refresh(REFRESH_DEBOUNCE, cx);
                }
                _ => {}
            },
        );
        let review_subscription =
            cx.subscribe(&review, |this, review, _: &PullRequestReviewEvent, cx| {
                let details = review.read(cx).details();
                let new_heads = (details.base_ref_oid, details.head_ref_oid);
                let new_title = pull_request_title(details.number, &details.title);
                if let ComparisonSource::PullRequest { heads, title, .. } = &mut this.source {
                    *title = new_title;
                    if *heads != new_heads {
                        *heads = new_heads;
                        this.schedule_refresh(Duration::ZERO, cx);
                    }
                }
            });
        let details = review.read(cx).details();
        let heads = (details.base_ref_oid, details.head_ref_oid);
        let title = pull_request_title(details.number, &details.title);
        let mut this = Self {
            project,
            repository,
            source: ComparisonSource::PullRequest {
                review,
                range: ReviewRange::All,
                heads,
                title,
            },
            git_objects: None,
            mode: ComparisonMode::Committed,
            state: ComparisonState::Loading,
            loaded_for: None,
            refresh_task: Task::ready(()),
            _subscriptions: vec![repository_subscription, review_subscription],
        };
        this.schedule_refresh(Duration::ZERO, cx);
        this
    }

    pub(crate) fn pull_request_review(&self) -> Option<&Entity<PullRequestReview>> {
        match &self.source {
            ComparisonSource::PullRequest { review, .. } => Some(review),
            ComparisonSource::Branches { .. } | ComparisonSource::Commit { .. } => None,
        }
    }

    pub(crate) fn review_range(&self) -> Option<ReviewRange> {
        match &self.source {
            ComparisonSource::PullRequest { range, .. } => Some(*range),
            ComparisonSource::Branches { .. } | ComparisonSource::Commit { .. } => None,
        }
    }

    pub(crate) fn set_review_range(&mut self, new_range: ReviewRange, cx: &mut Context<Self>) {
        if let ComparisonSource::PullRequest { range, .. } = &mut self.source
            && *range != new_range
        {
            *range = new_range;
            self.state = ComparisonState::Loading;
            self.loaded_for = None;
            cx.emit(ComparisonEvent::EntriesChanged);
            self.schedule_refresh(Duration::ZERO, cx);
            cx.notify();
        }
    }

    /// What the review comments of a file shown from this comparison attach to.
    pub(crate) fn review_file_context(&self, repo_path: &RepoPath) -> Option<ReviewFileContext> {
        let ComparisonSource::PullRequest { review, range, .. } = &self.source else {
            return None;
        };
        Some(ReviewFileContext {
            review: review.clone(),
            repo_path: repo_path.clone(),
            range: *range,
            view_commit: match self.mode {
                ComparisonMode::WorkingTree => None,
                ComparisonMode::Committed => self.compared_commit(),
            },
        })
    }

    pub(crate) fn title(&self) -> SharedString {
        match &self.source {
            ComparisonSource::Branches { base, compared } => {
                format!("{} since {}", compared.name(), base.name()).into()
            }
            ComparisonSource::Commit { title, .. } => title.clone(),
            ComparisonSource::PullRequest { title, .. } => title.clone(),
        }
    }

    pub(crate) fn state(&self) -> &ComparisonState {
        &self.state
    }

    pub(crate) fn mode(&self) -> ComparisonMode {
        self.mode
    }

    pub(crate) fn compared_commit(&self) -> Option<Oid> {
        match &self.source {
            ComparisonSource::Branches { .. } | ComparisonSource::PullRequest { .. } => self
                .loaded_for
                .map(|(_, [_, compared_commit])| compared_commit),
            ComparisonSource::Commit { sha, .. } => Some(*sha),
        }
    }

    fn mode_for(repository: &Entity<Repository>, compared: &Branch, cx: &App) -> ComparisonMode {
        let is_checked_out = repository
            .read(cx)
            .branch
            .as_ref()
            .is_some_and(|branch| branch.ref_name == compared.ref_name);
        if is_checked_out {
            ComparisonMode::WorkingTree
        } else {
            ComparisonMode::Committed
        }
    }

    fn schedule_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.refresh_task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            if let Err(error) = Self::refresh(this.clone(), cx).await {
                this.update(cx, |this, cx| {
                    let message = match &this.source {
                        ComparisonSource::Branches { base, compared } => {
                            friendly_error(&error, base, compared)
                        }
                        ComparisonSource::Commit { .. } | ComparisonSource::PullRequest { .. } => {
                            format!("{error:#}").into()
                        }
                    };
                    this.state = ComparisonState::Failed(message);
                    this.loaded_for = None;
                    cx.emit(ComparisonEvent::EntriesChanged);
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        enum Target {
            Branches(Branch, Branch),
            PullRequest(PullRequestDiffPlan, HashMap<RepoPath, RepoPath>),
        }
        let Some((git_objects, target)) = this.update(cx, |this, cx| {
            let target = match &this.source {
                ComparisonSource::Branches { base, compared } => {
                    Target::Branches(base.clone(), compared.clone())
                }
                ComparisonSource::PullRequest { review, range, .. } => {
                    let review = review.read(cx);
                    let details = review.details();
                    let plan = pull_request_diff_plan(
                        *range,
                        details.base_ref_oid,
                        details.head_ref_oid,
                        review.local_head(cx),
                        review.is_on_branch(cx),
                    );
                    let renames = match range {
                        ReviewRange::All => review.renames(),
                        ReviewRange::SinceLastReview(_) | ReviewRange::Commit { .. } => {
                            HashMap::default()
                        }
                    };
                    Target::PullRequest(plan, renames)
                }
                ComparisonSource::Commit { .. } => return None,
            };
            Some((this.git_objects.clone(), target))
        })?
        else {
            return Ok(());
        };
        let git_objects = match git_objects {
            Some(git_objects) => git_objects,
            None => {
                let connect = this.update(cx, |this, cx| {
                    LocalGitObjects::connect(&this.repository, cx)
                })?;
                let git_objects = connect.await?;
                this.update(cx, |this, _| this.git_objects = Some(git_objects.clone()))?;
                git_objects
            }
        };

        let (mode, commits, request, renames) = match target {
            Target::Branches(base, compared) => {
                let commits = git_objects.resolve_commits([&base, &compared]).await?;
                let [base_commit, compared_commit] = commits;
                let mode = this.update(cx, |this, cx| {
                    Self::mode_for(&this.repository, &compared, cx)
                })?;
                let request = match mode {
                    ComparisonMode::Committed => DiffTreeType::MergeBase {
                        base: base_commit.to_string().into(),
                        head: compared_commit.to_string().into(),
                    },
                    ComparisonMode::WorkingTree => DiffTreeType::MergeBaseWithWorktree {
                        base: base_commit.to_string().into(),
                    },
                };
                (mode, commits, request, HashMap::default())
            }
            Target::PullRequest(plan, renames) => {
                (plan.mode, [plan.base, plan.compared], plan.request, renames)
            }
        };
        let (mode, already_loaded) = this.update(cx, |this, cx| {
            if mode != this.mode {
                this.mode = mode;
                cx.emit(ComparisonEvent::ModeChanged);
            }
            let already_loaded = mode == ComparisonMode::Committed
                && this.loaded_for == Some((mode, commits))
                && matches!(this.state, ComparisonState::Loaded(_));
            (mode, already_loaded)
        })?;
        if already_loaded {
            return Ok(());
        }

        let tree_diff = git_objects.diff_tree(request).await?;

        this.update(cx, |this, cx| {
            let statuses = match mode {
                ComparisonMode::WorkingTree => this
                    .repository
                    .read(cx)
                    .cached_status()
                    .map(|entry| (entry.repo_path, entry.status))
                    .collect(),
                ComparisonMode::Committed => Vec::new(),
            };
            this.state =
                ComparisonState::Loaded(build_entries(tree_diff, statuses, &renames).into());
            this.loaded_for = Some((mode, commits));
            cx.emit(ComparisonEvent::EntriesChanged);
            cx.notify();
        })
    }

    pub(crate) fn load_file(
        &self,
        entry: &ComparisonEntry,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<LoadedCompareFile>> {
        match self.mode {
            ComparisonMode::WorkingTree => self.load_working_tree_file(entry, window, cx),
            ComparisonMode::Committed => self.load_committed_file(entry, window, cx),
        }
    }

    fn load_working_tree_file(
        &self,
        entry: &ComparisonEntry,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<LoadedCompareFile>> {
        let repo_path = entry.repo_path.clone();
        let Some(project_path) = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
        else {
            return Task::ready(Err(anyhow!(
                "{} is outside of the project",
                repo_path.display(PathStyle::local())
            )));
        };
        let project = self.project.clone();
        let repository = self.repository.clone();
        let old_side = entry.old_side;
        window.spawn(cx, async move |cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            let diff = match old_side {
                OldSide::Blob(oid) => {
                    let git_store = project.read_with(cx, |project, _| project.git_store().clone());
                    git_store
                        .update(cx, |git_store, cx| {
                            git_store.open_diff_since(Some(oid), buffer.clone(), repository, cx)
                        })
                        .await?
                }
                OldSide::Absent => {
                    let git_store = project.read_with(cx, |project, _| project.git_store().clone());
                    git_store
                        .update(cx, |git_store, cx| {
                            git_store.open_diff_since(None, buffer.clone(), repository, cx)
                        })
                        .await?
                }
                OldSide::Unknown => {
                    project
                        .update(cx, |project, cx| {
                            project.open_uncommitted_diff(buffer.clone(), cx)
                        })
                        .await?
                }
            };
            Ok(LoadedCompareFile {
                repo_path,
                buffer,
                diff,
                read_only: false,
            })
        })
    }

    fn load_committed_file(
        &self,
        entry: &ComparisonEntry,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<LoadedCompareFile>> {
        let repo_path = entry.repo_path.clone();
        let is_deleted = entry.status.is_deleted();
        let texts = match &self.source {
            ComparisonSource::Commit { files, .. } => {
                let Some(file) = files.get(&repo_path) else {
                    return Task::ready(Err(anyhow!("the file is not part of this commit")));
                };
                let text = |contents: &Option<String>| {
                    contents.as_ref().map(|contents| {
                        if file.is_binary {
                            FileText::Binary
                        } else {
                            FileText::Text(contents.clone())
                        }
                    })
                };
                Task::ready(Ok(FileTexts {
                    old: text(&file.old_text),
                    new: text(&file.new_text),
                }))
            }
            ComparisonSource::Branches { .. } | ComparisonSource::PullRequest { .. } => {
                let (Some(git_objects), Some(compared_commit)) =
                    (self.git_objects.clone(), self.compared_commit())
                else {
                    return Task::ready(Err(anyhow!("the comparison hasn't loaded yet")));
                };
                let old_blob = match entry.old_side {
                    OldSide::Blob(oid) => Some(oid),
                    OldSide::Absent | OldSide::Unknown => None,
                };
                let new_revision =
                    (!is_deleted).then(|| format!("{compared_commit}:{}", repo_path.as_unix_str()));
                cx.background_spawn(
                    async move { git_objects.load_texts(old_blob, new_revision).await },
                )
            }
        };
        let project = self.project.clone();
        let repository = self.repository.clone();
        let language_registry = project.read(cx).languages().clone();
        window.spawn(cx, async move |cx| {
            let texts = texts.await?;
            let is_binary = matches!(texts.old, Some(FileText::Binary))
                || matches!(texts.new, Some(FileText::Binary));
            let text = |file_text: Option<FileText>| match file_text {
                Some(FileText::Text(text)) => Some(text),
                Some(FileText::Binary) | None => None,
            };
            let (old_text, new_text) = if is_binary {
                (None, "(binary file not shown)".to_string())
            } else {
                (text(texts.old), text(texts.new).unwrap_or_default())
            };

            let worktree_id = repository
                .read_with(cx, |repository, cx| {
                    worktree_id_for_repo_path(repository, project.read(cx), &repo_path, cx)
                })
                .context("project has no worktrees")?;
            let display_name = repo_path
                .file_name()
                .map(ToString::to_string)
                .unwrap_or_else(|| repo_path.display(PathStyle::local()).to_string());
            let blob = Arc::new(GitBlob {
                path: repo_path.clone(),
                worktree_id,
                is_deleted,
                is_binary,
                display_name,
            }) as Arc<dyn language::File>;
            let buffer = build_buffer(new_text, blob, &language_registry, cx).await?;
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(Capability::ReadOnly, cx)
            });
            let diff = if is_binary {
                cx.update(|_, cx| {
                    let snapshot = buffer.read(cx).snapshot();
                    cx.new(|cx| {
                        BufferDiff::new_unchanged(
                            &snapshot,
                            snapshot.language().cloned(),
                            Some(language_registry.clone()),
                            cx,
                        )
                    })
                })?
            } else {
                build_buffer_diff(old_text, &buffer, &language_registry, cx).await?
            };
            Ok(LoadedCompareFile {
                repo_path,
                buffer,
                diff,
                read_only: true,
            })
        })
    }
}

fn pull_request_title(number: u64, title: &str) -> SharedString {
    format!("#{number} {title}").into()
}

fn tree_status_to_file_status(status: &TreeDiffStatus) -> FileStatus {
    let status_code = match status {
        TreeDiffStatus::Added => StatusCode::Added,
        TreeDiffStatus::Modified { .. } => StatusCode::Modified,
        TreeDiffStatus::Deleted { .. } => StatusCode::Deleted,
    };
    FileStatus::Tracked(TrackedStatus {
        index_status: status_code,
        worktree_status: status_code,
    })
}

/// Combines the tree diff with working-tree statuses (empty for committed comparisons):
/// untracked files show as added, and conflicted files keep their conflict status.
fn build_entries(
    tree_diff: TreeDiff,
    statuses: Vec<(RepoPath, FileStatus)>,
    renames: &HashMap<RepoPath, RepoPath>,
) -> Vec<ComparisonEntry> {
    let mut entries = tree_diff
        .entries
        .into_iter()
        .map(|(repo_path, tree_status)| {
            let old_side = match tree_status {
                TreeDiffStatus::Added => OldSide::Absent,
                TreeDiffStatus::Modified { old } | TreeDiffStatus::Deleted { old } => {
                    OldSide::Blob(old)
                }
            };
            let entry = ComparisonEntry {
                repo_path: repo_path.clone(),
                status: tree_status_to_file_status(&tree_status),
                old_side,
                old_path: None,
            };
            (repo_path, entry)
        })
        .collect::<HashMap<_, _>>();

    // Git diffs without rename detection here; take GitHub's renames so the paths and old-side
    // lines match the pull request's.
    for (new_path, old_path) in renames {
        let Some(OldSide::Blob(old_blob)) = entries
            .get(old_path)
            .filter(|entry| entry.status.is_deleted())
            .map(|entry| entry.old_side)
        else {
            continue;
        };
        if let Some(entry) = entries.get_mut(new_path)
            && entry.old_side == OldSide::Absent
        {
            entry.old_side = OldSide::Blob(old_blob);
            entry.status = tree_status_to_file_status(&TreeDiffStatus::Modified { old: old_blob });
            entry.old_path = Some(old_path.clone());
            entries.remove(old_path);
        }
    }

    for (repo_path, status) in statuses {
        if status.is_conflicted() {
            entries
                .entry(repo_path.clone())
                .and_modify(|entry| entry.status = status)
                .or_insert(ComparisonEntry {
                    repo_path,
                    status,
                    old_side: OldSide::Unknown,
                    old_path: None,
                });
        } else if status.is_untracked() {
            entries.entry(repo_path.clone()).or_insert(ComparisonEntry {
                repo_path,
                status: tree_status_to_file_status(&TreeDiffStatus::Added),
                old_side: OldSide::Absent,
                old_path: None,
            });
        }
    }

    let mut entries = entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
    entries
}

/// Git's merge-base errors only carry its stderr; recognize the common ones.
fn friendly_error(error: &anyhow::Error, base: &Branch, compared: &Branch) -> SharedString {
    let message = format!("{error:#}");
    if message.contains("no merge base") {
        format!(
            "“{}” and “{}” have no common ancestor",
            compared.name(),
            base.name()
        )
        .into()
    } else if message.contains("multiple merge bases") {
        format!(
            "“{}” and “{}” have multiple merge bases",
            compared.name(),
            base.name()
        )
        .into()
    } else {
        message.into()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    const MAIN_SHA: &str = "1111111111111111111111111111111111111111";
    pub(crate) const FEATURE_SHA: &str = "2222222222222222222222222222222222222222";

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).unwrap()
    }

    pub(crate) fn branch(ref_name: &str) -> Branch {
        Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: None,
        }
    }

    fn status(status_code: StatusCode) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: status_code,
        })
    }

    #[test]
    fn test_build_entries() {
        let tree_diff = TreeDiff {
            entries: [
                (
                    repo_path("b/modified.rs"),
                    TreeDiffStatus::Modified { old: oid(1) },
                ),
                (repo_path("a/added.rs"), TreeDiffStatus::Added),
                (
                    repo_path("c/deleted.rs"),
                    TreeDiffStatus::Deleted { old: oid(2) },
                ),
                (
                    repo_path("d/conflicted.rs"),
                    TreeDiffStatus::Modified { old: oid(3) },
                ),
                (
                    repo_path("e/deleted_but_untracked.rs"),
                    TreeDiffStatus::Deleted { old: oid(4) },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let conflict = FileStatus::Unmerged(git::status::UnmergedStatus {
            first_head: git::status::UnmergedStatusCode::Updated,
            second_head: git::status::UnmergedStatusCode::Updated,
        });
        let entries = build_entries(
            tree_diff,
            vec![
                (repo_path("d/conflicted.rs"), conflict),
                (repo_path("f/conflicted_only.rs"), conflict),
                (
                    repo_path("e/deleted_but_untracked.rs"),
                    FileStatus::Untracked,
                ),
                (repo_path("g/untracked.rs"), FileStatus::Untracked),
                (repo_path("b/modified.rs"), status(StatusCode::Modified)),
            ],
            &HashMap::default(),
        );
        assert_eq!(
            entries,
            vec![
                ComparisonEntry {
                    repo_path: repo_path("a/added.rs"),
                    status: status(StatusCode::Added),
                    old_side: OldSide::Absent,
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("b/modified.rs"),
                    status: status(StatusCode::Modified),
                    old_side: OldSide::Blob(oid(1)),
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("c/deleted.rs"),
                    status: status(StatusCode::Deleted),
                    old_side: OldSide::Blob(oid(2)),
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("d/conflicted.rs"),
                    status: conflict,
                    old_side: OldSide::Blob(oid(3)),
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("e/deleted_but_untracked.rs"),
                    status: status(StatusCode::Deleted),
                    old_side: OldSide::Blob(oid(4)),
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("f/conflicted_only.rs"),
                    status: conflict,
                    old_side: OldSide::Unknown,
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("g/untracked.rs"),
                    status: status(StatusCode::Added),
                    old_side: OldSide::Absent,
                    old_path: None,
                },
            ]
        );
    }

    #[test]
    fn test_pull_request_diff_plan() {
        let base = oid(1);
        let remote_head = oid(2);
        let local_head = oid(3);
        let reviewed = oid(4);
        let describe = |plan: PullRequestDiffPlan| {
            let request = match plan.request {
                DiffTreeType::MergeBase { base, head } => format!("{base}...{head}"),
                DiffTreeType::MergeBaseWithWorktree { base } => format!("{base}...worktree"),
                DiffTreeType::Since { base, head } => format!("{base}..{head}"),
            };
            (plan.mode, plan.base, plan.compared, request)
        };
        let short = |oid: Oid| oid.to_string();
        assert_eq!(
            describe(pull_request_diff_plan(
                ReviewRange::All,
                base,
                remote_head,
                Some(local_head),
                true
            )),
            (
                ComparisonMode::WorkingTree,
                base,
                local_head,
                format!("{}...worktree", short(base))
            )
        );
        assert_eq!(
            describe(pull_request_diff_plan(
                ReviewRange::All,
                base,
                remote_head,
                Some(local_head),
                false
            )),
            (
                ComparisonMode::Committed,
                base,
                remote_head,
                format!("{}...{}", short(base), short(remote_head))
            ),
            "off the branch, the pull request's own head is shown"
        );
        assert_eq!(
            describe(pull_request_diff_plan(
                ReviewRange::SinceLastReview(reviewed),
                base,
                remote_head,
                Some(local_head),
                true
            )),
            (
                ComparisonMode::WorkingTree,
                reviewed,
                local_head,
                format!("{}...worktree", short(reviewed))
            )
        );
        assert_eq!(
            describe(pull_request_diff_plan(
                ReviewRange::Commit {
                    sha: local_head,
                    parent: reviewed
                },
                base,
                remote_head,
                Some(local_head),
                true
            )),
            (
                ComparisonMode::WorkingTree,
                reviewed,
                local_head,
                format!("{}...worktree", short(reviewed))
            ),
            "the checked-out commit stays editable"
        );
        assert_eq!(
            describe(pull_request_diff_plan(
                ReviewRange::Commit {
                    sha: reviewed,
                    parent: base
                },
                base,
                remote_head,
                Some(local_head),
                true
            )),
            (
                ComparisonMode::Committed,
                base,
                reviewed,
                format!("{}..{}", short(base), short(reviewed))
            )
        );
    }

    #[test]
    fn test_build_entries_with_renames() {
        let tree_diff = TreeDiff {
            entries: [
                (repo_path("old.rs"), TreeDiffStatus::Deleted { old: oid(1) }),
                (repo_path("new.rs"), TreeDiffStatus::Added),
                (
                    repo_path("gone.rs"),
                    TreeDiffStatus::Deleted { old: oid(2) },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let renames = [(repo_path("new.rs"), repo_path("old.rs"))]
            .into_iter()
            .collect();
        assert_eq!(
            build_entries(tree_diff, Vec::new(), &renames),
            vec![
                ComparisonEntry {
                    repo_path: repo_path("gone.rs"),
                    status: status(StatusCode::Deleted),
                    old_side: OldSide::Blob(oid(2)),
                    old_path: None,
                },
                ComparisonEntry {
                    repo_path: repo_path("new.rs"),
                    status: status(StatusCode::Modified),
                    old_side: OldSide::Blob(oid(1)),
                    old_path: Some(repo_path("old.rs")),
                },
            ]
        );
    }

    #[test]
    fn test_friendly_error() {
        let base = branch("refs/heads/main");
        let compared = branch("refs/remotes/origin/feature");
        assert_eq!(
            friendly_error(
                &anyhow!("git diff-tree failed: fatal: main...feature: no merge base"),
                &base,
                &compared
            ),
            "“origin/feature” and “main” have no common ancestor"
        );
        assert_eq!(
            friendly_error(&anyhow!("something else"), &base, &compared),
            "something else"
        );
    }

    pub(crate) fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    pub(crate) async fn setup(
        current_branch: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>, Arc<FakeFs>) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "src": {
                    "changed.rs": "fn new() {}\n",
                    "added.rs": "fn added() {}\n",
                    "untracked.rs": "fn untracked() {}\n",
                },
            }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_merge_base_content_for_repo(
            dot_git,
            &[
                ("src/changed.rs", "fn old() {}\n".into()),
                ("src/deleted.rs", "fn deleted() {}\n".into()),
            ],
        );
        fs.set_head_for_repo(
            dot_git,
            &[
                ("src/changed.rs", "fn new() {}\n".into()),
                ("src/added.rs", "fn added() {}\n".into()),
            ],
            FEATURE_SHA,
        );
        fs.with_git_state(dot_git, true, |state| {
            state.refs.insert("refs/heads/main".into(), MAIN_SHA.into());
            state
                .refs
                .insert("refs/heads/feature".into(), FEATURE_SHA.into());
            state.branches.insert("main".into());
            state.branches.insert("feature".into());
        })
        .unwrap();
        fs.set_branch_name(dot_git, Some(current_branch));
        fs.set_status_for_repo(dot_git, &[("src/untracked.rs", FileStatus::Untracked)]);

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        (project, repository, fs)
    }

    fn entry_paths(
        comparison: &Entity<BranchComparison>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        comparison.read_with(cx, |comparison, _| match comparison.state() {
            ComparisonState::Loaded(entries) => entries
                .iter()
                .map(|entry| entry.repo_path.as_unix_str().to_string())
                .collect(),
            ComparisonState::Loading => vec!["<loading>".to_string()],
            ComparisonState::Failed(message) => vec![format!("<failed: {message}>")],
        })
    }

    #[gpui::test]
    async fn test_committed_comparison(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("main", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let comparison = cx.new(|cx| {
            BranchComparison::new(
                project.clone(),
                repository.clone(),
                branch("refs/heads/main"),
                branch("refs/heads/feature"),
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(
            entry_paths(&comparison, cx),
            ["src/added.rs", "src/changed.rs", "src/deleted.rs"],
            "untracked files only belong to working-tree comparisons"
        );
        comparison.read_with(cx, |comparison, _| {
            assert_eq!(comparison.mode, ComparisonMode::Committed);
            assert_eq!(comparison.title(), "feature since main");
        });

        let entries = comparison.read_with(cx, |comparison, _| match comparison.state() {
            ComparisonState::Loaded(entries) => entries.clone(),
            _ => panic!("comparison should be loaded"),
        });
        for (entry, expected_new_text, expected_old_text) in [
            (&entries[0], "fn added() {}\n", None),
            (&entries[1], "fn new() {}\n", Some("fn old() {}\n")),
            (&entries[2], "", Some("fn deleted() {}\n")),
        ] {
            let file = comparison
                .update_in(cx, |comparison, window, cx| {
                    comparison.load_file(entry, window, cx)
                })
                .await
                .unwrap();
            cx.run_until_parked();
            cx.update(|_, cx| {
                assert!(file.read_only);
                let buffer = file.buffer.read(cx);
                assert_eq!(buffer.text(), expected_new_text);
                assert_eq!(buffer.capability(), Capability::ReadOnly);
                assert_eq!(
                    file.diff.read(cx).base_text_string(cx).as_deref(),
                    expected_old_text,
                    "old text of {:?}",
                    entry.repo_path
                );
            });
        }

        fs.with_git_state(Path::new(path!("/project/.git")), true, |state| {
            state.refs.remove("refs/heads/feature");
        })
        .unwrap();
        comparison.update(cx, |comparison, cx| {
            comparison.schedule_refresh(Duration::ZERO, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            entry_paths(&comparison, cx),
            ["<failed: Branch “feature” no longer exists>"]
        );
    }

    #[gpui::test]
    async fn test_working_tree_comparison(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("feature", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let comparison = cx.new(|cx| {
            BranchComparison::new(
                project.clone(),
                repository.clone(),
                branch("refs/heads/main"),
                branch("refs/heads/feature"),
                cx,
            )
        });
        cx.run_until_parked();

        comparison.read_with(cx, |comparison, _| {
            assert_eq!(comparison.mode, ComparisonMode::WorkingTree);
        });
        assert_eq!(
            entry_paths(&comparison, cx),
            [
                "src/added.rs",
                "src/changed.rs",
                "src/deleted.rs",
                "src/untracked.rs"
            ]
        );

        let changed = comparison.read_with(cx, |comparison, _| match comparison.state() {
            ComparisonState::Loaded(entries) => entries[1].clone(),
            _ => panic!("comparison should be loaded"),
        });
        let file = comparison
            .update_in(cx, |comparison, window, cx| {
                comparison.load_file(&changed, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!file.read_only);
            assert_eq!(file.buffer.read(cx).capability(), Capability::ReadWrite);
            assert!(
                file.buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.disk_state().exists()),
                "the working-tree side is the project buffer"
            );
            assert_eq!(
                file.diff.read(cx).base_text_string(cx).as_deref(),
                Some("fn old() {}\n")
            );
        });

        fs.set_branch_name(Path::new(path!("/project/.git")), Some("main"));
        cx.run_until_parked();
        cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
        cx.run_until_parked();
        comparison.read_with(cx, |comparison, _| {
            assert_eq!(
                comparison.mode,
                ComparisonMode::Committed,
                "checking out another branch switches to comparing commits"
            );
        });
    }
}
