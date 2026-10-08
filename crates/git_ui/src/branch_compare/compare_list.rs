use std::{ops::Range, sync::Arc, time::Duration};

use anyhow::Result;
use git::Oid;
use gpui::{
    Action as _, AppContext as _, Context, Entity, ScrollStrategy, SharedString, Subscription,
    Task, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::paths::PathStyle;
use workspace::Workspace;

use super::{
    CompareBranches,
    compare_diff_view::CompareDiffView,
    comparison::{
        BranchComparison, ComparisonEntry, ComparisonEvent, ComparisonState, LoadedCompareFile,
    },
};
use crate::git_status_icon;

/// Moving through the list loads each file; waiting briefly avoids loading every file passed
/// over while holding a navigation key.
const SELECTION_LOAD_DEBOUNCE: Duration = Duration::from_millis(50);

struct LoadingFile {
    entry: ComparisonEntry,
    focus_when_loaded: bool,
}

/// The contents of the git panel's Compare tab: the files of a branch comparison. The selected
/// file is shown in a single [`CompareDiffView`] tab.
pub(crate) struct CompareList {
    workspace: WeakEntity<Workspace>,
    comparison: Option<Entity<BranchComparison>>,
    selected_index: Option<usize>,
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
            selected_index: None,
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
        self.selected_index = None;
        self.loading_file = None;
        self.shown = None;
        self.file_load_task = Task::ready(());
        cx.notify();
    }

    fn entries(&self, cx: &App) -> Option<Arc<[ComparisonEntry]>> {
        match self.comparison.as_ref()?.read(cx).state() {
            ComparisonState::Loaded(entries) => Some(entries.clone()),
            ComparisonState::Loading | ComparisonState::Failed(_) => None,
        }
    }

    fn selected_entry(&self, cx: &App) -> Option<ComparisonEntry> {
        let entries = self.entries(cx)?;
        entries.get(self.selected_index?).cloned()
    }

    fn handle_comparison_event(
        &mut self,
        event: &ComparisonEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.entries(cx).unwrap_or_default();
        let previously_selected = self
            .loading_file
            .as_ref()
            .map(|loading| loading.entry.repo_path.clone())
            .or_else(|| {
                self.shown
                    .as_ref()
                    .map(|(entry, _)| entry.repo_path.clone())
            });
        self.selected_index = if entries.is_empty() {
            None
        } else {
            let selected_path_index = previously_selected.as_ref().and_then(|selected_path| {
                entries
                    .iter()
                    .position(|entry| &entry.repo_path == selected_path)
            });
            Some(
                selected_path_index
                    .or(self.selected_index)
                    .unwrap_or(0)
                    .min(entries.len() - 1),
            )
        };

        if let Some(selected_entry) = self.selected_entry(cx) {
            let compared_commit = self
                .comparison
                .as_ref()
                .and_then(|comparison| comparison.read(cx).compared_commit());
            let needs_reload = matches!(event, ComparisonEvent::ModeChanged)
                || self.shown.as_ref() != Some(&(selected_entry, compared_commit));
            let is_loading_selected = self.loading_file.as_ref().is_some_and(|loading| {
                Some(&loading.entry.repo_path)
                    == self
                        .selected_entry(cx)
                        .as_ref()
                        .map(|entry| &entry.repo_path)
            });
            if needs_reload && !is_loading_selected {
                self.show_selected_file(false, false, window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn select_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(|_, _| Some(0), window, cx);
    }

    pub(crate) fn select_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(|_, count| count.checked_sub(1), window, cx);
    }

    pub(crate) fn select_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(
            |selected, count| match selected {
                Some(selected) => Some((selected + 1).min(count.saturating_sub(1))),
                None => Some(0),
            },
            window,
            cx,
        );
    }

    pub(crate) fn select_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_index(
            |selected, _| Some(selected.map_or(0, |selected| selected.saturating_sub(1))),
            window,
            cx,
        );
    }

    fn select_index(
        &mut self,
        new_index: impl FnOnce(Option<usize>, usize) -> Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.entries(cx).map_or(0, |entries| entries.len());
        if count == 0 {
            return;
        }
        let Some(index) = new_index(self.selected_index, count) else {
            return;
        };
        if self.selected_index == Some(index) {
            return;
        }
        self.selected_index = Some(index);
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Center);
        self.show_selected_file(false, true, window, cx);
        cx.notify();
    }

    pub(crate) fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_entry) = self.selected_entry(cx) else {
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

    fn select_and_confirm(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_index != Some(index) {
            self.selected_index = Some(index);
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
        let (Some(comparison), Some(entry)) = (self.comparison.clone(), self.selected_entry(cx))
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
        let entries = match comparison.read(cx).state() {
            ComparisonState::Loading => return Self::render_message("Comparing…".into()),
            ComparisonState::Failed(message) => return Self::render_message(message.clone()),
            ComparisonState::Loaded(entries) if entries.is_empty() => {
                return Self::render_message("No changes".into());
            }
            ComparisonState::Loaded(entries) => entries.clone(),
        };
        let selected_index = self.selected_index;
        uniform_list(
            "branch-comparison-entries",
            entries.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|index| {
                        let entry = entries.get(index)?;
                        Some(this.render_entry(index, entry, selected_index == Some(index), cx))
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.scroll_handle)
        .size_full()
        .into_any_element()
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &ComparisonEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file_name = entry
            .repo_path
            .file_name()
            .map(ToString::to_string)
            .unwrap_or_default();
        let directory = entry
            .repo_path
            .parent()
            .map(|parent| parent.display(PathStyle::local()).to_string())
            .filter(|directory| !directory.is_empty());
        let full_path: SharedString = entry
            .repo_path
            .display(PathStyle::local())
            .to_string()
            .into();
        ListItem::new(("branch-comparison-entry", index))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .start_slot(git_status_icon(entry.status))
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
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
                    .children(directory.map(|directory| {
                        Label::new(directory)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate_start()
                    })),
            )
            .tooltip(Tooltip::text(full_path))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_and_confirm(index, window, cx);
            }))
            .into_any_element()
    }
}

impl Render for CompareList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
