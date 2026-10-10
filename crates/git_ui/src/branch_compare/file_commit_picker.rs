use std::sync::Arc;

use anyhow::Result;
use fuzzy_nucleo::StringMatchCandidate;
use git::repository::{FileHistoryEntry, RepoPath};
use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, SharedString, Subscription, Task, Window, rems,
};
use picker::{Picker, PickerDelegate};
use project::{Project, git_store::Repository};
use time::OffsetDateTime;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::paths::PathStyle;
use workspace::{ModalView, Workspace};

use super::{
    branch_pair_picker::PairSelection,
    compare_diff_view::CompareDiffView,
    comparison::{
        LoadedCompareFile, LocalGitObjects, OldSide, build_committed_file, open_working_tree_file,
    },
};

type CompareCallback = Arc<dyn Fn(FileCommitComparison, &mut Window, &mut App)>;

/// An older commit of a file compared with a newer one, or with the file as it is now.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct FileCommitComparison {
    older: FileHistoryEntry,
    newer: Option<FileHistoryEntry>,
}

impl FileCommitComparison {
    fn title(&self) -> SharedString {
        let newer = match &self.newer {
            Some(newer) => newer.sha.display_short(),
            None => "current file".to_string(),
        };
        format!("{} → {newer}", self.older.sha.display_short()).into()
    }
}

/// The checked commits, or the highlighted one when none are checked, ordered by age. `history`
/// is newest first, as `git log` lists it.
fn comparison_for(
    history: &[FileHistoryEntry],
    checked: &[SharedString],
    highlighted: Option<usize>,
) -> Option<FileCommitComparison> {
    let mut indices = checked
        .iter()
        .filter_map(|sha| {
            history
                .iter()
                .position(|entry| entry.sha.to_string() == sha.as_ref())
        })
        .collect::<Vec<_>>();
    if indices.is_empty() {
        indices.extend(highlighted);
    }
    indices.sort_unstable();
    match indices.as_slice() {
        [single] => Some(FileCommitComparison {
            older: history.get(*single)?.clone(),
            newer: None,
        }),
        [newer, older] => Some(FileCommitComparison {
            older: history.get(*older)?.clone(),
            newer: Some(history.get(*newer)?.clone()),
        }),
        _ => None,
    }
}

/// A modal listing the commits that changed a file. One checked commit is compared with the
/// current file, two are compared with each other.
pub(super) struct FileCommitPicker {
    picker: Entity<Picker<FileCommitDelegate>>,
    _load_history: Task<()>,
    _subscription: Subscription,
}

impl FileCommitPicker {
    pub(super) fn new(
        repository: Entity<Repository>,
        repo_path: RepoPath,
        on_compare: impl Fn(FileCommitComparison, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = FileCommitDelegate {
            repo_path: repo_path.clone(),
            history: HistoryState::Loading,
            matches: Vec::new(),
            selected_index: 0,
            selection: PairSelection::default(),
            on_compare: Arc::new(on_compare),
            focus_handle: cx.focus_handle(),
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(40.))
                .show_scrollbar(true)
        });
        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle;
        });

        let git_objects = LocalGitObjects::connect(&repository, cx);
        let load_history = cx.spawn_in(window, async move |this, cx| {
            let history = match git_objects.await {
                Ok(git_objects) => git_objects.file_history(repo_path).await,
                Err(error) => Err(error),
            };
            this.update_in(cx, |this, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    picker.delegate.history = match history {
                        Ok(history) => HistoryState::Loaded(history.into()),
                        Err(error) => HistoryState::Failed(format!("{error:#}").into()),
                    };
                    picker.refresh(window, cx);
                });
            })
            .ok();
        });

        Self {
            _subscription: cx
                .subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            picker,
            _load_history: load_history,
        }
    }
}

impl ModalView for FileCommitPicker {}
impl EventEmitter<DismissEvent> for FileCommitPicker {}

impl Focusable for FileCommitPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for FileCommitPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("FileCommitPicker")
            .w(rems(40.))
            .child(self.picker.clone())
    }
}

enum HistoryState {
    Loading,
    Loaded(Arc<[FileHistoryEntry]>),
    Failed(SharedString),
}

impl HistoryState {
    fn entries(&self) -> &[FileHistoryEntry] {
        match self {
            HistoryState::Loaded(history) => history,
            HistoryState::Loading | HistoryState::Failed(_) => &[],
        }
    }
}

struct CommitMatch {
    /// The commit's index in the file's history.
    index: usize,
    /// Matched positions within the commit's subject.
    positions: Vec<usize>,
}

pub(super) struct FileCommitDelegate {
    repo_path: RepoPath,
    history: HistoryState,
    matches: Vec<CommitMatch>,
    selected_index: usize,
    selection: PairSelection,
    on_compare: CompareCallback,
    focus_handle: FocusHandle,
}

impl FileCommitDelegate {
    fn comparison(&self) -> Option<FileCommitComparison> {
        comparison_for(
            self.history.entries(),
            &self.selection.checked,
            self.matches
                .get(self.selected_index)
                .map(|commit_match| commit_match.index),
        )
    }

    fn compare(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(comparison) = self.comparison() else {
            return;
        };
        (self.on_compare)(comparison, window, cx);
        cx.emit(DismissEvent);
    }

    fn render_commit(
        &self,
        ix: usize,
        selected: bool,
        checkbox: Option<AnyElement>,
    ) -> Option<ListItem> {
        let commit_match = self.matches.get(ix)?;
        let entry = self.history.entries().get(commit_match.index)?;
        let commit_time = OffsetDateTime::from_unix_timestamp(entry.commit_timestamp)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let relative_time = time_format::format_localized_timestamp(
            commit_time,
            OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::Relative,
        );
        let mut details = format!(
            "{} • {} • {relative_time}",
            entry.sha.display_short(),
            entry.author_name
        );
        if entry.path != self.repo_path {
            details.push_str(" • ");
            details.push_str(&entry.path.display(PathStyle::local()));
        }
        let comparison = comparison_for(self.history.entries(), &self.selection.checked, None);
        let role = comparison.and_then(|comparison| {
            if comparison.older.sha == entry.sha {
                Some("old")
            } else if comparison.newer.is_some_and(|newer| newer.sha == entry.sha) {
                Some("new")
            } else {
                None
            }
        });

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<AnyElement>(checkbox)
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .child(
                            HighlightedLabel::new(
                                entry.subject.clone(),
                                commit_match.positions.clone(),
                            )
                            .truncate(),
                        )
                        .child(
                            Label::new(details)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .end_slot::<Label>(
                    role.map(|role| Label::new(role).size(LabelSize::Small).color(Color::Accent)),
                ),
        )
    }
}

impl PickerDelegate for FileCommitDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "file commit picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Check one commit to compare with the current file, or two…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.history {
            HistoryState::Loading => "Loading the file's history…".into(),
            HistoryState::Failed(error) => error.clone(),
            HistoryState::Loaded(history) if history.is_empty() => {
                "No commits changed this file".into()
            }
            HistoryState::Loaded(_) => "No matching commits".into(),
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let history = match &self.history {
            HistoryState::Loaded(history) => history.clone(),
            HistoryState::Loading | HistoryState::Failed(_) => Arc::from([]),
        };
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                (0..history.len())
                    .map(|index| CommitMatch {
                        index,
                        positions: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            } else {
                let candidates = history
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        StringMatchCandidate::new(
                            index,
                            &format!("{} {} {}", entry.subject, entry.sha, entry.author_name),
                        )
                    })
                    .collect::<Vec<_>>();
                fuzzy_nucleo::match_strings_async(
                    &candidates,
                    &query,
                    fuzzy_nucleo::Case::Smart,
                    fuzzy_nucleo::LengthPenalty::On,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
                .into_iter()
                .filter_map(|candidate| {
                    let subject_len = history.get(candidate.candidate_id)?.subject.len();
                    Some(CommitMatch {
                        index: candidate.candidate_id,
                        positions: candidate
                            .positions
                            .into_iter()
                            .filter(|position| *position < subject_len)
                            .collect(),
                    })
                })
                .collect()
            };
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.compare(window, cx);
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn is_multi_select_persistent(&self) -> bool {
        true
    }

    fn is_item_selected(&self, ix: usize) -> bool {
        self.matches
            .get(ix)
            .and_then(|commit_match| self.history.entries().get(commit_match.index))
            .is_some_and(|entry| self.selection.contains(&entry.sha.to_string().into()))
    }

    fn toggle_item_selected(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(entry) = self
            .matches
            .get(ix)
            .and_then(|commit_match| self.history.entries().get(commit_match.index))
        {
            self.selection.toggle(entry.sha.to_string().into());
            cx.notify();
        }
    }

    fn selected_item_count(&self) -> usize {
        self.selection.checked.len()
    }

    fn clear_selection(&mut self, _cx: &mut Context<Picker<Self>>) {
        self.selection = PairSelection::default();
    }

    fn confirm_multi(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.compare(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        self.render_commit(ix, selected, None)
    }

    fn render_match_with_checkbox(
        &self,
        ix: usize,
        selected: bool,
        checkbox: AnyElement,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        self.render_commit(ix, selected, Some(checkbox))
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let comparison = self.comparison();
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_2()
                .justify_between()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(
                        comparison
                            .as_ref()
                            .map(FileCommitComparison::title)
                            .unwrap_or_else(|| "Check up to two commits".into()),
                    )
                    .size(LabelSize::Small)
                    .color(if comparison.is_some() {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .truncate(),
                )
                .child(
                    Button::new("compare-file-commits", "Compare")
                        .disabled(comparison.is_none())
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Confirm, &self.focus_handle, cx)
                                .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                        )
                        .on_click(cx.listener(|picker, _, window, cx| {
                            picker.delegate.compare(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

/// Loads the comparison into a new diff tab. Comparing with the current file shows the file's own
/// buffer, so it stays editable.
pub(super) fn open_file_comparison(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    current_path: RepoPath,
    comparison: FileCommitComparison,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let title = comparison.title();
    let file = load_file(
        project.clone(),
        repository,
        current_path,
        comparison,
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let file = file.await;
        workspace.update_in(cx, |workspace, window, cx| match file {
            Ok(file) => {
                let workspace_entity = cx.entity();
                let view = cx.new(|cx| CompareDiffView::new(project, workspace_entity, window, cx));
                view.update(cx, |view, cx| view.show_file(file, title, None, window, cx));
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            }
            Err(error) => workspace.show_error(error, cx),
        })
    })
    .detach_and_log_err(cx);
}

fn load_file(
    project: Entity<Project>,
    repository: Entity<Repository>,
    current_path: RepoPath,
    comparison: FileCommitComparison,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<LoadedCompareFile>> {
    let git_objects = LocalGitObjects::connect(&repository, cx);
    window.spawn(cx, async move |cx| {
        let git_objects = git_objects.await?;
        let older = &comparison.older;
        let old_blob = git_objects.blob_at(older.sha, &older.path).await?;
        match comparison.newer {
            None => {
                let old_side = old_blob.map_or(OldSide::Absent, OldSide::Blob);
                cx.update(|window, cx| {
                    open_working_tree_file(project, repository, current_path, old_side, window, cx)
                })?
                .await
            }
            Some(newer) => {
                // The file is missing from the newer commit when that commit deleted it.
                let is_deleted = git_objects.blob_at(newer.sha, &newer.path).await?.is_none();
                let new_revision =
                    (!is_deleted).then(|| format!("{}:{}", newer.sha, newer.path.as_unix_str()));
                let texts = cx.background_spawn(async move {
                    git_objects.load_texts(old_blob, new_revision).await
                });
                cx.update(|window, cx| {
                    build_committed_file(
                        project, repository, newer.path, is_deleted, texts, window, cx,
                    )
                })?
                .await
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_compare::{CompareFileCommits, comparison::tests::init_test};
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use language::Capability;
    use serde_json::json;
    use std::path::Path;
    use util::{path, rel_path::rel_path};
    use workspace::{Item as _, MultiWorkspace};

    const OLDER_SHA: &str = "1111111111111111111111111111111111111111";
    const NEWER_SHA: &str = "2222222222222222222222222222222222222222";

    /// `src/new.rs` was `src/old.rs` in the older commit and was renamed before the newer one.
    async fn setup(
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Repository>, Vec<FileHistoryEntry>) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "src": { "new.rs": "one\ntwo\nthree\n" } }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        let old_blobs =
            fs.set_merge_base_content_for_repo(dot_git, &[("src/old.rs", "one\n".into())]);
        fs.set_head_for_repo(dot_git, &[("src/new.rs", "one\ntwo\n".into())], NEWER_SHA);
        fs.with_git_state(dot_git, true, |state| {
            state
                .refs
                .insert(format!("{OLDER_SHA}:src/old.rs"), old_blobs[0].to_string());
            state
                .refs
                .insert(format!("{NEWER_SHA}:src/new.rs"), "3".repeat(40));
        })
        .unwrap();
        let history = vec![
            FileHistoryEntry {
                sha: NEWER_SHA.parse().unwrap(),
                path: RepoPath::new("src/new.rs").unwrap(),
                subject: "Rename and extend".into(),
                author_name: "Author".into(),
                commit_timestamp: 2,
            },
            FileHistoryEntry {
                sha: OLDER_SHA.parse().unwrap(),
                path: RepoPath::new("src/old.rs").unwrap(),
                subject: "Add old".into(),
                author_name: "Author".into(),
                commit_timestamp: 1,
            },
        ];
        fs.set_file_history_for_repo(
            dot_git,
            RepoPath::new("src/new.rs").unwrap(),
            history.clone(),
        );

        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        (project, repository, history)
    }

    #[gpui::test]
    async fn test_load_file_uses_paths_from_the_history(cx: &mut TestAppContext) {
        let (project, repository, history) = setup(cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let current = FileCommitComparison {
            older: history[1].clone(),
            newer: None,
        };
        let file = cx
            .update(|window, cx| {
                load_file(
                    project.clone(),
                    repository.clone(),
                    RepoPath::new("src/new.rs").unwrap(),
                    current,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!file.read_only);
            let buffer = file.buffer.read(cx);
            assert_eq!(buffer.capability(), Capability::ReadWrite);
            assert_eq!(buffer.text(), "one\ntwo\nthree\n", "the live file");
            assert_eq!(
                file.diff.read(cx).base_text_string(cx).as_deref(),
                Some("one\n"),
                "the older commit's text, read from its pre-rename path"
            );
        });

        let commits = FileCommitComparison {
            older: history[1].clone(),
            newer: Some(history[0].clone()),
        };
        let file = cx
            .update(|window, cx| {
                load_file(
                    project.clone(),
                    repository.clone(),
                    RepoPath::new("src/new.rs").unwrap(),
                    commits,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(file.read_only);
            let buffer = file.buffer.read(cx);
            assert_eq!(buffer.capability(), Capability::ReadOnly);
            assert_eq!(buffer.text(), "one\ntwo\n", "the newer commit's text");
            assert_eq!(
                file.diff.read(cx).base_text_string(cx).as_deref(),
                Some("one\n")
            );
        });
    }

    #[gpui::test]
    async fn test_compare_file_commits_opens_a_diff_tab(cx: &mut TestAppContext) {
        let (project, _repository, _history) = setup(cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, rel_path("src/new.rs")),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        cx.dispatch_action(CompareFileCommits);
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<FileCommitPicker>(cx).is_some()
        }));
        cx.dispatch_action(picker::MultiSelectNext);
        cx.dispatch_action(picker::MultiSelectNext);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<FileCommitPicker>(cx).is_none());
            let view = workspace
                .active_item_as::<CompareDiffView>(cx)
                .expect("the comparison should open in a tab");
            assert_eq!(
                view.read(cx).tab_tooltip_text(cx).as_deref(),
                Some("src/new.rs · 1111111 → 2222222")
            );
        });
    }

    fn entry(sha_digit: char, path: &str) -> FileHistoryEntry {
        FileHistoryEntry {
            sha: sha_digit.to_string().repeat(40).parse().unwrap(),
            path: RepoPath::new(path).unwrap(),
            subject: format!("commit {sha_digit}").into(),
            author_name: "Author".into(),
            commit_timestamp: 1,
        }
    }

    fn sha(entry: &FileHistoryEntry) -> SharedString {
        entry.sha.to_string().into()
    }

    #[test]
    fn test_comparison_for_checked_and_highlighted_commits() {
        let history = [
            entry('3', "new.rs"),
            entry('2', "new.rs"),
            entry('1', "old.rs"),
        ];

        assert_eq!(comparison_for(&history, &[], None), None);
        assert_eq!(
            comparison_for(&history, &[], Some(1)),
            Some(FileCommitComparison {
                older: history[1].clone(),
                newer: None,
            }),
            "with nothing checked, the highlighted commit is compared with the current file"
        );
        assert_eq!(
            comparison_for(&history, &[sha(&history[2])], Some(0)),
            Some(FileCommitComparison {
                older: history[2].clone(),
                newer: None,
            }),
            "a checked commit wins over the highlighted one"
        );
        assert_eq!(
            comparison_for(&history, &[sha(&history[0]), sha(&history[2])], Some(1)),
            Some(FileCommitComparison {
                older: history[2].clone(),
                newer: Some(history[0].clone()),
            }),
            "the older commit is the old side whichever was checked first"
        );
    }

    #[test]
    fn test_comparison_title() {
        let history = [entry('2', "new.rs"), entry('1', "old.rs")];
        let current = FileCommitComparison {
            older: history[1].clone(),
            newer: None,
        };
        assert_eq!(current.title(), "1111111 → current file");
        let commits = FileCommitComparison {
            older: history[1].clone(),
            newer: Some(history[0].clone()),
        };
        assert_eq!(commits.title(), "1111111 → 2222222");
    }
}
