//! Comparing two branches: pick a base and a compared branch, then browse the files changed on the
//! compared branch since it split off from the base, one file at a time. Also compares commits from
//! one file's history.

mod branch_pair_picker;
mod compare_diff_view;
mod compare_list;
mod comparison;
mod file_commit_picker;
mod file_tree;

use anyhow::{Context as _, Result, ensure};
use editor::Editor;
use git::{blame::Blame, repository::Branch};
use gpui::{App, AppContext as _, Context, Focusable as _, Window, actions};
use language::Point;
use project::File;
use util::ResultExt as _;
use workspace::Workspace;

use crate::git_panel::GitPanel;
use branch_pair_picker::BranchPairPicker;
#[cfg(test)]
pub(crate) use compare_diff_view::CompareDiffView;
pub(crate) use compare_list::CompareList;
use compare_list::VerticalDirection;
pub(crate) use comparison::{BranchComparison, LocalGitObjects};
use comparison::{CompareLocation, REMOTE_NOT_SUPPORTED};
use file_commit_picker::{FileCommitPicker, open_file_comparison};

actions!(
    git,
    [
        /// Picks two branches and lists the files changed on the second one since it split off
        /// from the first in the git panel's Compare tab.
        CompareBranches,
        /// Shows the current line's blamed commit in the git panel's Compare tab.
        CompareBlameCommit,
        /// Picks commits that changed the active file and shows how the file differs between two
        /// of them, or between one of them and the current file.
        CompareFileCommits,
    ]
);

actions!(
    git_panel,
    [
        /// Activates the Compare tab in the git panel.
        ActivateCompareTab,
        /// Focuses the file filter in the Compare tab.
        FocusCompareFilter,
        /// Returns to the Compare list while keeping its file filter.
        FinishCompareFilter,
        /// Clears the Compare tab's file filter and returns to the list.
        ClearCompareFilter,
        /// Expands the selected folder in the Compare tab, or selects the next row.
        ExpandCompareEntry,
        /// Collapses the selected folder or its nearest open parent in the Compare tab.
        CollapseCompareEntry,
        /// Scrolls the Compare tab's list down by half a page without changing the selection.
        ScrollCompareListDown,
        /// Scrolls the Compare tab's list up by half a page without changing the selection.
        ScrollCompareListUp,
        /// Scrolls the displayed comparison diff down by half a page without moving focus.
        ScrollCompareDiffDown,
        /// Scrolls the displayed comparison diff up by half a page without moving focus.
        ScrollCompareDiffUp,
        /// Moves the Compare tab's selection down by half a page.
        SelectCompareHalfPageDown,
        /// Moves the Compare tab's selection up by half a page.
        SelectCompareHalfPageUp,
    ]
);

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(compare_branches);
    workspace.register_action(compare_blame_commit);
    workspace.register_action(compare_file_commits);
    workspace.register_action(|workspace, _: &ActivateCompareTab, window, cx| {
        let Some(panel) = workspace.panel::<GitPanel>(cx) else {
            return;
        };
        workspace.focus_panel::<GitPanel>(window, cx);
        panel.update(cx, |panel, cx| panel.activate_compare_tab(window, cx));
    });
    workspace.register_action(|workspace, _: &FocusCompareFilter, window, cx| {
        let Some(panel) = workspace.panel::<GitPanel>(cx) else {
            return;
        };
        workspace.focus_panel::<GitPanel>(window, cx);
        panel.update(cx, |panel, cx| panel.activate_compare_tab(window, cx));
        update_compare_list(workspace, cx, |list, cx| list.focus_filter(window, cx));
    });
    workspace.register_action(|workspace, _: &FinishCompareFilter, window, cx| {
        update_compare_list(workspace, cx, |list, cx| list.finish_filter(window, cx));
        workspace.focus_panel::<GitPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &ClearCompareFilter, window, cx| {
        update_compare_list(workspace, cx, |list, cx| list.clear_filter(window, cx));
        workspace.focus_panel::<GitPanel>(window, cx);
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
    workspace.register_action(|workspace, _: &ScrollCompareDiffDown, window, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.scroll_diff(VerticalDirection::Down, workspace, window, cx)
        });
    });
    workspace.register_action(|workspace, _: &ScrollCompareDiffUp, window, cx| {
        update_compare_list(workspace, cx, |list, cx| {
            list.scroll_diff(VerticalDirection::Up, workspace, window, cx)
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

fn blame_commit_location(blame: &Blame, row: u32) -> Result<(git::Oid, CompareLocation)> {
    let entry = blame
        .entries
        .iter()
        .find(|entry| entry.range.contains(&row))
        .context("This line has no committed history")?;
    ensure!(!entry.sha.is_zero(), "This line has no committed history");
    Ok((
        entry.sha,
        CompareLocation {
            repo_path: git::repository::RepoPath::new(&entry.filename)?,
            row: entry
                .original_line_number
                .saturating_sub(1)
                .saturating_add(row.saturating_sub(entry.range.start)),
        },
    ))
}

fn compare_blame_commit(
    workspace: &mut Workspace,
    _: &CompareBlameCommit,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<GitPanel>(cx) else {
        workspace.show_error("The git panel is not available", cx);
        return;
    };
    let compare_list = panel.read(cx).compare_list().clone();
    compare_list.update(cx, |list, _| list.cancel_blame_commit_lookup());
    let project = workspace.project().clone();
    let target = (|| -> Result<_> {
        ensure!(
            project.read(cx).is_local(),
            "Comparing blame commits is not supported for remote projects"
        );
        let editor = workspace
            .active_item_as::<Editor>(cx)
            .context("Open a working file to compare its blamed commit")?;
        let origin = editor.focus_handle(cx);
        ensure!(
            origin.contains_focused(window, cx),
            "Focus the working file to compare its blamed commit"
        );
        let snapshot = editor.update(cx, |editor, cx| editor.snapshot(window, cx));
        let cursor = editor
            .read(cx)
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let (buffer_snapshot, point) = snapshot
            .buffer_snapshot()
            .point_to_buffer_point(cursor)
            .context("The cursor is not on a file line")?;
        let buffer = editor
            .read(cx)
            .buffer()
            .read(cx)
            .buffer(buffer_snapshot.remote_id())
            .context("The file is no longer available")?;
        ensure!(
            File::from_dyn(buffer.read(cx).file()).is_some(),
            "This command supports working files, not historical diffs"
        );
        let git_store = project.read(cx).git_store();
        let (repository, _) = git_store
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_snapshot.remote_id(), cx)
            .context("The current file is not in a Git repository")?;
        Ok((buffer, point.row, repository, origin))
    })();
    let (buffer, row, repository, origin) = match target {
        Ok(target) => target,
        Err(error) => {
            workspace.show_error(error, cx);
            return;
        }
    };
    let blame = project.update(cx, |project, cx| project.blame_buffer(&buffer, None, cx));
    let source_item = workspace.active_item(cx).map(|item| item.item_id());
    let origin_weak = origin.downgrade();
    let list = compare_list.downgrade();
    let task = cx.spawn_in(window, async move |workspace, cx| {
        let result = async {
            let blame = blame.await?.context("No blame information is available for this file")?;
            let (sha, location) = blame_commit_location(&blame, row)?;
            let (details, diff) = repository.read_with(cx, |repository, cx| (
                repository.show_commit(sha.to_string(), cx),
                repository.load_commit_diff(sha.to_string(), false, cx),
            ));
            let (details, diff) = futures::try_join!(details, diff)?;
            ensure!(!diff.is_shallow_boundary,
                "This commit is at a shallow history boundary. Fetch its parent history to view its changes");
            ensure!(diff.files.iter().any(|file| file.path == location.repo_path),
                "The blamed file is not present in this commit's changes");
            anyhow::Ok((sha, location, details, diff))
        }.await;
        let current = workspace.update_in(cx, |workspace, window, cx| {
            origin_weak.upgrade().is_some_and(|origin| origin.contains_focused(window, cx))
                && workspace.active_item(cx).map(|item| item.item_id()) == source_item
        });
        if !matches!(current, Ok(true)) { return; }
        if let Err(error) = list.update(cx, |list, _| list.finish_blame_commit_lookup()) {
            log::debug!("Blame comparison was cancelled: {error:#}");
            return;
        }
        match result {
            Err(error) => { workspace.update(cx, |workspace, cx| workspace.show_error(error, cx)).log_err(); }
            Ok((sha, location, details, diff)) => {
                let comparison = workspace.update_in(cx, |workspace, window, cx| {
                    let comparison = cx.new(|_| BranchComparison::for_commit(project, repository, sha, details, diff));
                    workspace.focus_panel::<GitPanel>(window, cx);
                    panel.update(cx, |panel, cx| panel.activate_compare_tab(window, cx));
                    comparison
                });
                match comparison {
                    Ok(comparison) => { list.update_in(cx, |list, window, cx| {
                        list.set_commit_comparison(comparison, location, window, cx);
                    }).log_err(); },
                    Err(error) => { log::debug!("Blame comparison workspace was closed: {error:#}"); }
                };
            }
        }
    });
    compare_list.update(cx, |list, cx| {
        list.set_blame_commit_task(task, &origin, window, cx)
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

fn compare_file_commits(
    workspace: &mut Workspace,
    _: &CompareFileCommits,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().read(cx);
    if !project.is_local() {
        workspace.show_error(REMOTE_NOT_SUPPORTED, cx);
        return;
    }
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        workspace.show_error("No file is open", cx);
        return;
    };
    let Some(file) = editor
        .read(cx)
        .file_at(editor.read(cx).selections.newest_anchor().head(), cx)
    else {
        workspace.show_error("No file is open", cx);
        return;
    };
    let project_path = project::ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    let Some((repository, repo_path)) = project
        .git_store()
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)
    else {
        workspace.show_error("The file isn't in a git repository", cx);
        return;
    };

    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        FileCommitPicker::new(
            repository.clone(),
            repo_path.clone(),
            move |comparison, window, cx| {
                workspace_handle
                    .update(cx, |workspace, cx| {
                        open_file_comparison(
                            workspace,
                            repository.clone(),
                            repo_path.clone(),
                            comparison,
                            window,
                            cx,
                        )
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
