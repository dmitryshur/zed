//! Comparing two branches: pick a base and a compared branch, then browse the files changed on the
//! compared branch since it split off from the base, one file at a time.

mod branch_pair_picker;
mod compare_diff_view;
mod compare_list;
mod comparison;
mod file_tree;

use git::repository::Branch;
use gpui::{App, AppContext as _, Context, Window, actions};
use workspace::Workspace;

use crate::git_panel::GitPanel;
use branch_pair_picker::BranchPairPicker;
pub(crate) use compare_list::CompareList;
use compare_list::VerticalDirection;
pub(crate) use comparison::BranchComparison;
use comparison::REMOTE_NOT_SUPPORTED;

actions!(
    git,
    [
        /// Picks two branches and lists the files changed on the second one since it split off
        /// from the first in the git panel's Compare tab.
        CompareBranches,
    ]
);

actions!(
    git_panel,
    [
        /// Activates the Compare tab in the git panel.
        ActivateCompareTab,
        /// Expands the selected folder in the Compare tab, or selects the next row.
        ExpandCompareEntry,
        /// Collapses the selected folder or its nearest open parent in the Compare tab.
        CollapseCompareEntry,
        /// Scrolls the Compare tab's list down by half a page without changing the selection.
        ScrollCompareListDown,
        /// Scrolls the Compare tab's list up by half a page without changing the selection.
        ScrollCompareListUp,
        /// Moves the Compare tab's selection down by half a page.
        SelectCompareHalfPageDown,
        /// Moves the Compare tab's selection up by half a page.
        SelectCompareHalfPageUp,
    ]
);

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(compare_branches);
    workspace.register_action(|workspace, _: &ActivateCompareTab, window, cx| {
        let Some(panel) = workspace.panel::<GitPanel>(cx) else {
            return;
        };
        workspace.focus_panel::<GitPanel>(window, cx);
        panel.update(cx, |panel, cx| panel.activate_compare_tab(window, cx));
    });
    workspace.register_action(|workspace, _: &ExpandCompareEntry, window, cx| {
        update_compare_list(workspace, cx, |list, cx| list.expand_selected(window, cx));
    });
    workspace.register_action(|workspace, _: &CollapseCompareEntry, window, cx| {
        update_compare_list(workspace, cx, |list, cx| list.collapse_selected(window, cx));
    });
    workspace.register_action(|workspace, _: &ScrollCompareListDown, _, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.scroll_half_page(VerticalDirection::Down, cx)
        });
    });
    workspace.register_action(|workspace, _: &ScrollCompareListUp, _, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.scroll_half_page(VerticalDirection::Up, cx)
        });
    });
    workspace.register_action(|workspace, _: &SelectCompareHalfPageDown, window, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.select_half_page(VerticalDirection::Down, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &SelectCompareHalfPageUp, window, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.select_half_page(VerticalDirection::Up, window, cx)
        });
    });
}

/// The Compare tab's actions are only bound while the tab has focus, so they can be handled at
/// the workspace level.
fn update_compare_list(
    workspace: &Workspace,
    cx: &mut App,
    update: impl FnOnce(&mut CompareList, &mut Context<CompareList>),
) {
    let Some(panel) = workspace.panel::<GitPanel>(cx) else {
        return;
    };
    let compare_list = panel.read(cx).compare_list().clone();
    compare_list.update(cx, update);
}

fn compare_branches(
    workspace: &mut Workspace,
    _: &CompareBranches,
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

    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        BranchPairPicker::new(
            repository.clone(),
            move |base, compared, window, cx| {
                workspace_handle
                    .update(cx, |workspace, cx| {
                        start_comparison(workspace, repository.clone(), base, compared, window, cx)
                    })
                    .ok();
            },
            window,
            cx,
        )
    });
}

fn start_comparison(
    workspace: &mut Workspace,
    repository: gpui::Entity<project::git_store::Repository>,
    base: Branch,
    compared: Branch,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<GitPanel>(cx) else {
        workspace.show_error("The git panel is not available", cx);
        return;
    };
    let project = workspace.project().clone();
    let comparison = cx.new(|cx| BranchComparison::new(project, repository, base, compared, cx));
    workspace.focus_panel::<GitPanel>(window, cx);
    panel.update(cx, |panel, cx| {
        panel.show_comparison(comparison, window, cx)
    });
}
