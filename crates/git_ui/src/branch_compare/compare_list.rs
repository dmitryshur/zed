use std::{ops::Range, sync::Arc};

use anyhow::Result;
use collections::HashSet;
use editor::{Editor, EditorEvent, scroll::ScrollAmount};
use file_icons::FileIcons;
use fuzzy_nucleo::{Case, LengthPenalty, StringMatchCandidate};
use git::{Oid, repository::RepoPath};
use gpui::{
    Action as _, AppContext as _, Context, Entity, Focusable as _, ScrollStrategy, SharedString,
    Subscription, Task, UniformListScrollHandle, WeakEntity, Window, point, uniform_list,
};
use settings::Settings as _;
use ui::{IndentGuideColors, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::paths::PathStyle;
use workspace::Workspace;

use super::{
    ClearCompareFilter, CompareBranches,
    compare_diff_view::CompareDiffView,
    comparison::{
        BranchComparison, ComparisonEntry, ComparisonEvent, ComparisonMode, ComparisonState,
        LoadedCompareFile,
    },
    file_tree::{CompareRow, build_rows},
};
use crate::{git_panel_settings::GitPanelSettings, git_status_icon};

const TREE_INDENT: f32 = 16.0;

fn matching_entry_indices(entries: &[ComparisonEntry], query: &str) -> HashSet<usize> {
    let candidates = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            StringMatchCandidate::new(index, entry.repo_path.as_unix_str().to_string())
        })
        .collect::<Vec<_>>();
    fuzzy_nucleo::match_strings(
        &candidates,
        query,
        Case::Smart,
        LengthPenalty::On,
        entries.len(),
    )
    .into_iter()
    .map(|matched| matched.candidate_id)
    .collect()
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComparedFile {
    entry: ComparisonEntry,
    compared_commit: Option<Oid>,
    mode: ComparisonMode,
}

struct LoadingFile {
    file: ComparedFile,
    activate_when_loaded: bool,
}

/// The contents of the git panel's Compare tab: the files of a branch comparison, as a tree. The
/// confirmed file is shown in a single [`CompareDiffView`] tab.
pub(crate) struct CompareList {
    workspace: WeakEntity<Workspace>,
    comparison: Option<Entity<BranchComparison>>,
    entries: Arc<[ComparisonEntry]>,
    rows: Vec<CompareRow>,
    collapsed_directories: HashSet<RepoPath>,
    filtered_collapsed_directories: HashSet<RepoPath>,
    filter_editor: Option<Entity<Editor>>,
    filter_query: String,
    filter_visible: bool,
    selected_row: Option<usize>,
    initial_file_pending: bool,
    /// The number of rows in the last render, which the page size is measured against.
    rendered_row_count: usize,
    scroll_handle: UniformListScrollHandle,
    diff_view: WeakEntity<CompareDiffView>,
    loading_file: Option<LoadingFile>,
    shown: Option<ComparedFile>,
    file_load_task: Task<()>,
    _comparison_subscription: Option<Subscription>,
    _filter_subscriptions: Vec<Subscription>,
}

impl CompareList {
    pub(crate) fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            comparison: None,
            entries: Arc::from([]),
            rows: Vec::new(),
            collapsed_directories: HashSet::default(),
            filtered_collapsed_directories: HashSet::default(),
            filter_editor: None,
            filter_query: String::new(),
            filter_visible: false,
            selected_row: None,
            initial_file_pending: false,
            rendered_row_count: 0,
            scroll_handle: UniformListScrollHandle::new(),
            diff_view: WeakEntity::new_invalid(),
            loading_file: None,
            shown: None,
            file_load_task: Task::ready(()),
            _comparison_subscription: None,
            _filter_subscriptions: Vec::new(),
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
        self.initial_file_pending = comparison.is_some();
        self.comparison = comparison;
        self.entries = Arc::from([]);
        self.rows.clear();
        self.collapsed_directories.clear();
        self.filtered_collapsed_directories.clear();
        self.filter_query.clear();
        self.filter_visible = false;
        if let Some(editor) = &self.filter_editor {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        self.selected_row = None;
        self.loading_file = None;
        self.shown = None;
        self.file_load_task = Task::ready(());
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        let visible_entry_indices = self
            .has_filter()
            .then(|| matching_entry_indices(&self.entries, &self.filter_query));
        let collapsed = if self.has_filter() {
            &self.filtered_collapsed_directories
        } else {
            &self.collapsed_directories
        };
        self.rows = build_rows(&self.entries, collapsed, visible_entry_indices.as_ref());
    }

    fn has_filter(&self) -> bool {
        !self.filter_query.is_empty()
    }

    pub(crate) fn filter_is_visible(&self) -> bool {
        self.filter_visible
    }

    pub(crate) fn filter_is_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.filter_editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    pub(crate) fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = match &self.filter_editor {
            Some(editor) => editor.clone(),
            None => {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Filter files…", window, cx);
                    editor
                });
                self._filter_subscriptions.push(cx.subscribe(
                    &editor,
                    |this, editor, event: &EditorEvent, cx| {
                        if matches!(event, EditorEvent::BufferEdited) {
                            let query = editor.read(cx).text(cx);
                            this.update_filter(query, cx);
                        }
                    },
                ));
                let focus_handle = editor.focus_handle(cx);
                self._filter_subscriptions.push(cx.on_focus_in(
                    &focus_handle,
                    window,
                    |_, _, cx| cx.notify(),
                ));
                self._filter_subscriptions.push(cx.on_focus_out(
                    &focus_handle,
                    window,
                    |_, _, _, cx| cx.notify(),
                ));
                self.filter_editor = Some(editor.clone());
                editor
            }
        };
        self.filter_visible = true;
        editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn finish_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_filter() {
            self.filter_visible = true;
            cx.notify();
        } else {
            self.clear_filter(window, cx);
        }
    }

    pub(crate) fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_filter(String::new(), cx);
        self.filter_visible = false;
        if let Some(editor) = &self.filter_editor {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        cx.notify();
    }

    fn update_filter(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_string();
        if self.filter_query == query {
            return;
        }
        let selected = self.selected_row.and_then(|row| self.row_key(row));
        self.filter_query = query;
        self.filtered_collapsed_directories.clear();
        self.rebuild_rows();
        self.selected_row = selected
            .as_ref()
            .and_then(|key| self.row_index(key))
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| matches!(row, CompareRow::File { .. }))
            });
        if let Some(row) = self.selected_row {
            self.scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
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
        if matches!(event, ComparisonEvent::ModeChanged) {
            // The entries still describe the old mode until `EntriesChanged` follows.
            return;
        }
        let previously_selected = self.selected_row.and_then(|row| self.row_key(row));
        let entries =
            self.comparison
                .as_ref()
                .and_then(|comparison| match comparison.read(cx).state() {
                    ComparisonState::Loaded(entries) => Some(entries.clone()),
                    ComparisonState::Loading | ComparisonState::Failed(_) => None,
                });
        let loaded = entries.is_some();
        self.entries = entries.unwrap_or_else(|| Arc::from([]));
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

        if loaded {
            if self.initial_file_pending {
                self.initial_file_pending = false;
                if let Some(entry) = self.selected_entry() {
                    self.load_file(entry, true, window, cx);
                }
            } else {
                self.refresh_displayed_file(window, cx);
            }
        }
        cx.notify();
    }

    fn refresh_displayed_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (file, activate) = match &self.loading_file {
            Some(loading) => (&loading.file, loading.activate_when_loaded),
            None => {
                let Some(file) = self.shown.as_ref() else {
                    return;
                };
                if self.existing_diff_view(cx).is_none() {
                    return;
                }
                (file, false)
            }
        };
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.repo_path == file.entry.repo_path)
            .cloned()
        else {
            return;
        };
        let Some(comparison) = self.comparison.as_ref() else {
            return;
        };
        let comparison = comparison.read(cx);
        let updated_file = ComparedFile {
            entry: entry.clone(),
            compared_commit: comparison.compared_commit(),
            mode: comparison.mode(),
        };
        if file != &updated_file {
            self.load_file(entry, activate, window, cx);
        }
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

    pub(crate) fn scroll_diff(
        &mut self,
        direction: VerticalDirection,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(comparison), Some(view)) = (self.comparison.as_ref(), self.diff_view.upgrade())
        else {
            return;
        };
        if self.shown.is_none()
            || !matches!(comparison.read(cx).state(), ComparisonState::Loaded(_))
        {
            return;
        }
        let Some(pane) = workspace.pane_for(&view) else {
            return;
        };
        if pane
            .read(cx)
            .active_item()
            .is_none_or(|item| item.item_id() != view.entity_id())
        {
            return;
        }
        let amount = ScrollAmount::Page(match direction {
            VerticalDirection::Down => 0.5,
            VerticalDirection::Up => -0.5,
        });
        view.update(cx, |view, cx| view.scroll(&amount, window, cx));
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
        _window: &mut Window,
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
            && loading.file.entry.repo_path == selected_entry.repo_path
        {
            loading.activate_when_loaded = true;
            return;
        }
        if self.loading_file.take().is_some() {
            self.file_load_task = Task::ready(());
        }
        let shown_view = self
            .existing_diff_view(cx)
            .filter(|view| view.read(cx).shown_path() == Some(&selected_entry.repo_path));
        match shown_view {
            Some(view) => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.activate_item(&view, false, false, window, cx);
                    });
                }
            }
            None => self.load_file(selected_entry, true, window, cx),
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
        let collapsed = if self.has_filter() {
            &mut self.filtered_collapsed_directories
        } else {
            &mut self.collapsed_directories
        };
        if !collapsed.remove(&path) {
            collapsed.insert(path.clone());
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
        }
        self.confirm(window, cx);
        cx.notify();
    }

    fn load_file(
        &mut self,
        entry: ComparisonEntry,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(comparison) = self.comparison.clone() else {
            return;
        };
        self.loading_file = Some(LoadingFile {
            file: ComparedFile {
                entry: entry.clone(),
                compared_commit: comparison.read(cx).compared_commit(),
                mode: comparison.read(cx).mode(),
            },
            activate_when_loaded: activate,
        });
        self.file_load_task = cx.spawn_in(window, async move |this, cx| {
            let Ok(load) = this.update_in(cx, |_, window, cx| {
                comparison.update(cx, |comparison, cx| {
                    comparison.load_file(&entry, window, cx)
                })
            }) else {
                return;
            };
            let result = load.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_file_load(&comparison, result, window, cx);
            })
            .ok();
        });
    }

    fn finish_file_load(
        &mut self,
        comparison: &Entity<BranchComparison>,
        result: Result<LoadedCompareFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(loading) = self.loading_file.take() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let view = if loading.activate_when_loaded {
            self.diff_view_or_create(&workspace, window, cx)
        } else {
            let Some(view) = self.existing_diff_view(cx) else {
                return;
            };
            view
        };
        let comparison = comparison.read(cx);
        let title = comparison.title();
        self.shown = Some(loading.file);
        view.update(cx, |view, cx| match result {
            Ok(file) => view.show_file(file, title, window, cx),
            Err(error) => view.show_error(format!("{error:#}").into(), title, cx),
        });
        if loading.activate_when_loaded {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&view, false, false, window, cx);
            });
        }
    }

    fn existing_diff_view(&self, cx: &gpui::App) -> Option<Entity<CompareDiffView>> {
        let view = self.diff_view.upgrade()?;
        self.workspace.upgrade()?.read(cx).pane_for(&view)?;
        Some(view)
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
        if self.has_filter() && self.rows.is_empty() {
            return Self::render_message("No matching files".into());
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
            .when(self.filter_visible, |this| {
                this.children(self.filter_editor.as_ref().map(|editor| {
                    h_flex()
                        .h(rems(1.75))
                        .px_2()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(Icon::new(IconName::MagnifyingGlass).size(IconSize::Small))
                        .child(div().flex_1().min_w_0().child(editor.clone()))
                        .child(
                            IconButton::new("clear-compare-filter", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Clear Filter"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ClearCompareFilter.boxed_clone(), cx);
                                }),
                        )
                }))
            })
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
    use editor::{DiffViewStyle, Editor, ScrollBeyondLastLine};
    use gpui::{
        KeyBinding, KeyBindingContextPredicate, Modifiers, TestAppContext, VisualTestContext, size,
    };
    use settings::{KeymapFile, SettingsStore};
    use std::time::Duration;
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

    fn bind_compare_keys(cx: &mut VisualTestContext) {
        for source in [
            include_str!("../../../../assets/keymaps/default-linux.json"),
            include_str!("../../../../assets/keymaps/vim.json"),
        ] {
            let keymap = KeymapFile::parse(source).expect("keymap should parse");
            for section in keymap.sections().filter(|section| {
                section.context.starts_with("GitPanel") && section.context.contains("Compare")
            }) {
                cx.update(|_, cx| {
                    let context = KeyBindingContextPredicate::parse(&section.context)
                        .expect("Compare context should parse");
                    let bindings = section
                        .bindings()
                        .map(|(keystrokes, action)| {
                            let (name, input) = KeymapFile::parse_action(action)
                                .expect("Compare action should parse")
                                .expect("Compare binding should have an action");
                            let action = cx
                                .build_action(name, input.cloned())
                                .expect("Compare action should be registered");
                            KeyBinding::load(
                                keystrokes,
                                action,
                                Some(context.clone().into()),
                                false,
                                None,
                                cx.keyboard_mapper().as_ref(),
                            )
                            .expect("Compare binding should load")
                        })
                        .collect::<Vec<_>>();
                    cx.bind_keys(bindings);
                });
            }
        }
    }

    fn editor_scroll_y(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
        editor.update_in(cx, |editor, _, cx| editor.scroll_position(cx).y)
    }

    #[test]
    fn test_matching_entry_indices() {
        let entries = [
            "README.md",
            "src/CompareList.rs",
            "src/compare_diff_view.rs",
            "crates/editor/src/editor.rs",
        ]
        .map(|path| ComparisonEntry {
            repo_path: RepoPath::new(path).expect("path should be valid"),
            status: git::status::FileStatus::Untracked,
            old_side: super::super::comparison::OldSide::Absent,
        });
        for (query, expected) in [
            ("cmp", vec![1, 2]),
            ("cdv", vec![2]),
            ("crates/ed", vec![3]),
            ("ComL", vec![1]),
            ("SRC", vec![1, 2, 3]),
            ("missing", vec![]),
            ("", vec![0, 1, 2, 3]),
        ] {
            assert_eq!(
                matching_entry_indices(&entries, query),
                expected.into_iter().collect::<HashSet<_>>(),
                "query: {query}"
            );
        }
    }

    #[gpui::test]
    async fn test_compare_filter_keyboard_flow(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("main", cx).await;
        let dot_git = std::path::Path::new(util::path!("/project/.git"));
        let long_text = (0..200)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        fs.set_head_for_repo(
            dot_git,
            &[
                ("src/added.rs", long_text),
                ("src/changed.rs", "fn new() {}\n".into()),
            ],
            super::super::comparison::tests::FEATURE_SHA,
        );
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        bind_compare_keys(cx);
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
        let view = diff_views(&workspace, cx)
            .into_iter()
            .next()
            .expect("comparison should display a diff");
        let right_editor =
            view.read_with(cx, |view, cx| view.editor().read(cx).rhs_editor().clone());
        let src = RepoPath::new("src").expect("path should be valid");
        cx.simulate_keystrokes("k h");
        list.read_with(cx, |list, _| {
            assert!(list.collapsed_directories.contains(&src));
            assert_eq!(list.rows.len(), 1);
        });

        cx.simulate_keystrokes("/");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert!(!list.read_with(cx, |list, _| list.filter_is_visible()));

        cx.simulate_keystrokes("/");
        assert!(cx.update(|window, cx| { list.read(cx).filter_is_focused(window, cx) }));
        cx.simulate_keystrokes("j k h l /");
        list.read_with(cx, |list, _| {
            assert_eq!(list.filter_query, "jkhl/");
            assert!(list.rows.is_empty());
            assert_eq!(list.selected_row, None);
            assert_eq!(list.entries.len(), 3);
        });
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        cx.simulate_keystrokes("enter enter");
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        cx.simulate_keystrokes("escape");
        list.read_with(cx, |list, _| {
            assert!(!list.has_filter());
            assert!(!list.filter_visible);
            assert_eq!(list.rows.len(), 1);
        });

        cx.simulate_keystrokes("/");
        cx.simulate_input("src/chg");
        cx.simulate_keystrokes("enter");
        list.read_with(cx, |list, _| {
            assert_eq!(list.filter_query, "src/chg");
            assert_eq!(list.rows.len(), 2);
            assert!(list.collapsed_directories.contains(&src));
            assert!(list.filtered_collapsed_directories.is_empty());
        });
        assert!(!cx.update(|window, cx| { list.read(cx).filter_is_focused(window, cx) }));
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        cx.simulate_keystrokes("ctrl-f");
        assert!(editor_scroll_y(&right_editor, cx) > 0.);
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));

        cx.dispatch_action(crate::git_panel::ActivateChangesTab);
        cx.run_until_parked();
        cx.simulate_keystrokes("/");
        assert!(!cx.update(|window, cx| { list.read(cx).filter_is_focused(window, cx) }));
        cx.dispatch_action(super::super::ActivateCompareTab);
        cx.run_until_parked();
        assert_eq!(
            list.read_with(cx, |list, _| list.filter_query.clone()),
            "src/chg"
        );

        cx.simulate_keystrokes("h");
        assert_eq!(list.read_with(cx, |list, _| list.rows.len()), 1);
        cx.simulate_keystrokes("l j enter");
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
        assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);
        assert!(cx.update(|window, cx| { panel.focus_handle(cx).contains_focused(window, cx) }));
        cx.simulate_keystrokes("escape");
        list.read_with(cx, |list, _| {
            assert!(!list.has_filter());
            assert!(!list.filter_visible);
            assert!(list.collapsed_directories.contains(&src));
            assert_eq!(list.rows.len(), 1);
        });
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
        cx.simulate_keystrokes("/");
        cx.simulate_input("chg");
        cx.simulate_keystrokes("enter");
        list.update_in(cx, |list, window, cx| {
            list.set_comparison(None, window, cx);
        });
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            assert!(list.comparison.is_none());
            assert!(!list.has_filter());
            assert!(!list.filter_visible);
            assert!(list.rows.is_empty());
        });
    }

    #[gpui::test]
    async fn test_refresh_keeps_the_displayed_file(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("main", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        bind_compare_keys(cx);
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
        let view = diff_views(&workspace, cx)
            .into_iter()
            .next()
            .expect("comparison should display a diff");
        let right_editor =
            view.read_with(cx, |view, cx| view.editor().read(cx).rhs_editor().clone());
        let list = panel.read_with(cx, |panel, _| panel.compare_list().clone());
        cx.simulate_keystrokes("j /");
        cx.simulate_input("chg");
        cx.simulate_keystrokes("enter");
        let assert_selection = |cx: &mut VisualTestContext| {
            assert_eq!(
                list.read_with(cx, |list, _| list.filter_query.clone()),
                "chg"
            );
            assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
            assert_eq!(
                list.read_with(cx, |list, _| {
                    list.selected_entry()
                        .map(|entry| entry.repo_path.as_unix_str().to_string())
                })
                .as_deref(),
                Some("src/changed.rs")
            );
            assert!(
                cx.update(|window, cx| { panel.focus_handle(cx).contains_focused(window, cx) })
            );
        };
        let settle_refresh = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
        };
        let dot_git = std::path::Path::new(util::path!("/project/.git"));
        for (branch_name, read_only) in [("feature", false), ("main", true)] {
            fs.set_branch_name(dot_git, Some(branch_name));
            settle_refresh(cx);
            assert_selection(cx);
            assert_eq!(
                right_editor.read_with(cx, |editor, cx| editor.read_only(cx)),
                read_only
            );
        }

        let refresh_head = |contents: &[(&str, String)], sha: &str, cx: &mut VisualTestContext| {
            fs.set_head_for_repo(dot_git, contents, sha);
            fs.with_git_state(dot_git, true, |state| {
                state.refs.insert("refs/heads/feature".into(), sha.into());
            })
            .expect("repository should exist");
            settle_refresh(cx);
        };
        let other_editor = cx.new_window_entity(Editor::single_line);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(other_editor.clone()),
                None,
                true,
                window,
                cx,
            );
            workspace.focus_panel::<GitPanel>(window, cx);
        });
        refresh_head(
            &[
                ("src/added.rs", "updated added\n".into()),
                ("src/changed.rs", "updated changed\n".into()),
            ],
            "3333333333333333333333333333333333333333",
            cx,
        );
        assert_selection(cx);
        assert_eq!(
            right_editor.read_with(cx, |editor, cx| {
                editor.buffer().read(cx).snapshot(cx).text()
            }),
            "updated added\n"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .map(|item| item.item_id())
            }),
            Some(other_editor.item_id()),
            "refreshing a hidden diff must not activate it"
        );

        refresh_head(
            &[("src/changed.rs", "updated changed\n".into())],
            "4444444444444444444444444444444444444444",
            cx,
        );
        assert_selection(cx);
        assert_eq!(
            diff_views(&workspace, cx),
            vec![view.clone()],
            "removing the displayed path keeps the existing view"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.remove_item(view.item_id(), false, false, window, cx);
            });
        });
        refresh_head(
            &[
                ("src/added.rs", "added again\n".into()),
                ("src/changed.rs", "updated changed\n".into()),
            ],
            "5555555555555555555555555555555555555555",
            cx,
        );
        assert!(diff_views(&workspace, cx).is_empty());
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .map(|item| item.item_id())
            }),
            Some(other_editor.item_id())
        );
        workspace.update_in(cx, |workspace, window, cx| {
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
        list.read_with(cx, |list, _| {
            assert!(!list.has_filter());
            assert!(!list.filter_visible);
            assert_eq!(list.entries.len(), 3);
        });
    }

    #[gpui::test]
    async fn test_scroll_diff_from_compare_list(cx: &mut TestAppContext) {
        for diff_view_style in [DiffViewStyle::Unified, DiffViewStyle::Split] {
            let (project, repository, fs) = setup("main", cx).await;
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(diff_view_style);
                    settings.editor.scroll_beyond_last_line = Some(ScrollBeyondLastLine::Off);
                });
            });
            let old_text = (0..200)
                .map(|index| format!("line {index}\n"))
                .collect::<String>();
            let new_text = old_text.replacen("line 0", "changed line 0", 1);
            let dot_git = std::path::Path::new(util::path!("/project/.git"));
            fs.set_merge_base_content_for_repo(dot_git, &[("src/changed.rs", old_text)]);
            fs.set_head_for_repo(
                dot_git,
                &[
                    ("src/changed.rs", new_text),
                    ("src/other.rs", "other file\n".into()),
                ],
                super::super::comparison::tests::FEATURE_SHA,
            );
            let window_handle =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window_handle
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .expect("workspace should exist");
            let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
            cx.simulate_resize(size(px(1600.), px(900.)));
            bind_compare_keys(cx);
            let panel = workspace.update_in(cx, GitPanel::new_test);
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_panel(panel.clone(), window, cx);
                workspace.focus_panel::<GitPanel>(window, cx);
                panel.update(cx, |panel, cx| panel.activate_compare_tab(window, cx));
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("ctrl-f ctrl-b");
            assert!(diff_views(&workspace, cx).is_empty());

            workspace.update_in(cx, |workspace, window, cx| {
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
            let view = diff_views(&workspace, cx)
                .into_iter()
                .next()
                .expect("comparison should display a diff");
            let editor = view.read_with(cx, |view, _| view.editor().clone());
            let (right_editor, left_editor) = editor.read_with(cx, |editor, _| {
                (editor.rhs_editor().clone(), editor.lhs_editor().cloned())
            });
            assert_eq!(
                left_editor.is_some(),
                diff_view_style == DiffViewStyle::Split
            );
            right_editor.update_in(cx, |editor, window, cx| {
                editor.set_scroll_position(point(0., 0.), window, cx);
            });
            cx.run_until_parked();
            let visible_line_count = right_editor.read_with(cx, |editor, _| {
                editor
                    .visible_line_count()
                    .expect("editor should be laid out")
            });
            assert!((2.0..200.0).contains(&visible_line_count));
            let selections =
                right_editor.read_with(cx, |editor, _| editor.selections.disjoint_anchors_arc());
            let list = panel.read_with(cx, |panel, _| panel.compare_list().clone());
            let sidebar_state = |cx: &mut VisualTestContext| {
                list.read_with(cx, |list, _| {
                    (
                        list.selected_row,
                        list.scroll_handle.0.borrow().base_handle.offset(),
                    )
                })
            };
            cx.simulate_keystrokes("j");
            assert_eq!(sidebar_state(cx).0, Some(2));
            assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
            let initial_sidebar_state = sidebar_state(cx);
            let assert_unchanged = |cx: &mut VisualTestContext| {
                assert_eq!(sidebar_state(cx), initial_sidebar_state);
                assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);
                assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
                assert_eq!(
                    right_editor
                        .read_with(cx, |editor, _| { editor.selections.disjoint_anchors_arc() }),
                    selections
                );
                assert!(
                    cx.update(|window, cx| { panel.focus_handle(cx).contains_focused(window, cx) })
                );
            };

            cx.simulate_keystrokes("ctrl-f");
            let scrolled = editor_scroll_y(&right_editor, cx);
            assert!((scrolled - (visible_line_count / 2.).trunc()).abs() < 1.);
            assert_unchanged(cx);
            if let Some(left_editor) = &left_editor {
                assert_eq!(editor_scroll_y(left_editor, cx), scrolled);
            }
            cx.simulate_keystrokes("ctrl-b");
            assert_eq!(editor_scroll_y(&right_editor, cx), 0.);
            assert_unchanged(cx);

            cx.simulate_keystrokes(&"ctrl-f ".repeat(20));
            let bottom = editor_scroll_y(&right_editor, cx);
            assert!(bottom > scrolled);
            cx.simulate_keystrokes("ctrl-f");
            assert_eq!(editor_scroll_y(&right_editor, cx), bottom);
            cx.simulate_keystrokes(&"ctrl-b ".repeat(20));
            assert_eq!(editor_scroll_y(&right_editor, cx), 0.);
            cx.simulate_keystrokes("ctrl-b");
            assert_eq!(editor_scroll_y(&right_editor, cx), 0.);
            assert_unchanged(cx);

            cx.simulate_keystrokes("k ctrl-f");
            let confirmed_scroll = editor_scroll_y(&right_editor, cx);
            cx.simulate_keystrokes("g f");
            assert_eq!(editor_scroll_y(&right_editor, cx), confirmed_scroll);
            assert!(
                cx.update(|window, cx| { panel.focus_handle(cx).contains_focused(window, cx) })
            );

            cx.dispatch_action(menu::SelectPrevious);
            cx.run_until_parked();
            assert_eq!(sidebar_state(cx).0, Some(0));
            cx.simulate_keystrokes("ctrl-f");
            assert!(editor_scroll_y(&right_editor, cx) > 0.);
            assert_eq!(sidebar_state(cx).0, Some(0));
            assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));

            let other_editor = cx.new_window_entity(Editor::single_line);
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(
                    Box::new(other_editor.clone()),
                    None,
                    true,
                    window,
                    cx,
                );
                workspace.focus_panel::<GitPanel>(window, cx);
            });
            cx.run_until_parked();
            let hidden_scroll = editor_scroll_y(&right_editor, cx);
            cx.simulate_keystrokes("ctrl-f");
            assert_eq!(editor_scroll_y(&right_editor, cx), hidden_scroll);
            assert_eq!(
                workspace.read_with(cx, |workspace, cx| {
                    workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .map(|item| item.item_id())
                }),
                Some(other_editor.item_id())
            );
            cx.simulate_keystrokes("ctrl-b");
            assert_eq!(editor_scroll_y(&right_editor, cx), hidden_scroll);
            workspace.update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.remove_item(other_editor.item_id(), false, false, window, cx);
                });
            });
            cx.run_until_parked();

            view.update(cx, |view, cx| {
                view.show_error("Cannot load file".into(), "feature since main".into(), cx);
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("ctrl-f ctrl-b");
            assert_eq!(shown_path(&view, cx), None);
            assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);

            workspace.update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.remove_item(view.item_id(), false, false, window, cx);
                });
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("ctrl-f ctrl-b");
            assert!(diff_views(&workspace, cx).is_empty());
            assert!(
                cx.update(|window, cx| { panel.focus_handle(cx).contains_focused(window, cx) })
            );
        }
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
                    .map(|file| file.entry.repo_path.as_unix_str().to_string())
            })
        };
        let settle = |cx: &mut VisualTestContext| {
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
        assert_eq!(shown(cx).as_deref(), Some("src/added.rs"));

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
    async fn test_compare_tab_shows_confirmed_file_in_one_tab(cx: &mut TestAppContext) {
        let (project, repository, fs) = setup("main", cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        bind_compare_keys(cx);
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
        let view = views
            .into_iter()
            .next()
            .expect("comparison should display a diff");
        let list = panel.read_with(cx, |panel, _| panel.compare_list().clone());
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        let panel_has_focus = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panel.focus_handle(cx).contains_focused(window, cx))
        };
        assert!(panel_has_focus(cx), "focus stays in the panel");

        cx.simulate_keystrokes("j shift-g g g j j");
        assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        assert_eq!(list.read_with(cx, |list, _| list.selected_row), Some(2));
        assert!(
            panel_has_focus(cx),
            "moving the selection keeps focus in the panel"
        );

        let dot_git = std::path::Path::new(util::path!("/project/.git"));
        let gate = fs.install_blob_read_gate_for_repo(dot_git);
        cx.simulate_keystrokes("g f");
        assert_eq!(gate.waiting(), 1);
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        cx.simulate_keystrokes("j");
        assert_eq!(list.read_with(cx, |list, _| list.selected_row), Some(3));
        assert_eq!(gate.waiting(), 1, "navigation keeps the confirmed load");
        cx.simulate_keystrokes("/");
        cx.simulate_input("deleted");
        cx.simulate_keystrokes("enter");
        assert_eq!(gate.waiting(), 1, "filtering keeps the confirmed load");
        gate.open();
        cx.run_until_parked();
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/changed.rs"));
        assert!(panel_has_focus(cx), "confirming keeps focus in the panel");
        cx.simulate_keystrokes("escape");

        let gate = fs.install_blob_read_gate_for_repo(dot_git);
        cx.simulate_keystrokes("g f");
        assert_eq!(gate.waiting(), 1);
        cx.simulate_keystrokes("k g f");
        gate.open();
        cx.run_until_parked();
        assert_eq!(
            shown_path(&view, cx).as_deref(),
            Some("src/changed.rs"),
            "confirming the displayed file cancels another pending load"
        );

        list.update_in(cx, |list, window, cx| {
            list.select_first(window, cx);
            list.select_next(window, cx);
            list.confirm(window, cx);
            list.select_last(window, cx);
            list.confirm(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/deleted.rs"));
        assert_eq!(diff_views(&workspace, cx), vec![view.clone()]);

        let file_position = list.read_with(cx, |list, _| {
            let state = list.scroll_handle.0.borrow();
            let bounds = state.base_handle.bounds();
            let row_height = state
                .last_item_size
                .expect("list should be laid out")
                .contents
                .height
                / list.rows.len() as f32;
            point(
                bounds.left() + px(40.),
                bounds.top() + state.base_handle.offset().y + row_height * 1.5,
            )
        });
        cx.simulate_click(file_position, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(shown_path(&view, cx).as_deref(), Some("src/added.rs"));
        assert!(panel_has_focus(cx), "clicking keeps focus in the panel");

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.remove_item(view.item_id(), false, false, window, cx)
            });
            workspace.focus_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(menu::SelectNext);
        cx.run_until_parked();
        assert!(diff_views(&workspace, cx).is_empty());
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        let views = diff_views(&workspace, cx);
        assert_eq!(views.len(), 1, "confirmation reopens a closed diff tab");
        assert_ne!(views[0], view);
        assert_eq!(shown_path(&views[0], cx).as_deref(), Some("src/changed.rs"));
        assert!(panel_has_focus(cx));
    }
}
