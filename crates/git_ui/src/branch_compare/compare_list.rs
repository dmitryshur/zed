use std::{ops::Range, sync::Arc, time::Duration};

use anyhow::Result;
use collections::HashSet;
use file_icons::FileIcons;
use git::{Oid, repository::RepoPath};
use gpui::{
    Action as _, AppContext as _, Context, Entity, ScrollStrategy, SharedString, Subscription,
    Task, UniformListScrollHandle, WeakEntity, Window, point, uniform_list,
};
use settings::Settings as _;
use ui::{IndentGuideColors, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::paths::PathStyle;
use workspace::Workspace;

use super::{
    CompareBranches,
    compare_diff_view::CompareDiffView,
    comparison::{
        BranchComparison, ComparisonEntry, ComparisonEvent, ComparisonState, LoadedCompareFile,
    },
    file_tree::{CompareRow, build_rows},
};
use crate::{git_panel_settings::GitPanelSettings, git_status_icon};

/// Moving through the list loads each file; waiting briefly avoids loading every file passed
/// over while holding a navigation key.
const SELECTION_LOAD_DEBOUNCE: Duration = Duration::from_millis(50);

const TREE_INDENT: f32 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerticalDirection {
    Up,
    Down,
}

/// Identifies a row across rebuilds of the tree, when row indices shift.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    File(RepoPath),
    Directory(RepoPath),
}

struct LoadingFile {
    entry: ComparisonEntry,
    focus_when_loaded: bool,
}

/// The contents of the git panel's Compare tab: the files of a branch comparison, as a tree. The
/// selected file is shown in a single [`CompareDiffView`] tab.
pub(crate) struct CompareList {
    workspace: WeakEntity<Workspace>,
    comparison: Option<Entity<BranchComparison>>,
    entries: Arc<[ComparisonEntry]>,
    rows: Vec<CompareRow>,
    collapsed_directories: HashSet<RepoPath>,
    selected_row: Option<usize>,
    /// The number of rows in the last render, which the page size is measured against.
    rendered_row_count: usize,
    scroll_handle: UniformListScrollHandle,
    diff_view: WeakEntity<CompareDiffView>,
    loading_file: Option<LoadingFile>,
    /// The entry and compared commit the diff view was last loaded with.
    shown: Option<(ComparisonEntry, Option<Oid>)>,
    file_load_task: Task<()>,
    _comparison_subscription: Option<Subscription>,
}

impl CompareList {
    pub(crate) fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            comparison: None,
            entries: Arc::from([]),
            rows: Vec::new(),
            collapsed_directories: HashSet::default(),
            selected_row: None,
            rendered_row_count: 0,
            scroll_handle: UniformListScrollHandle::new(),
            diff_view: WeakEntity::new_invalid(),
            loading_file: None,
            shown: None,
            file_load_task: Task::ready(()),
            _comparison_subscription: None,
        }
    }

    pub(crate) fn set_comparison(
        &mut self,
        comparison: Option<Entity<BranchComparison>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._comparison_subscription = comparison.as_ref().map(|comparison| {
            cx.subscribe_in(comparison, window, |this, _, event, window, cx| {
                this.handle_comparison_event(event, window, cx);
            })
        });
        self.comparison = comparison;
        self.entries = Arc::from([]);
        self.rows.clear();
        self.collapsed_directories.clear();
        self.selected_row = None;
        self.loading_file = None;
        self.shown = None;
        self.file_load_task = Task::ready(());
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows = build_rows(&self.entries, &self.collapsed_directories);
    }

    fn row_key(&self, row_index: usize) -> Option<RowKey> {
        match self.rows.get(row_index)? {
            CompareRow::Directory { path, .. } => Some(RowKey::Directory(path.clone())),
            CompareRow::File { entry_index, .. } => self
                .entries
                .get(*entry_index)
                .map(|entry| RowKey::File(entry.repo_path.clone())),
        }
    }

    fn row_index(&self, key: &RowKey) -> Option<usize> {
        self.rows.iter().position(|row| match (row, key) {
            (CompareRow::Directory { path, .. }, RowKey::Directory(key_path)) => path == key_path,
            (CompareRow::File { entry_index, .. }, RowKey::File(key_path)) => self
                .entries
                .get(*entry_index)
                .is_some_and(|entry| &entry.repo_path == key_path),
            _ => false,
        })
    }

    fn entry_for_row(&self, row_index: usize) -> Option<&ComparisonEntry> {
        match self.rows.get(row_index)? {
            CompareRow::File { entry_index, .. } => self.entries.get(*entry_index),
            CompareRow::Directory { .. } => None,
        }
    }

    fn selected_entry(&self) -> Option<ComparisonEntry> {
        self.entry_for_row(self.selected_row?).cloned()
    }

    fn handle_comparison_event(
        &mut self,
        event: &ComparisonEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previously_selected = self
            .loading_file
            .as_ref()
            .map(|loading| RowKey::File(loading.entry.repo_path.clone()))
            .or_else(|| self.selected_row.and_then(|row| self.row_key(row)));
        self.entries = self
            .comparison
            .as_ref()
            .and_then(|comparison| match comparison.read(cx).state() {
                ComparisonState::Loaded(entries) => Some(entries.clone()),
                ComparisonState::Loading | ComparisonState::Failed(_) => None,
            })
            .unwrap_or_else(|| Arc::from([]));
        self.rebuild_rows();
        let first_file_row = self
            .rows
            .iter()
            .position(|row| matches!(row, CompareRow::File { .. }));
        self.selected_row = match &previously_selected {
            Some(key) => self.row_index(key).or_else(|| {
                self.selected_row
                    .map(|row| row.min(self.rows.len().saturating_sub(1)))
                    .filter(|_| !self.rows.is_empty())
            }),
            None => first_file_row,
        };

        if let Some(selected_entry) = self.selected_entry() {
            let compared_commit = self
                .comparison
                .as_ref()
                .and_then(|comparison| comparison.read(cx).compared_commit());
            let is_loading_selected = self
                .loading_file
                .as_ref()
                .is_some_and(|loading| loading.entry.repo_path == selected_entry.repo_path);
            let needs_reload = matches!(event, ComparisonEvent::ModeChanged)
                || self.shown.as_ref() != Some(&(selected_entry, compared_commit));
            if needs_reload && !is_loading_selected {
                self.show_selected_file(false, false, window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn select_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_row(|_, _| Some(0), window, cx);
    }

    pub(crate) fn select_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_row(|_, count| count.checked_sub(1), window, cx);
    }

    pub(crate) fn select_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_row(
            |selected, count| match selected {
                Some(selected) => Some((selected + 1).min(count.saturating_sub(1))),
                None => Some(0),
            },
            window,
            cx,
        );
    }

    pub(crate) fn select_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_row(
            |selected, _| Some(selected.map_or(0, |selected| selected.saturating_sub(1))),
            window,
            cx,
        );
    }

    /// Moves the selection by half the number of rows that fit in the list.
    pub(crate) fn select_half_page(
        &mut self,
        direction: VerticalDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.rows_per_page() / 2).max(1);
        self.select_row(
            |selected, count| {
                Some(match (direction, selected) {
                    (_, None) => 0,
                    (VerticalDirection::Down, Some(selected)) => {
                        (selected + step).min(count.saturating_sub(1))
                    }
                    (VerticalDirection::Up, Some(selected)) => selected.saturating_sub(step),
                })
            },
            window,
            cx,
        );
    }

    /// Scrolls the list by half its height, leaving the selection (and the diff) as is.
    pub(crate) fn scroll_half_page(
        &mut self,
        direction: VerticalDirection,
        cx: &mut Context<Self>,
    ) {
        let state = self.scroll_handle.0.borrow();
        let Some(item_size) = state.last_item_size else {
            return;
        };
        let scroll_handle = state.base_handle.clone();
        drop(state);

        let distance = item_size.item.height / 2.;
        let offset = scroll_handle.offset();
        let lowest_offset = -scroll_handle.max_offset().y;
        let new_y = match direction {
            VerticalDirection::Down => {
                let new_y = offset.y - distance;
                if new_y < lowest_offset {
                    lowest_offset
                } else {
                    new_y
                }
            }
            VerticalDirection::Up => {
                let new_y = offset.y + distance;
                if new_y > px(0.) { px(0.) } else { new_y }
            }
        };
        scroll_handle.set_offset(point(offset.x, new_y));
        cx.notify();
    }

    fn rows_per_page(&self) -> usize {
        let Some(item_size) = self.scroll_handle.0.borrow().last_item_size else {
            return 1;
        };
        if self.rendered_row_count == 0 {
            return 1;
        }
        let row_height = item_size.contents.height / self.rendered_row_count as f32;
        if row_height > px(0.) {
            ((item_size.item.height / row_height) as usize).max(1)
        } else {
            1
        }
    }

    fn select_row(
        &mut self,
        new_row: impl FnOnce(Option<usize>, usize) -> Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let Some(row) = new_row(self.selected_row, count) else {
            return;
        };
        if self.selected_row == Some(row) {
            return;
        }
        self.selected_row = Some(row);
        self.scroll_handle
            .scroll_to_item(row, ScrollStrategy::Center);
        if self.entry_for_row(row).is_some() {
            self.show_selected_file(false, true, window, cx);
        } else {
            // A folder row keeps the last file's diff; drop a load still waiting on its debounce.
            self.loading_file = None;
            self.file_load_task = Task::ready(());
        }
        cx.notify();
    }

    pub(crate) fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(CompareRow::Directory { path, .. }) =
            self.selected_row.and_then(|row| self.rows.get(row))
        {
            let path = path.clone();
            self.toggle_directory(path, cx);
            return;
        }
        let Some(selected_entry) = self.selected_entry() else {
            return;
        };
        if let Some(loading) = self.loading_file.as_mut()
            && loading.entry.repo_path == selected_entry.repo_path
        {
            loading.focus_when_loaded = true;
            return;
        }
        let shown_view = self.diff_view.upgrade().filter(|view| {
            view.read(cx).shown_path() == Some(&selected_entry.repo_path)
                && self
                    .workspace
                    .upgrade()
                    .is_some_and(|workspace| workspace.read(cx).pane_for(view).is_some())
        });
        match shown_view {
            Some(view) => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.activate_item(&view, true, true, window, cx);
                    });
                }
            }
            None => self.show_selected_file(true, false, window, cx),
        }
    }

    /// Opens a collapsed folder; otherwise moves to the next row.
    pub(crate) fn expand_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(CompareRow::Directory {
            path,
            expanded: false,
            ..
        }) = self.selected_row.and_then(|row| self.rows.get(row))
        {
            let path = path.clone();
            self.toggle_directory(path, cx);
        } else {
            self.select_next(window, cx);
        }
    }

    /// Closes the selected folder or the nearest open folder above the selection, and selects it;
    /// otherwise moves to the previous row.
    pub(crate) fn collapse_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_row) = self.selected_row else {
            return;
        };
        let start_path = match self.rows.get(selected_row) {
            Some(CompareRow::Directory {
                path,
                expanded: true,
                ..
            }) => {
                let path = path.clone();
                self.toggle_directory(path, cx);
                return;
            }
            Some(CompareRow::Directory { path, .. }) => path.clone(),
            Some(CompareRow::File { entry_index, .. }) => match self.entries.get(*entry_index) {
                Some(entry) => entry.repo_path.clone(),
                None => return,
            },
            None => return,
        };
        let mut ancestor = start_path.parent().map(RepoPath::from_rel_path);
        while let Some(path) = ancestor {
            let has_expanded_row = self.rows.iter().any(|row| {
                matches!(
                    row,
                    CompareRow::Directory { path: row_path, expanded: true, .. } if row_path == &path
                )
            });
            if has_expanded_row {
                self.toggle_directory(path, cx);
                return;
            }
            ancestor = path.parent().map(RepoPath::from_rel_path);
        }
        self.select_previous(window, cx);
    }

    /// Collapses or expands a folder and selects it.
    fn toggle_directory(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        if !self.collapsed_directories.remove(&path) {
            self.collapsed_directories.insert(path.clone());
        }
        self.rebuild_rows();
        self.selected_row = self.row_index(&RowKey::Directory(path));
        if let Some(row) = self.selected_row {
            self.scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn select_and_confirm(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_row != Some(row) {
            self.selected_row = Some(row);
            self.loading_file = None;
            self.file_load_task = Task::ready(());
        }
        self.confirm(window, cx);
        cx.notify();
    }

    fn show_selected_file(
        &mut self,
        focus: bool,
        debounce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(comparison), Some(entry)) = (self.comparison.clone(), self.selected_entry())
        else {
            return;
        };
        self.loading_file = Some(LoadingFile {
            entry: entry.clone(),
            focus_when_loaded: focus,
        });
        self.file_load_task = cx.spawn_in(window, async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(SELECTION_LOAD_DEBOUNCE)
                    .await;
            }
            let Ok(load) = this.update_in(cx, |_, window, cx| {
                comparison.update(cx, |comparison, cx| {
                    comparison.load_file(&entry, window, cx)
                })
            }) else {
                return;
            };
            let result = load.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_file_load(&comparison, entry, result, window, cx);
            })
            .ok();
        });
    }

    fn finish_file_load(
        &mut self,
        comparison: &Entity<BranchComparison>,
        entry: ComparisonEntry,
        result: Result<LoadedCompareFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self
            .loading_file
            .take()
            .is_some_and(|loading| loading.focus_when_loaded);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let view = self.diff_view_or_create(&workspace, window, cx);
        let comparison = comparison.read(cx);
        let title = comparison.title();
        self.shown = Some((entry, comparison.compared_commit()));
        view.update(cx, |view, cx| match result {
            Ok(file) => view.show_file(file, title, window, cx),
            Err(error) => view.show_error(format!("{error:#}").into(), title, cx),
        });
        workspace.update(cx, |workspace, cx| {
            workspace.activate_item(&view, focus, focus, window, cx);
        });
    }

    fn diff_view_or_create(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<CompareDiffView> {
        if let Some(view) = self.diff_view.upgrade()
            && workspace.read(cx).pane_for(&view).is_some()
        {
            return view;
        }
        let project = workspace.read(cx).project().clone();
        let view = cx.new(|cx| CompareDiffView::new(project, workspace.clone(), window, cx));
        workspace.update(cx, |workspace, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, false, window, cx);
        });
        self.diff_view = view.downgrade();
        view
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .comparison
            .as_ref()
            .map(|comparison| comparison.read(cx).title());
        h_flex()
            .h(rems(1.75))
            .px_2()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(
                    title
                        .clone()
                        .unwrap_or_else(|| "No branch comparison".into()),
                )
                .size(LabelSize::Small)
                .color(if title.is_some() {
                    Color::Default
                } else {
                    Color::Muted
                })
                .truncate(),
            )
            .when(title.is_some(), |header| {
                header.child(
                    IconButton::new("clear-branch-comparison", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Clear Comparison"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_comparison(None, window, cx);
                        })),
                )
            })
    }

    fn render_message(message: SharedString) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px_2()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(comparison) = self.comparison.as_ref() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Label::new("Compare two branches to list their changed files")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new("compare-branches", "Compare Branches…").on_click(
                        |_, window, cx| window.dispatch_action(CompareBranches.boxed_clone(), cx),
                    ),
                )
                .into_any_element();
        };
        match comparison.read(cx).state() {
            ComparisonState::Loading => return Self::render_message("Comparing…".into()),
            ComparisonState::Failed(message) => return Self::render_message(message.clone()),
            ComparisonState::Loaded(entries) if entries.is_empty() => {
                return Self::render_message("No changes".into());
            }
            ComparisonState::Loaded(_) => {}
        }
        uniform_list(
            "branch-comparison-rows",
            self.rows.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|row_index| this.render_row(row_index, cx))
                    .collect()
            }),
        )
        .with_decoration(
            ui::indent_guides(px(TREE_INDENT), IndentGuideColors::panel(cx))
                .with_left_offset(ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET - px(2.))
                .with_compute_indents_fn(cx.entity(), |this, range, _window, _cx| {
                    range
                        .map(|row_index| this.rows.get(row_index).map_or(0, CompareRow::depth))
                        .collect()
                }),
        )
        .track_scroll(&self.scroll_handle)
        .size_full()
        .into_any_element()
    }

    fn render_row(&self, row_index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let selected = self.selected_row == Some(row_index);
        let item = match self.rows.get(row_index)? {
            CompareRow::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let folder_indicator = GitPanelSettings::get_global(cx).folder_indicator;
                let indicators = FileIcons::get_folder_indicators(
                    folder_indicator,
                    *expanded,
                    path.as_std_path(),
                    cx,
                );
                let render_indicator = |themed: Option<SharedString>, fallback: IconName| {
                    themed
                        .map(Icon::from_path)
                        .unwrap_or_else(|| Icon::new(fallback))
                        .size(IconSize::Small)
                        .color(Color::Muted)
                };
                let mut icons = Vec::new();
                if folder_indicator.shows_chevron() {
                    icons.push(render_indicator(
                        indicators.chevron,
                        if *expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        },
                    ));
                }
                if folder_indicator.shows_icon() {
                    icons.push(render_indicator(
                        indicators.icon,
                        if *expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        },
                    ));
                }
                ListItem::new(("branch-comparison-directory", row_index))
                    .indent_level(*depth)
                    .indent_step_size(px(TREE_INDENT))
                    .start_slot(h_flex().gap_0p5().children(icons))
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .tooltip(Tooltip::text(path.display(PathStyle::local()).to_string()))
            }
            CompareRow::File { entry_index, depth } => {
                let entry = self.entries.get(*entry_index)?;
                let file_name = entry
                    .repo_path
                    .file_name()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                ListItem::new(("branch-comparison-file", row_index))
                    .indent_level(*depth)
                    .indent_step_size(px(TREE_INDENT))
                    .start_slot(git_status_icon(entry.status))
                    .child(
                        Label::new(file_name)
                            .size(LabelSize::Small)
                            .color(if entry.status.is_deleted() {
                                Color::Disabled
                            } else {
                                Color::Default
                            })
                            .truncate(),
                    )
                    .tooltip(Tooltip::text(
                        entry.repo_path.display(PathStyle::local()).to_string(),
                    ))
            }
        };
        Some(
            item.spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_and_confirm(row_index, window, cx);
                }))
                .into_any_element(),
        )
    }
}

impl Render for CompareList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rendered_row_count = self.rows.len();
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        branch_compare::{
            comparison::tests::{branch, setup},
            start_comparison,
        },
        git_panel::GitPanel,
    };
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use workspace::{MultiWorkspace, item::ItemHandle as _};

    fn diff_views(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Vec<Entity<CompareDiffView>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<CompareDiffView>(cx).collect()
        })
    }

    fn shown_path(view: &Entity<CompareDiffView>, cx: &mut VisualTestContext) -> Option<String> {
        view.read_with(cx, |view, _| {
            view.shown_path()
                .map(|repo_path| repo_path.as_unix_str().to_string())
        })
    }

    #[gpui::test]
    async fn test_compare_tree_folders_and_paging(cx: &mut TestAppContext) {
        let (project, repository, _fs) = setup("main", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let panel = workspace.update_in(cx, GitPanel::new_test);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            start_comparison(
                workspace,
                repository.clone(),
                branch("refs/heads/main"),
                branch("refs/heads/feature"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let list = panel.read_with(cx, |panel, _| panel.compare_list().clone());
        let state = |cx: &mut VisualTestContext| {
            list.read_with(cx, |list, _| (list.rows.len(), list.selected_row))
        };
        let shown = |cx: &mut VisualTestContext| {
            list.read_with(cx, |list, _| {
                list.shown
                    .as_ref()
                    .map(|(entry, _)| entry.repo_path.as_unix_str().to_string())
            })
        };
        let settle = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(SELECTION_LOAD_DEBOUNCE * 2);
            cx.run_until_parked();
        };

        assert_eq!(
            state(cx),
            (4, Some(1)),
            "src/ plus three files; the first file is selected"
        );
        assert_eq!(shown(cx).as_deref(), Some("src/added.rs"));

        cx.dispatch_action(menu::SelectPrevious);
        settle(cx);
        assert_eq!(state(cx), (4, Some(0)));
        assert_eq!(
            shown(cx).as_deref(),
            Some("src/added.rs"),
            "a selected folder keeps the last file's diff"
        );

        cx.dispatch_action(super::super::CollapseCompareEntry);
        cx.run_until_parked();
        assert_eq!(
            state(cx),
            (1, Some(0)),
            "collapsing hides the folder's files"
        );
        cx.dispatch_action(super::super::ExpandCompareEntry);
        cx.run_until_parked();
        assert_eq!(state(cx), (4, Some(0)));

        cx.dispatch_action(menu::SelectLast);
        settle(cx);
        assert_eq!(state(cx), (4, Some(3)));
        cx.dispatch_action(super::super::CollapseCompareEntry);
        cx.run_until_parked();
        assert_eq!(
            state(cx),
            (1, Some(0)),
            "collapsing from a file closes and selects its folder"
        );
        cx.dispatch_action(super::super::ExpandCompareEntry);
        cx.run_until_parked();

        cx.dispatch_action(super::super::ScrollCompareListDown);
        settle(cx);
        assert_eq!(
            state(cx),
            (4, Some(0)),
            "scrolling leaves the selection alone"
        );
        assert_eq!(shown(cx).as_deref(), Some("src/deleted.rs"));

        cx.dispatch_action(super::super::SelectCompareHalfPageDown);
        settle(cx);
        let (_, selected_row) = state(cx);
        assert!(
            selected_row.is_some_and(|row| row > 0),
            "the selection moves down"
        );
    }

    #[gpui::test]
    async fn test_half_page_scrolling_and_selection(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("main", cx).await;
        let dot_git = std::path::Path::new(util::path!("/project/.git"));
        let many_files = (0..60)
            .map(|index| {
                (
                    format!("src/many/file_{index:02}.rs"),
                    format!("// {index}\n"),
                )
            })
            .collect::<Vec<_>>();
        fs.set_merge_base_content_for_repo(
            dot_git,
            &many_files
                .iter()
                .map(|(path, content)| (path.as_str(), format!("{content}// old\n")))
                .collect::<Vec<_>>(),
        );
        fs.set_head_for_repo(
            dot_git,
            &many_files
                .iter()
                .map(|(path, content)| (path.as_str(), content.clone()))
                .collect::<Vec<_>>(),
            super::super::comparison::tests::FEATURE_SHA,
        );
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let panel = workspace.update_in(cx, GitPanel::new_test);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            start_comparison(
                workspace,
                repository.clone(),
                branch("refs/heads/main"),
                branch("refs/heads/feature"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let list = panel.read_with(cx, |panel, _| panel.compare_list().clone());
        let scroll_y = |cx: &mut VisualTestContext| {
            list.read_with(cx, |list, _| {
                list.scroll_handle.0.borrow().base_handle.offset().y
            })
        };
        let selected_row =
            |cx: &mut VisualTestContext| list.read_with(cx, |list, _| list.selected_row);
        assert_eq!(list.read_with(cx, |list, _| list.rows.len()), 61);
        assert_eq!(selected_row(cx), Some(1));
        assert_eq!(scroll_y(cx), px(0.));

        cx.dispatch_action(super::super::ScrollCompareListDown);
        cx.run_until_parked();
        let scrolled = scroll_y(cx);
        assert!(scrolled < px(0.), "the list scrolls down");
        assert_eq!(
            selected_row(cx),
            Some(1),
            "scrolling leaves the selection alone"
        );
        cx.dispatch_action(super::super::ScrollCompareListUp);
        cx.run_until_parked();
        assert_eq!(scroll_y(cx), px(0.));

        let rows_per_page = list.read_with(cx, |list, _| list.rows_per_page());
        assert!(rows_per_page > 2, "the test window fits several rows");
        cx.dispatch_action(super::super::SelectCompareHalfPageDown);
        cx.run_until_parked();
        assert_eq!(selected_row(cx), Some(1 + rows_per_page / 2));
        cx.dispatch_action(super::super::SelectCompareHalfPageUp);
        cx.run_until_parked();
        assert_eq!(selected_row(cx), Some(1));
    }

    #[gpui::test]
    async fn test_compare_tab_shows_the_selected_file_in_one_tab(cx: &mut TestAppContext) {
        let (project, repository, _fs) = setup("main", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let panel = workspace.update_in(cx, GitPanel::new_test);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            start_comparison(
                workspace,
                repository.clone(),
                branch("refs/heads/main"),
                branch("refs/heads/feature"),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let views = diff_views(&workspace, cx);
        assert_eq!(views.len(), 1);
        let view = views[0].clone();
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        let panel_has_focus = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.focus_handle(cx).contains_focused(window, cx))
        };
        assert!(panel_has_focus(cx), "focus stays in the panel");

        cx.dispatch_action(menu::SelectNext);
        cx.executor().advance_clock(SELECTION_LOAD_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
        assert!(
            panel_has_focus(cx),
            "moving the selection keeps focus in the panel"
        );

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| view.focus_handle(cx).contains_focused(window, cx)),
            "confirming focuses the diff"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.remove_item(view.item_id(), false, false, window, cx)
            });
            workspace.focus_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(menu::SelectNext);
        cx.executor().advance_clock(SELECTION_LOAD_DEBOUNCE * 2);
        cx.run_until_parked();
        let views = diff_views(&workspace, cx);
        assert_eq!(views.len(), 1, "a closed diff tab is reopened");
        assert_ne!(views[0], view);
        assert_eq!(shown_path(&views[0], cx).as_deref(), Some("src/deleted.rs"));
    }
}
