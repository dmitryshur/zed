use super::*;
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{TestAppContext, VisualTestContext};
use menu::SelectPrevious;
use project::{Project, ProjectPath};
use serde_json::json;
use std::{cell::RefCell, rc::Rc};
use util::{path, rel_path::rel_path};
use workspace::{ActivatePreviousItem, AppState, MultiWorkspace, Workspace, item::test::TestItem};

#[ctor::ctor(unsafe)]
fn init_logger() {
    zlog::init_test();
}

#[gpui::test]
async fn test_open_with_prev_tab_selected_and_cycle_on_toggle_action(
    cx: &mut gpui::TestAppContext,
) {
    let app_state = init_test(cx);

    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "First file",
                "2.txt": "Second file",
                "3.txt": "Third file",
                "4.txt": "Fourth file",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab_1 = open_buffer("1.txt", &workspace, cx).await;
    let tab_2 = open_buffer("2.txt", &workspace, cx).await;
    let tab_3 = open_buffer("3.txt", &workspace, cx).await;
    let tab_4 = open_buffer("4.txt", &workspace, cx).await;

    // Starts with the previously opened item selected
    let tab_switcher = open_tab_switcher(false, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 4);
        assert_match_at_position(tab_switcher, 0, tab_4.boxed_clone());
        assert_match_selection(tab_switcher, 1, tab_3.boxed_clone());
        assert_match_at_position(tab_switcher, 2, tab_2.boxed_clone());
        assert_match_at_position(tab_switcher, 3, tab_1.boxed_clone());
    });

    cx.dispatch_action(Toggle { select_last: false });
    cx.dispatch_action(Toggle { select_last: false });
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 4);
        assert_match_at_position(tab_switcher, 0, tab_4.boxed_clone());
        assert_match_at_position(tab_switcher, 1, tab_3.boxed_clone());
        assert_match_at_position(tab_switcher, 2, tab_2.boxed_clone());
        assert_match_selection(tab_switcher, 3, tab_1.boxed_clone());
    });

    cx.dispatch_action(SelectPrevious);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 4);
        assert_match_at_position(tab_switcher, 0, tab_4.boxed_clone());
        assert_match_at_position(tab_switcher, 1, tab_3.boxed_clone());
        assert_match_selection(tab_switcher, 2, tab_2.boxed_clone());
        assert_match_at_position(tab_switcher, 3, tab_1.boxed_clone());
    });
}

#[gpui::test]
async fn test_open_with_last_tab_selected(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);

    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "First file",
                "2.txt": "Second file",
                "3.txt": "Third file",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab_1 = open_buffer("1.txt", &workspace, cx).await;
    let tab_2 = open_buffer("2.txt", &workspace, cx).await;
    let tab_3 = open_buffer("3.txt", &workspace, cx).await;

    // Starts with the last item selected
    let tab_switcher = open_tab_switcher(true, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 3);
        assert_match_at_position(tab_switcher, 0, tab_3);
        assert_match_at_position(tab_switcher, 1, tab_2);
        assert_match_selection(tab_switcher, 2, tab_1);
    });
}

#[gpui::test]
async fn test_open_item_on_modifiers_release(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);

    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "First file",
                "2.txt": "Second file",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab_1 = open_buffer("1.txt", &workspace, cx).await;
    let tab_2 = open_buffer("2.txt", &workspace, cx).await;

    cx.simulate_modifiers_change(Modifiers::control());
    let tab_switcher = open_tab_switcher(false, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 2);
        assert_match_at_position(tab_switcher, 0, tab_2.boxed_clone());
        assert_match_selection(tab_switcher, 1, tab_1.boxed_clone());
    });

    cx.simulate_modifiers_change(Modifiers::none());
    cx.read(|cx| {
        let active_editor = workspace.read(cx).active_item_as::<Editor>(cx).unwrap();
        assert_eq!(active_editor.read(cx).title(cx), "1.txt");
    });
    assert_tab_switcher_is_closed(workspace, cx);
}

#[gpui::test]
async fn test_open_on_empty_pane(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state.fs.as_fake().insert_tree("/root", json!({})).await;

    let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    cx.simulate_modifiers_change(Modifiers::control());
    let tab_switcher = open_tab_switcher(false, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert!(tab_switcher.delegate.matches.is_empty());
    });

    cx.simulate_modifiers_change(Modifiers::none());
    assert_tab_switcher_is_closed(workspace, cx);
}

#[gpui::test]
async fn test_open_with_single_item(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({"1.txt": "Single file"}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab = open_buffer("1.txt", &workspace, cx).await;

    let tab_switcher = open_tab_switcher(false, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 1);
        assert_match_selection(tab_switcher, 0, tab);
    });
}

#[gpui::test]
async fn test_close_selected_item(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "First file",
                "2.txt": "Second file",
                "3.txt": "Third file",
                "4.txt": "Fourth file",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab_1 = open_buffer("1.txt", &workspace, cx).await;
    let tab_3 = open_buffer("3.txt", &workspace, cx).await;
    let tab_2 = open_buffer("2.txt", &workspace, cx).await;
    let tab_4 = open_buffer("4.txt", &workspace, cx).await;

    // After opening all buffers, let's navigate to the previous item two times, finishing with:
    //
    // 1.txt | [3.txt] | 2.txt | 4.txt
    //
    // With 3.txt being the active item in the pane.
    cx.dispatch_action(ActivatePreviousItem::default());
    cx.dispatch_action(ActivatePreviousItem::default());
    cx.run_until_parked();

    cx.simulate_modifiers_change(Modifiers::control());
    let tab_switcher = open_tab_switcher(false, &workspace, cx);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 4);
        assert_match_at_position(tab_switcher, 0, tab_3.boxed_clone());
        assert_match_selection(tab_switcher, 1, tab_2.boxed_clone());
        assert_match_at_position(tab_switcher, 2, tab_4.boxed_clone());
        assert_match_at_position(tab_switcher, 3, tab_1.boxed_clone());
    });

    cx.simulate_modifiers_change(Modifiers::control());
    cx.dispatch_action(CloseSelectedItem);
    tab_switcher.update(cx, |tab_switcher, _| {
        assert_eq!(tab_switcher.delegate.matches.len(), 3);
        assert_match_selection(tab_switcher, 0, tab_3);
        assert_match_at_position(tab_switcher, 1, tab_4);
        assert_match_at_position(tab_switcher, 2, tab_1);
    });

    // Still switches tab on modifiers release
    cx.simulate_modifiers_change(Modifiers::none());
    cx.read(|cx| {
        let active_editor = workspace.read(cx).active_item_as::<Editor>(cx).unwrap();
        assert_eq!(active_editor.read(cx).title(cx), "3.txt");
    });
    assert_tab_switcher_is_closed(workspace, cx);
}

#[gpui::test]
async fn test_quick_switch_before_popover_visible(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);

    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "First file",
                "2.txt": "Second file",
                "3.txt": "Third file",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    open_buffer("1.txt", &workspace, cx).await;
    open_buffer("2.txt", &workspace, cx).await;
    let _tab_3 = open_buffer("3.txt", &workspace, cx).await;

    // Simulate quick Ctrl+Tab: press modifier, open switcher, release modifier
    // all before the POPOVER_DELAY (300ms) elapses.
    cx.simulate_modifiers_change(Modifiers::control());
    let tab_switcher = open_tab_switcher(false, &workspace, cx);

    // Verify the switcher is not visible yet (before delay)
    tab_switcher.read_with(cx, |picker, cx| {
        let tab_switcher = picker
            .delegate
            .tab_switcher
            .upgrade()
            .expect("tab switcher should exist");
        assert!(!tab_switcher.read(cx).visible);
    });

    // Release modifiers before delay — should confirm the pre-selected item (2.txt)
    cx.simulate_modifiers_change(Modifiers::none());

    cx.read(|cx| {
        let active_editor = workspace.read(cx).active_item_as::<Editor>(cx).unwrap();
        assert_eq!(
            active_editor.read(cx).title(cx),
            "2.txt",
            "quick switch should select previous tab, not a random one"
        );
    });
    assert_tab_switcher_is_closed(workspace, cx);
}

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        super::init(cx);
        editor::init(cx);
        state
    })
}

#[track_caller]
fn open_tab_switcher(
    select_last: bool,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<TabSwitcherDelegate>> {
    cx.dispatch_action(Toggle { select_last });
    get_active_tab_switcher(workspace, cx)
}

#[track_caller]
fn get_active_tab_switcher(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<TabSwitcherDelegate>> {
    cx.run_until_parked();
    workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<TabSwitcher>(cx)
            .expect("tab switcher is not open")
            .read(cx)
            .picker
            .clone()
    })
}

fn open_tab_switcher_with_preview(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Picker<TabSwitcherDelegate>>, Entity<Editor>) {
    let editors = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update({
        let editors = editors.clone();
        move |_, cx| {
            cx.observe_new::<Editor>(move |_, _, cx| {
                editors.borrow_mut().push(cx.entity());
            })
        }
    });
    let picker = open_tab_switcher_for_active_pane(workspace, cx);
    let preview = cx
        .read(|cx| {
            editors
                .borrow()
                .iter()
                .find(|editor| editor.read(cx).read_only(cx))
                .cloned()
        })
        .expect("the tab switcher should create a read-only preview editor");
    (picker, preview)
}

#[gpui::test]
async fn test_open_in_active_pane_previews_live_buffers_without_switching_tabs(
    cx: &mut TestAppContext,
) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "first.txt": "Saved contents",
                "second.txt": "Second file",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
    let first = open_buffer("first.txt", &workspace, cx).await;
    let first_editor = cx.read(|cx| first.act_as::<Editor>(cx).unwrap());
    let contents = (0..500)
        .map(|row| {
            if row == 350 {
                "Unsaved cursor line\n".to_string()
            } else {
                format!("Line {row}\n")
            }
        })
        .collect::<String>();
    let cursor = MultiBufferOffset(contents.find("Unsaved cursor line").unwrap());
    first_editor.update_in(cx, |editor, window, cx| {
        editor.set_text(contents, window, cx);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([cursor..cursor]);
        });
    });
    let second = open_buffer("second.txt", &workspace, cx).await;
    let (picker, preview) = open_tab_switcher_with_preview(&workspace, cx);
    assert!(
        preview
            .read_with(cx, |editor, cx| editor.text(cx))
            .contains("Unsaved cursor line")
    );
    preview.update_in(cx, |editor, window, cx| {
        assert!(
            editor.snapshot(window, cx).scroll_position().y > 0.0,
            "the cursor should be centered in the preview"
        );
    });
    let original_selection =
        first_editor.read_with(cx, |editor, _| *editor.selections.newest_anchor());
    let assert_workspace_unchanged = |cx: &mut VisualTestContext| {
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_item(cx).unwrap().item_id(),
                second.item_id()
            );
            assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
        });
        first_editor.read_with(cx, |editor, _| {
            assert_eq!(*editor.selections.newest_anchor(), original_selection)
        });
    };
    assert_workspace_unchanged(cx);
    cx.dispatch_action(menu::SelectNext);
    cx.run_until_parked();
    assert_eq!(
        preview.read_with(cx, |editor, cx| editor.text(cx)),
        "Second file"
    );
    assert_workspace_unchanged(cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("first", window, cx)
    });
    cx.run_until_parked();
    assert!(
        preview
            .read_with(cx, |editor, cx| editor.text(cx))
            .contains("Unsaved cursor line")
    );
    assert_workspace_unchanged(cx);
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert_workspace_unchanged(cx);
    assert_tab_switcher_is_closed(workspace.clone(), cx);
}

#[gpui::test]
async fn test_open_in_active_pane_preview_refreshes_after_closing_and_filtering(
    cx: &mut TestAppContext,
) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "first.txt": "First file",
                "second.txt": "Second file",
                "third.txt": "Third file",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
    open_buffer("first.txt", &workspace, cx).await;
    open_buffer("second.txt", &workspace, cx).await;
    open_buffer("third.txt", &workspace, cx).await;
    let (picker, preview) = open_tab_switcher_with_preview(&workspace, cx);
    assert_eq!(
        preview.read_with(cx, |editor, cx| editor.text(cx)),
        "Second file"
    );
    cx.dispatch_action(CloseSelectedItem);
    cx.run_until_parked();
    assert_eq!(
        preview.read_with(cx, |editor, cx| editor.text(cx)),
        "First file"
    );
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("missing", window, cx)
    });
    cx.run_until_parked();
    picker.read_with(cx, |picker, cx| {
        assert_eq!(picker.delegate.match_count(), 0);
        assert!(picker.delegate.try_get_preview_data_for_match(cx).is_none());
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("third", window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        preview.read_with(cx, |editor, cx| editor.text(cx)),
        "Third file"
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_tab_switcher_is_closed(workspace.clone(), cx);
    assert_eq!(
        workspace.read_with(cx, |workspace, cx| workspace
            .active_item_as::<Editor>(cx)
            .unwrap()
            .read(cx)
            .title(cx)
            .into_owned()),
        "third.txt"
    );
}

#[gpui::test]
async fn test_open_in_active_pane_previews_untitled_buffers_and_cursor_boundaries(
    cx: &mut TestAppContext,
) {
    let app_state = init_test(cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
    let editor = workspace
        .update_in(cx, Editor::new_in_workspace)
        .await
        .unwrap();
    let (picker, preview) = open_tab_switcher_with_preview(&workspace, cx);
    assert_eq!(preview.read_with(cx, |editor, cx| editor.text(cx)), "");
    for contents in ["Untitled contents", "First line\n\n", ""] {
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(contents, window, cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                let cursor = MultiBufferOffset(contents.len());
                selections.select_ranges([cursor..cursor]);
            });
        });
        picker.update_in(cx, |picker, window, cx| picker.refresh(window, cx));
        cx.run_until_parked();
        assert_eq!(
            preview.read_with(cx, |editor, cx| editor.text(cx)),
            contents
        );
        picker.read_with(cx, |picker, cx| {
            let update = picker.delegate.try_get_preview_data_for_match(cx).unwrap();
            assert!(matches!(update.source, picker::PreviewSource::Buffer(_)));
            assert_eq!(
                update.match_location.unwrap().range,
                contents.len()..contents.len()
            );
        });
    }
    let item = cx.new(|cx| {
        let mut item = TestItem::new(cx).with_label("terminal");
        item.tab_descriptions = Some(vec!["terminal"]);
        item
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(item), None, false, window, cx)
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("terminal", window, cx)
    });
    cx.run_until_parked();
    picker.read_with(cx, |picker, cx| {
        let update = picker.delegate.try_get_preview_data_for_match(cx).unwrap();
        let picker::PreviewSource::Message(message) = update.source else {
            panic!("non-editor tabs should show a placeholder")
        };
        assert_eq!(message.text.as_ref(), "No preview available for this tab");
    });
}

#[gpui::test]
async fn test_open_in_active_pane_previews_multibuffer_at_cursor(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "first.txt": "First file",
                "second.txt": "Second file",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
    let first = open_buffer("first.txt", &workspace, cx).await;
    let second = open_buffer("second.txt", &workspace, cx).await;
    let buffers = cx.read(|cx| {
        [&first, &second].map(|item| {
            item.act_as::<Editor>(cx)
                .unwrap()
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
        })
    });
    let multi_buffer = cx.new(|cx| {
        let capability = buffers[0].read(cx).capability();
        let mut multi_buffer =
            editor::MultiBuffer::without_headers(capability).with_title("combined".to_string());
        for buffer in &buffers {
            let end = buffer.read(cx).max_point();
            multi_buffer.set_excerpts_for_buffer(buffer.clone(), [Default::default()..end], 0, cx);
        }
        multi_buffer
    });
    let editor = cx.new_window_entity(|window, cx| {
        let mut editor = Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
        let cursor = editor.buffer().read(cx).snapshot(cx).len();
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([cursor..cursor])
        });
        editor
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
    });
    let (picker, preview) = open_tab_switcher_with_preview(&workspace, cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("combined", window, cx)
    });
    cx.run_until_parked();
    preview.read_with(cx, |editor, cx| {
        assert_eq!(editor.text(cx), "Second file");
        assert_eq!(editor.buffer().read(cx).all_buffers().len(), 1);
        assert!(editor.buffer().read(cx).all_buffers().contains(&buffers[1]));
    });
}

async fn open_buffer(
    file_path: &str,
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Box<dyn ItemHandle> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let worktree_id = project.update(cx, |project, cx| {
        let worktree = project.worktrees(cx).last().expect("worktree not found");
        worktree.read(cx).id()
    });
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path(file_path).into(),
    };
    workspace
        .update_in(cx, move |workspace, window, cx| {
            workspace.open_path(project_path, None, true, window, cx)
        })
        .await
        .unwrap()
}

#[track_caller]
fn assert_match_selection(
    tab_switcher: &Picker<TabSwitcherDelegate>,
    expected_selection_index: usize,
    expected_item: Box<dyn ItemHandle>,
) {
    assert_eq!(
        tab_switcher.delegate.selected_index(),
        expected_selection_index,
        "item is not selected"
    );
    assert_match_at_position(tab_switcher, expected_selection_index, expected_item);
}

#[track_caller]
fn assert_match_at_position(
    tab_switcher: &Picker<TabSwitcherDelegate>,
    match_index: usize,
    expected_item: Box<dyn ItemHandle>,
) {
    let match_item = tab_switcher
        .delegate
        .matches
        .get(match_index)
        .unwrap_or_else(|| panic!("Tab Switcher has no match for index {match_index}"));
    assert_eq!(match_item.item.item_id(), expected_item.item_id());
}

#[track_caller]
fn assert_tab_switcher_is_closed(workspace: Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update(cx, |workspace, cx| {
        assert!(
            workspace.active_modal::<TabSwitcher>(cx).is_none(),
            "tab switcher is still open"
        );
    });
}

#[track_caller]
fn open_tab_switcher_for_active_pane(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<TabSwitcherDelegate>> {
    cx.dispatch_action(OpenInActivePane);
    get_active_tab_switcher(workspace, cx)
}

#[gpui::test]
async fn test_open_in_active_pane_deduplicates_files_by_path(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "1.txt": "",
                "2.txt": "",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    open_buffer("1.txt", &workspace, cx).await;
    open_buffer("2.txt", &workspace, cx).await;

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(
            workspace.active_pane().clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        );
    });
    open_buffer("1.txt", &workspace, cx).await;

    let tab_switcher = open_tab_switcher_for_active_pane(&workspace, cx);

    tab_switcher.read_with(cx, |picker, _cx| {
        assert_eq!(
            picker.delegate.matches.len(),
            2,
            "should show 2 unique files despite 3 tabs"
        );
    });
}

#[gpui::test]
async fn test_open_in_active_pane_clones_files_to_current_pane(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({"1.txt": ""}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    open_buffer("1.txt", &workspace, cx).await;

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(
            workspace.active_pane().clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        );
    });

    let panes = workspace.read_with(cx, |workspace, _| workspace.panes().to_vec());

    let tab_switcher = open_tab_switcher_for_active_pane(&workspace, cx);
    tab_switcher.update(cx, |picker, _| {
        picker.delegate.selected_index = 0;
    });

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    let editor_1 = panes[0].read_with(cx, |pane, cx| {
        pane.active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("pane 1 should have editor")
    });

    let editor_2 = panes[1].read_with(cx, |pane, cx| {
        pane.active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("pane 2 should have editor")
    });

    assert_ne!(
        editor_1.entity_id(),
        editor_2.entity_id(),
        "should clone to new instance"
    );
}

#[gpui::test]
async fn test_open_in_active_pane_moves_terminals_to_current_pane(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let test_item = cx.new(|cx| TestItem::new(cx).with_label("terminal"));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(test_item.clone()), None, true, window, cx);
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(
            workspace.active_pane().clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        );
    });

    let panes = workspace.read_with(cx, |workspace, _| workspace.panes().to_vec());

    let tab_switcher = open_tab_switcher_for_active_pane(&workspace, cx);
    tab_switcher.update(cx, |picker, _| {
        picker.delegate.selected_index = 0;
    });

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert!(
        !panes[0].read_with(cx, |pane, _| {
            pane.items()
                .any(|item| item.item_id() == test_item.item_id())
        }),
        "should be removed from pane 1"
    );
    assert!(
        panes[1].read_with(cx, |pane, _| {
            pane.items()
                .any(|item| item.item_id() == test_item.item_id())
        }),
        "should be moved to pane 2"
    );
}

#[gpui::test]
async fn test_open_in_active_pane_closes_file_in_all_panes(cx: &mut gpui::TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({"1.txt": ""}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    open_buffer("1.txt", &workspace, cx).await;

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(
            workspace.active_pane().clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        );
    });
    open_buffer("1.txt", &workspace, cx).await;

    let panes = workspace.read_with(cx, |workspace, _| workspace.panes().to_vec());

    let tab_switcher = open_tab_switcher_for_active_pane(&workspace, cx);
    tab_switcher.update(cx, |picker, _| {
        picker.delegate.selected_index = 0;
    });

    cx.dispatch_action(CloseSelectedItem);
    cx.run_until_parked();

    for pane in &panes {
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.items_len()),
            0,
            "all panes should be empty"
        );
    }
}

#[gpui::test]
async fn test_toggle_all_stays_open_after_closing_last_tab_in_active_pane(
    cx: &mut gpui::TestAppContext,
) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a.txt": "",
                "b.txt": "",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let tab_a = open_buffer("a.txt", &workspace, cx).await;
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(
            workspace.active_pane().clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        );
    });
    open_buffer("b.txt", &workspace, cx).await;

    // Right pane (with b.txt) is now the active pane.
    cx.dispatch_action(ToggleAll);
    let tab_switcher = get_active_tab_switcher(&workspace, cx);

    tab_switcher.update(cx, |picker, _| {
        assert_eq!(picker.delegate.matches.len(), 2);
        // Explicitly select b.txt (index 0, the most recently activated item)
        // to close the last tab in the active (right) pane.
        picker.delegate.selected_index = 0;
    });

    cx.dispatch_action(CloseSelectedItem);
    cx.run_until_parked();

    // Tab switcher must remain open with a.txt as the only match
    let tab_switcher = get_active_tab_switcher(&workspace, cx);
    tab_switcher.update(cx, |picker, cx| {
        assert_eq!(picker.delegate.matches.len(), 1);
        assert_match_at_position(picker, 0, tab_a.boxed_clone());
        let _ = cx;
    });
}
