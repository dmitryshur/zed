use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use buffer_diff::BufferDiff;
use collections::HashMap;
use file_content::decode_text;
use git::{
    Oid,
    repository::{Branch, GitRepository, RepoPath, is_binary_content},
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus},
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription,
    Task, WeakEntity, Window,
};
use language::{Buffer, Capability};
use project::{
    Project,
    git_store::{LocalRepositoryState, Repository, RepositoryEvent, RepositoryState},
};
use util::{ResultExt as _, paths::PathStyle};

use crate::commit_view::{GitBlob, build_buffer, build_buffer_diff, worktree_id_for_repo_path};

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OldSide {
    Absent,
    Blob(Oid),
    /// `git diff --merge-base` skips conflicted paths, so their merge-base blob is unknown.
    Unknown,
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
    base: Branch,
    compared: Branch,
    git_objects: Option<LocalGitObjects>,
    mode: ComparisonMode,
    state: ComparisonState,
    /// The mode and commits the loaded entries were computed for.
    loaded_for: Option<(ComparisonMode, [Oid; 2])>,
    refresh_task: Task<()>,
    _repository_subscription: Subscription,
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
            base,
            compared,
            git_objects: None,
            mode,
            state: ComparisonState::Loading,
            loaded_for: None,
            refresh_task: Task::ready(()),
            _repository_subscription: repository_subscription,
        };
        this.schedule_refresh(Duration::ZERO, cx);
        this
    }

    pub(crate) fn title(&self) -> SharedString {
        format!("{} since {}", self.compared.name(), self.base.name()).into()
    }

    pub(crate) fn state(&self) -> &ComparisonState {
        &self.state
    }

    pub(crate) fn mode(&self) -> ComparisonMode {
        self.mode
    }

    pub(crate) fn compared_commit(&self) -> Option<Oid> {
        self.loaded_for
            .map(|(_, [_, compared_commit])| compared_commit)
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
                    this.state =
                        ComparisonState::Failed(friendly_error(&error, &this.base, &this.compared));
                    this.loaded_for = None;
                    cx.emit(ComparisonEvent::EntriesChanged);
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let (git_objects, base, compared) = this.update(cx, |this, _| {
            (
                this.git_objects.clone(),
                this.base.clone(),
                this.compared.clone(),
            )
        })?;
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

        let commits = git_objects.resolve_commits([&base, &compared]).await?;
        let [base_commit, compared_commit] = commits;
        let (mode, already_loaded) = this.update(cx, |this, cx| {
            let mode = Self::mode_for(&this.repository, &this.compared, cx);
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

        let request = match mode {
            ComparisonMode::Committed => DiffTreeType::MergeBase {
                base: base_commit.to_string().into(),
                head: compared_commit.to_string().into(),
            },
            ComparisonMode::WorkingTree => DiffTreeType::MergeBaseWithWorktree {
                base: base_commit.to_string().into(),
            },
        };
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
            this.state = ComparisonState::Loaded(build_entries(tree_diff, statuses).into());
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
        let (Some(git_objects), Some(compared_commit)) =
            (self.git_objects.clone(), self.compared_commit())
        else {
            return Task::ready(Err(anyhow!("the comparison hasn't loaded yet")));
        };
        let repo_path = entry.repo_path.clone();
        let is_deleted = entry.status.is_deleted();
        let old_blob = match entry.old_side {
            OldSide::Blob(oid) => Some(oid),
            OldSide::Absent | OldSide::Unknown => None,
        };
        let new_revision =
            (!is_deleted).then(|| format!("{compared_commit}:{}", repo_path.as_unix_str()));
        let project = self.project.clone();
        let repository = self.repository.clone();
        let language_registry = project.read(cx).languages().clone();
        window.spawn(cx, async move |cx| {
            let texts = git_objects.load_texts(old_blob, new_revision).await?;
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
            };
            (repo_path, entry)
        })
        .collect::<HashMap<_, _>>();

    for (repo_path, status) in statuses {
        if status.is_conflicted() {
            entries
                .entry(repo_path.clone())
                .and_modify(|entry| entry.status = status)
                .or_insert(ComparisonEntry {
                    repo_path,
                    status,
                    old_side: OldSide::Unknown,
                });
        } else if status.is_untracked() {
            entries.entry(repo_path.clone()).or_insert(ComparisonEntry {
                repo_path,
                status: tree_status_to_file_status(&TreeDiffStatus::Added),
                old_side: OldSide::Absent,
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
        );
        assert_eq!(
            entries,
            vec![
                ComparisonEntry {
                    repo_path: repo_path("a/added.rs"),
                    status: status(StatusCode::Added),
                    old_side: OldSide::Absent,
                },
                ComparisonEntry {
                    repo_path: repo_path("b/modified.rs"),
                    status: status(StatusCode::Modified),
                    old_side: OldSide::Blob(oid(1)),
                },
                ComparisonEntry {
                    repo_path: repo_path("c/deleted.rs"),
                    status: status(StatusCode::Deleted),
                    old_side: OldSide::Blob(oid(2)),
                },
                ComparisonEntry {
                    repo_path: repo_path("d/conflicted.rs"),
                    status: conflict,
                    old_side: OldSide::Blob(oid(3)),
                },
                ComparisonEntry {
                    repo_path: repo_path("e/deleted_but_untracked.rs"),
                    status: status(StatusCode::Deleted),
                    old_side: OldSide::Blob(oid(4)),
                },
                ComparisonEntry {
                    repo_path: repo_path("f/conflicted_only.rs"),
                    status: conflict,
                    old_side: OldSide::Unknown,
                },
                ComparisonEntry {
                    repo_path: repo_path("g/untracked.rs"),
                    status: status(StatusCode::Added),
                    old_side: OldSide::Absent,
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

    fn init_test(cx: &mut TestAppContext) {
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
