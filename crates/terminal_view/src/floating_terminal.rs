use gpui::{Action, Axis, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, actions};
use ui::prelude::*;
use util::ResultExt;
use workspace::{FloatingPaneLayer, Item, Pane, SplitDirection, Workspace};

use crate::{TerminalView, default_working_directory, terminal_panel::build_terminal_pane};

const KEYBOARD_GEOMETRY_STEP: f32 = 40.;

actions!(
    floating_terminal,
    [
        /// Shows or hides all floating terminals, creating one if necessary.
        Toggle,
        /// Opens a new floating terminal.
        New,
        /// Focuses the next floating terminal.
        Next,
        /// Focuses the previous floating terminal.
        Previous,
        /// Focuses the floating terminal to the left of the active one.
        FocusLeft,
        /// Focuses the floating terminal below the active one.
        FocusDown,
        /// Focuses the floating terminal above the active one.
        FocusUp,
        /// Focuses the floating terminal to the right of the active one.
        FocusRight,
        /// Closes the active floating terminal.
        Close,
        /// Increases the active floating terminal's width by 40 pixels.
        IncreaseWidth,
        /// Decreases the active floating terminal's width by 40 pixels.
        DecreaseWidth,
        /// Increases the active floating terminal's height by 40 pixels.
        IncreaseHeight,
        /// Decreases the active floating terminal's height by 40 pixels.
        DecreaseHeight,
        /// Moves the active floating terminal left by 40 pixels.
        MoveLeft,
        /// Moves the active floating terminal down by 40 pixels.
        MoveDown,
        /// Moves the active floating terminal up by 40 pixels.
        MoveUp,
        /// Moves the active floating terminal right by 40 pixels.
        MoveRight,
        /// Restores default sizes and positions for the current floating-terminal layout.
        ResetPositions,
        /// Switches between stacked and tiled floating-terminal layouts, retaining each layout's edits.
        ToggleLayout,
        /// Toggles the focused floating terminal between 80% size and its previous geometry.
        ToggleMaximize,
    ]
);

pub(super) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &Toggle, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if workspace.has_active_modal(window, cx) {
                    return;
                }
                let layer = workspace.floating_panes().clone();
                if !layer.read(cx).has_panes() {
                    new_terminal(workspace, window, cx);
                } else {
                    layer.update(cx, |layer, cx| {
                        if layer.is_visible() {
                            layer.hide(window, cx);
                        } else {
                            layer.show(window, cx);
                        }
                    });
                }
            });
        });
        workspace.register_action(|_, _: &New, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    new_terminal(workspace, window, cx);
                }
            });
        });
        workspace.register_action(|_, _: &Next, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.activate_relative(true, window, cx));
                }
            });
        });
        workspace.register_action(|_, _: &Previous, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.activate_relative(false, window, cx));
                }
            });
        });
        workspace.register_action(|_, _: &FocusLeft, window, cx| {
            focus_terminal_in_direction(SplitDirection::Left, window, cx);
        });
        workspace.register_action(|_, _: &FocusDown, window, cx| {
            focus_terminal_in_direction(SplitDirection::Down, window, cx);
        });
        workspace.register_action(|_, _: &FocusUp, window, cx| {
            focus_terminal_in_direction(SplitDirection::Up, window, cx);
        });
        workspace.register_action(|_, _: &FocusRight, window, cx| {
            focus_terminal_in_direction(SplitDirection::Right, window, cx);
        });
        workspace.register_action(|_, _: &IncreaseWidth, window, cx| {
            resize_terminal(Axis::Horizontal, px(KEYBOARD_GEOMETRY_STEP), window, cx);
        });
        workspace.register_action(|_, _: &DecreaseWidth, window, cx| {
            resize_terminal(Axis::Horizontal, px(-KEYBOARD_GEOMETRY_STEP), window, cx);
        });
        workspace.register_action(|_, _: &IncreaseHeight, window, cx| {
            resize_terminal(Axis::Vertical, px(KEYBOARD_GEOMETRY_STEP), window, cx);
        });
        workspace.register_action(|_, _: &DecreaseHeight, window, cx| {
            resize_terminal(Axis::Vertical, px(-KEYBOARD_GEOMETRY_STEP), window, cx);
        });
        workspace.register_action(|_, _: &MoveLeft, window, cx| {
            move_terminal(gpui::point(px(-KEYBOARD_GEOMETRY_STEP), px(0.)), window, cx);
        });
        workspace.register_action(|_, _: &MoveDown, window, cx| {
            move_terminal(gpui::point(px(0.), px(KEYBOARD_GEOMETRY_STEP)), window, cx);
        });
        workspace.register_action(|_, _: &MoveUp, window, cx| {
            move_terminal(gpui::point(px(0.), px(-KEYBOARD_GEOMETRY_STEP)), window, cx);
        });
        workspace.register_action(|_, _: &MoveRight, window, cx| {
            move_terminal(gpui::point(px(KEYBOARD_GEOMETRY_STEP), px(0.)), window, cx);
        });
        workspace.register_action(|_, _: &ResetPositions, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.reset_positions(cx));
                }
            });
        });
        workspace.register_action(|_, _: &ToggleLayout, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    cx.stop_active_drag(window);
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.toggle_layout(cx));
                }
            });
        });
        workspace.register_action(|_, _: &ToggleMaximize, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.toggle_maximize(window, cx));
                }
            });
        });
        workspace.register_action(|_, _: &Close, window, cx| {
            cx.defer_in(window, |workspace, window, cx| {
                if !workspace.has_active_modal(window, cx) {
                    workspace
                        .floating_panes()
                        .clone()
                        .update(cx, |layer, cx| layer.close_active(window, cx));
                }
            });
        });
    })
    .detach();
}

fn focus_terminal_in_direction(
    direction: SplitDirection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.defer_in(window, move |workspace, window, cx| {
        if !workspace.has_active_modal(window, cx) {
            workspace.floating_panes().clone().update(cx, |layer, cx| {
                layer.activate_in_direction(direction, window, cx)
            });
        }
    });
}

fn resize_terminal(axis: Axis, amount: Pixels, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, move |workspace, window, cx| {
        if !workspace.has_active_modal(window, cx) {
            workspace.floating_panes().clone().update(cx, |layer, cx| {
                layer.resize_active(axis, amount, cx);
            });
        }
    });
}

fn move_terminal(delta: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, move |workspace, window, cx| {
        if !workspace.has_active_modal(window, cx) {
            workspace
                .floating_panes()
                .clone()
                .update(cx, |layer, cx| layer.move_active(delta, cx));
        }
    });
}

fn new_terminal(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    let working_directory = default_working_directory(workspace, cx);
    let layer = workspace.floating_panes().clone();
    let Some((pane, placeholder)) = add_starting_pane(workspace, window, cx) else {
        workspace.show_toast(
            workspace::Toast::new(
                workspace::notifications::NotificationId::unique::<New>(),
                "At most four floating terminals can be open in a workspace.",
            )
            .autohide(),
            cx,
        );
        return;
    };
    if !project.read(cx).supports_terminal(cx) {
        placeholder.update(cx, |placeholder, cx| {
            placeholder.error = Some("Terminals are not supported in this project".into());
            cx.emit(workspace::item::ItemEvent::UpdateTab);
            cx.notify();
        });
        return;
    }
    let workspace = workspace.weak_handle();
    let pane_id = pane.entity_id();
    let pane = pane.downgrade();
    let placeholder = placeholder.downgrade();
    layer.update(cx, |layer, cx| {
        let creation = cx.spawn_in(window, async move |layer, cx| {
            let result = project
                .update(cx, |project, cx| {
                    project.create_terminal_shell(working_directory, cx)
                })
                .await;
            finish_creation(
                layer,
                pane,
                placeholder,
                workspace,
                project.downgrade(),
                result,
                cx,
            )
            .log_err();
        });
        layer.set_creation_task(pane_id, creation);
    });
}

fn add_starting_pane(
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Entity<Pane>, Entity<StartingTerminal>)> {
    let layer = workspace.floating_panes().clone();
    if !layer.read(cx).can_add_pane() {
        return None;
    }
    let project = workspace.project().clone();
    let pane = build_terminal_pane(workspace.weak_handle(), project, false, window, cx);
    pane.update(cx, |pane, cx| {
        pane.set_can_toggle_zoom(false, cx);
        pane.set_should_display_welcome_page(false);
    });
    let placeholder = cx.new(|cx| StartingTerminal {
        error: None,
        focus_handle: cx.focus_handle(),
    });
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(placeholder.clone()), true, false, None, window, cx)
    });
    let added = layer.update(cx, |layer, cx| {
        layer.add_pane(pane.clone(), New.boxed_clone(), window, cx)
    });
    added.then_some((pane, placeholder))
}

fn finish_creation(
    layer: WeakEntity<FloatingPaneLayer>,
    pane: WeakEntity<Pane>,
    placeholder: WeakEntity<StartingTerminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<project::Project>,
    result: anyhow::Result<Entity<terminal::Terminal>>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    match result {
        Ok(terminal) => {
            let modal_open = workspace.update_in(cx, |workspace, window, cx| {
                workspace.has_active_modal(window, cx)
            })?;
            layer.update_in(cx, |layer, window, cx| {
                let Some(pane) = pane
                    .upgrade()
                    .filter(|pane| layer.pane(pane.entity_id()).is_some())
                else {
                    return;
                };
                let focus = layer.is_visible()
                    && !modal_open
                    && pane.focus_handle(cx).contains_focused(window, cx);
                let terminal_view =
                    cx.new(|cx| TerminalView::new(terminal, workspace, None, project, window, cx));
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(terminal_view), true, focus, None, window, cx);
                    pane.remove_item(placeholder.entity_id(), false, false, window, cx);
                });
            })?;
        }
        Err(error) => {
            log::error!("Failed to create floating terminal: {error:#}");
            placeholder.update(cx, |placeholder, cx| {
                placeholder.error = Some(error.to_string().into());
                cx.emit(workspace::item::ItemEvent::UpdateTab);
                cx.notify();
            })?;
        }
    }
    Ok(())
}

struct StartingTerminal {
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl Focusable for StartingTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<workspace::item::ItemEvent> for StartingTerminal {}

impl Item for StartingTerminal {
    type Event = workspace::item::ItemEvent;
    fn to_item_events(event: &Self::Event, callback: &mut dyn FnMut(Self::Event)) {
        callback(*event);
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        if self.error.is_some() {
            "Failed to start terminal".into()
        } else {
            "Starting terminal…".into()
        }
    }
}

impl Render for StartingTerminal {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .justify_center()
            .items_center()
            .track_focus(&self.focus_handle)
            .child(Label::new(if self.error.is_some() {
                "Failed to start terminal"
            } else {
                "Starting terminal…"
            }))
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).color(Color::Muted)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{DismissEvent, Modifiers, TestAppContext, UpdateGlobal, VisualTestContext};
    use terminal::{
        Terminal, TerminalBuilder,
        terminal_settings::{AlternateScroll, CursorShape},
    };
    use util::paths::PathStyle;
    use workspace::item::ItemEvent;

    fn display_terminal(cx: &mut App) -> Entity<Terminal> {
        cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        })
    }

    async fn complete(
        workspace: &Entity<Workspace>,
        pane: &Entity<Pane>,
        placeholder: &Entity<StartingTerminal>,
        terminal: &Entity<Terminal>,
        cx: &mut VisualTestContext,
    ) {
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().downgrade());
        let project = workspace.read_with(cx, |workspace, _| workspace.project().downgrade());
        let workspace = workspace.downgrade();
        let pane = pane.downgrade();
        let placeholder = placeholder.downgrade();
        let terminal = terminal.clone();
        cx.update(|window, cx| {
            window.spawn(cx, async move |cx| {
                finish_creation(
                    layer,
                    pane,
                    placeholder,
                    workspace,
                    project,
                    Ok(terminal),
                    cx,
                )
            })
        })
        .await
        .expect("complete terminal creation");
        cx.run_until_parked();
    }

    fn starting_pane(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> (Entity<Pane>, Entity<StartingTerminal>) {
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                add_starting_pane(workspace, window, cx).expect("floating window capacity")
            })
        })
    }

    #[test]
    fn floating_terminal_keymap_overrides_terminal_and_search_contexts() {
        let floating_context = "FloatingTerminal || (FloatingTerminal > (Terminal || Editor))";
        let keymap = gpui::Keymap::new(vec![
            gpui::KeyBinding::new(
                "ctrl-tab",
                workspace::ActivateNextPane,
                Some("Terminal && !AgentPanel"),
            ),
            gpui::KeyBinding::new(
                "ctrl-shift-tab",
                workspace::ActivatePreviousPane,
                Some("Terminal && !AgentPanel"),
            ),
            gpui::KeyBinding::new("ctrl-tab", workspace::ActivateNextPane, Some("Editor")),
            gpui::KeyBinding::new(
                "ctrl-shift-tab",
                workspace::ActivatePreviousPane,
                Some("Editor"),
            ),
            gpui::KeyBinding::new(
                "super-right",
                workspace::ActivateNextPane,
                Some("Terminal && !AgentPanel"),
            ),
            gpui::KeyBinding::new("super-right", workspace::ActivateNextPane, Some("Editor")),
            gpui::KeyBinding::new("ctrl-tab", Next, Some(floating_context)),
            gpui::KeyBinding::new("ctrl-shift-tab", Previous, Some(floating_context)),
            gpui::KeyBinding::new("super-h", FocusLeft, Some(floating_context)),
            gpui::KeyBinding::new("super-j", FocusDown, Some(floating_context)),
            gpui::KeyBinding::new("super-k", FocusUp, Some(floating_context)),
            gpui::KeyBinding::new("super-l", FocusRight, Some(floating_context)),
            gpui::KeyBinding::new("super-right", IncreaseWidth, Some(floating_context)),
            gpui::KeyBinding::new("super-left", DecreaseWidth, Some(floating_context)),
            gpui::KeyBinding::new("super-up", IncreaseHeight, Some(floating_context)),
            gpui::KeyBinding::new("super-down", DecreaseHeight, Some(floating_context)),
            gpui::KeyBinding::new("super-shift-h", MoveLeft, Some(floating_context)),
            gpui::KeyBinding::new("super-shift-j", MoveDown, Some(floating_context)),
            gpui::KeyBinding::new("super-shift-k", MoveUp, Some(floating_context)),
            gpui::KeyBinding::new("super-shift-l", MoveRight, Some(floating_context)),
            gpui::KeyBinding::new("super-[", ToggleLayout, Some(floating_context)),
            gpui::KeyBinding::new("super-]", ToggleMaximize, Some(floating_context)),
        ]);
        for contexts in [
            vec!["Workspace", "FloatingTerminal", "Pane", "Terminal"],
            vec!["Workspace", "FloatingTerminal", "Pane", "Editor"],
            vec!["Workspace", "FloatingTerminal", "Pane"],
        ] {
            let contexts = contexts
                .into_iter()
                .map(|context| gpui::KeyContext::parse(context).expect("valid key context"))
                .collect::<Vec<_>>();
            for (keystroke, action) in [
                ("ctrl-tab", Next.boxed_clone()),
                ("ctrl-shift-tab", Previous.boxed_clone()),
                ("super-h", FocusLeft.boxed_clone()),
                ("super-j", FocusDown.boxed_clone()),
                ("super-k", FocusUp.boxed_clone()),
                ("super-l", FocusRight.boxed_clone()),
                ("super-right", IncreaseWidth.boxed_clone()),
                ("super-left", DecreaseWidth.boxed_clone()),
                ("super-up", IncreaseHeight.boxed_clone()),
                ("super-down", DecreaseHeight.boxed_clone()),
                ("super-shift-h", MoveLeft.boxed_clone()),
                ("super-shift-j", MoveDown.boxed_clone()),
                ("super-shift-k", MoveUp.boxed_clone()),
                ("super-shift-l", MoveRight.boxed_clone()),
                ("super-[", ToggleLayout.boxed_clone()),
                ("super-]", ToggleMaximize.boxed_clone()),
            ] {
                let (bindings, pending) = keymap.bindings_for_input(
                    &[gpui::Keystroke::parse(keystroke).expect("valid keystroke")],
                    &contexts,
                );
                assert!(!pending);
                assert!(
                    bindings
                        .first()
                        .is_some_and(|binding| binding.action().partial_eq(action.as_ref()))
                );
            }
        }
        let contexts = [
            gpui::KeyContext::parse("Workspace").expect("workspace context"),
            gpui::KeyContext::parse("Terminal").expect("terminal context"),
        ];
        let (bindings, _) = keymap.bindings_for_input(
            &[gpui::Keystroke::parse("ctrl-tab").expect("valid keystroke")],
            &contexts,
        );
        assert!(
            bindings
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&workspace::ActivateNextPane))
        );
        let (bindings, _) = keymap.bindings_for_input(
            &[gpui::Keystroke::parse("super-right").expect("valid keystroke")],
            &contexts,
        );
        assert!(
            bindings
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&workspace::ActivateNextPane))
        );
    }

    struct TestSearchField {
        editor: Entity<editor::Editor>,
    }

    #[gpui::test]
    async fn floating_terminal_layout_shortcut_retains_geometry_sessions_and_focus(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        cx.update(|cx| {
            let context = "FloatingTerminal || (FloatingTerminal > (Terminal || Editor))";
            cx.bind_keys([
                gpui::KeyBinding::new("super-[", ToggleLayout, Some(context)),
                gpui::KeyBinding::new("super-]", ToggleMaximize, Some(context)),
                gpui::KeyBinding::new("super-shift-l", MoveRight, Some(context)),
                gpui::KeyBinding::new("super-shift-h", MoveLeft, Some(context)),
                gpui::KeyBinding::new("super-k", IncreaseHeight, Some(context)),
                gpui::KeyBinding::new("super-h", DecreaseWidth, Some(context)),
            ]);
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let (first, pending_first) = starting_pane(&workspace, &mut cx);
        let first_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &first, &pending_first, &first_terminal, &mut cx).await;
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        let second_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(
            &workspace,
            &second,
            &pending_second,
            &second_terminal,
            &mut cx,
        )
        .await;
        cx.simulate_keystrokes("super-shift-l super-k");
        let stacked = terminal_bounds(&second_terminal, &cx);
        cx.simulate_keystrokes("super-[");
        let tiled_default = terminal_bounds(&second_terminal, &cx);
        assert!(tiled_default.bounds.size.width < stacked.bounds.size.width);
        assert!(tiled_default.bounds.size.height > stacked.bounds.size.height);
        let first_default = terminal_bounds(&first_terminal, &cx);
        assert_eq!(first_default.bounds.size, tiled_default.bounds.size);
        assert!(first_default.bounds.right() <= tiled_default.bounds.left());
        cx.simulate_keystrokes("super-shift-h super-h");
        let tiled = terminal_bounds(&second_terminal, &cx);
        for _ in 0..3 {
            cx.simulate_keystrokes("super-[");
            assert_eq!(terminal_bounds(&second_terminal, &cx), stacked);
            cx.simulate_keystrokes("super-[");
            assert_eq!(terminal_bounds(&second_terminal, &cx), tiled);
            cx.update(|window, cx| assert!(second.read(cx).has_focus(window, cx)));
        }
        cx.simulate_keystrokes("super-]");
        let enlarged = terminal_bounds(&second_terminal, &cx);
        cx.simulate_keystrokes("super-[");
        assert_eq!(terminal_bounds(&second_terminal, &cx), stacked);
        cx.simulate_keystrokes("super-[");
        assert_eq!(terminal_bounds(&second_terminal, &cx), enlarged);
        cx.simulate_keystrokes("super-]");
        assert_eq!(terminal_bounds(&second_terminal, &cx), tiled);
        cx.dispatch_action(ResetPositions);
        assert_eq!(terminal_bounds(&second_terminal, &cx), tiled_default);
        cx.dispatch_action(Toggle);
        cx.dispatch_action(ToggleLayout);
        cx.dispatch_action(Toggle);
        assert_eq!(terminal_bounds(&second_terminal, &cx), stacked);
        let modal = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| TestModal {
                    focus_handle: cx.focus_handle(),
                });
                workspace.active_modal::<TestModal>(cx).unwrap()
            })
        });
        cx.dispatch_action(ToggleLayout);
        assert_eq!(terminal_bounds(&second_terminal, &cx), stacked);
        cx.update(|window, cx| assert!(modal.focus_handle(cx).is_focused(window)));
    }

    #[gpui::test]
    async fn floating_terminal_movement_reset_and_maximize_shortcuts(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        cx.update(|cx| {
            let context = "FloatingTerminal || (FloatingTerminal > (Terminal || Editor))";
            cx.bind_keys([
                gpui::KeyBinding::new("super-shift-h", MoveLeft, Some(context)),
                gpui::KeyBinding::new("super-shift-j", MoveDown, Some(context)),
                gpui::KeyBinding::new("super-shift-k", MoveUp, Some(context)),
                gpui::KeyBinding::new("super-shift-l", MoveRight, Some(context)),
                gpui::KeyBinding::new("super-[", ToggleLayout, Some(context)),
                gpui::KeyBinding::new("super-]", ToggleMaximize, Some(context)),
                gpui::KeyBinding::new("super-h", DecreaseWidth, Some(context)),
            ]);
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let (first, pending_first) = starting_pane(&workspace, &mut cx);
        let first_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &first, &pending_first, &first_terminal, &mut cx).await;
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        let second_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(
            &workspace,
            &second,
            &pending_second,
            &second_terminal,
            &mut cx,
        )
        .await;
        let first_bounds = terminal_bounds(&first_terminal, &cx);
        let original = terminal_bounds(&second_terminal, &cx);
        for (keystroke, delta) in [
            ("super-shift-h", gpui::point(px(-40.), px(0.))),
            ("super-shift-j", gpui::point(px(0.), px(40.))),
            ("super-shift-k", gpui::point(px(0.), px(-40.))),
            ("super-shift-l", gpui::point(px(40.), px(0.))),
        ] {
            let before = terminal_bounds(&second_terminal, &cx);
            cx.simulate_keystrokes(keystroke);
            let after = terminal_bounds(&second_terminal, &cx);
            assert!((after.bounds.origin.x - before.bounds.origin.x - delta.x).abs() < px(0.01));
            assert!((after.bounds.origin.y - before.bounds.origin.y - delta.y).abs() < px(0.01));
            assert_eq!(after.bounds.size, before.bounds.size);
            assert_eq!(terminal_bounds(&first_terminal, &cx), first_bounds);
            cx.update(|window, cx| assert!(second.read(cx).has_focus(window, cx)));
        }
        cx.simulate_keystrokes("super-]");
        let enlarged = terminal_bounds(&second_terminal, &cx);
        assert!(enlarged.bounds.size.width > original.bounds.size.width);
        assert!(enlarged.bounds.size.height > original.bounds.size.height);
        cx.simulate_keystrokes("super-shift-h super-h");
        assert_eq!(terminal_bounds(&second_terminal, &cx), enlarged);
        cx.simulate_keystrokes("super-]");
        assert_eq!(terminal_bounds(&second_terminal, &cx), original);
        cx.simulate_keystrokes("super-shift-l super-shift-l super-shift-l");
        assert!(
            (terminal_bounds(&second_terminal, &cx).bounds.origin.x
                - original.bounds.origin.x
                - px(120.))
            .abs()
                < px(0.01)
        );
        cx.simulate_keystrokes("super-shift-h super-shift-h super-shift-h");
        assert_eq!(terminal_bounds(&second_terminal, &cx), original);
        cx.simulate_keystrokes("super-shift-l super-h super-]");
        cx.dispatch_action(ResetPositions);
        assert_eq!(terminal_bounds(&first_terminal, &cx), first_bounds);
        assert_eq!(terminal_bounds(&second_terminal, &cx), original);
        cx.update(|_, cx| {
            assert!(
                second_terminal
                    .update(cx, |terminal, _| terminal.take_input_log())
                    .is_empty()
            )
        });
        let modal = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| TestModal {
                    focus_handle: cx.focus_handle(),
                });
                workspace.active_modal::<TestModal>(cx).unwrap()
            })
        });
        for action in [
            MoveRight.boxed_clone(),
            ResetPositions.boxed_clone(),
            ToggleMaximize.boxed_clone(),
        ] {
            cx.update(|window, cx| window.dispatch_action(action, cx));
            cx.run_until_parked();
        }
        assert_eq!(terminal_bounds(&second_terminal, &cx), original);
        cx.update(|window, cx| assert!(modal.focus_handle(cx).is_focused(window)));
        assert_eq!(
            layer.read_with(&cx, |layer, _| layer.active_pane()),
            Some(second)
        );
    }

    #[gpui::test]
    async fn floating_terminal_limit_counts_pending_hidden_and_failed_windows(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        cx.executor().allow_parking();
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        let created = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            let created = created.clone();
            cx.observe_new(move |_: &mut StartingTerminal, _, _| {
                created.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell = Some(
                        settings::Shell::Program("__nonexistent_floating_limit_shell__".into()),
                    );
                });
            });
            cx.bind_keys([gpui::KeyBinding::new(
                "super-n",
                New,
                Some("FloatingTerminal"),
            )]);
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let panes = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let panes = (0..4)
                    .map(|_| {
                        add_starting_pane(workspace, window, cx).expect("four available slots")
                    })
                    .collect::<Vec<_>>();
                assert!(add_starting_pane(workspace, window, cx).is_none());
                panes
            })
        });
        assert_eq!(created.load(Ordering::SeqCst), 4);
        assert!(!layer.read_with(&cx, |layer, _| layer.can_add_pane()));
        cx.simulate_keystrokes("super-n super-n super-n");
        assert_eq!(created.load(Ordering::SeqCst), 4);
        let button = cx
            .debug_bounds("ICON-Plus")
            .expect("floating terminal new button");
        cx.simulate_click(button.center(), Modifiers::default());
        assert_eq!(created.load(Ordering::SeqCst), 4);
        cx.dispatch_action(Toggle);
        cx.dispatch_action(New);
        assert_eq!(created.load(Ordering::SeqCst), 4);
        assert!(!layer.read_with(&cx, |layer, _| layer.is_visible()));
        cx.dispatch_action(Toggle);
        cx.dispatch_action(Close);
        assert!(layer.read_with(&cx, |layer, _| layer.can_add_pane()));
        let button = cx
            .debug_bounds("ICON-Plus")
            .expect("floating terminal new button");
        cx.simulate_click(button.center(), Modifiers::default());
        cx.condition(&layer, |layer, cx| {
            layer.active_pane().is_some_and(|pane| {
                pane.read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<StartingTerminal>())
                    .is_some_and(|item| item.read(cx).error.is_some())
            })
        })
        .await;
        assert_eq!(created.load(Ordering::SeqCst), 5);
        assert!(!layer.read_with(&cx, |layer, _| layer.can_add_pane()));
        cx.dispatch_action(New);
        assert_eq!(created.load(Ordering::SeqCst), 5);
        assert_eq!(
            panes
                .iter()
                .take(3)
                .filter(|(pane, _)| layer
                    .read_with(&cx, |layer, _| layer.pane(pane.entity_id()).is_some()))
                .count(),
            3
        );
    }

    impl EventEmitter<workspace::ToolbarItemEvent> for TestSearchField {}

    impl workspace::ToolbarItemView for TestSearchField {
        fn set_active_pane_item(
            &mut self,
            _: Option<&dyn workspace::ItemHandle>,
            _: &mut Window,
            _: &mut Context<Self>,
        ) -> workspace::ToolbarItemLocation {
            workspace::ToolbarItemLocation::PrimaryLeft
        }
    }

    impl Render for TestSearchField {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(300.)).h(px(24.)).child(self.editor.clone())
        }
    }

    fn terminal_bounds(
        terminal: &Entity<Terminal>,
        cx: &VisualTestContext,
    ) -> terminal::TerminalBounds {
        terminal.read_with(cx, |terminal, _| terminal.last_content().terminal_bounds)
    }

    #[gpui::test]
    async fn floating_terminal_keyboard_resize_keeps_sessions_editor_layout_and_search_focus(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        cx.update(|cx| {
            editor::init(cx);
            let bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )
            .expect("default keymap");
            cx.bind_keys(bindings);
            let context = "FloatingTerminal || (FloatingTerminal > (Terminal || Editor))";
            cx.bind_keys([
                gpui::KeyBinding::new("super-right", IncreaseWidth, Some(context)),
                gpui::KeyBinding::new("super-left", DecreaseWidth, Some(context)),
                gpui::KeyBinding::new("super-up", IncreaseHeight, Some(context)),
                gpui::KeyBinding::new("super-down", DecreaseHeight, Some(context)),
            ]);
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(IncreaseWidth);
        cx.dispatch_action(DecreaseHeight);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        assert!(!layer.read_with(&cx, |layer, _| layer.has_panes()));
        let (first, pending_first) = starting_pane(&workspace, &mut cx);
        let first_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &first, &pending_first, &first_terminal, &mut cx).await;
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        let second_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(
            &workspace,
            &second,
            &pending_second,
            &second_terminal,
            &mut cx,
        )
        .await;
        let editor_bounds = workspace.read_with(&cx, |workspace, _| {
            workspace.bounding_box_for_pane(workspace.active_pane())
        });
        let first_bounds = terminal_bounds(&first_terminal, &cx);
        let second_view = second.read_with(&cx, |pane, _| {
            pane.active_item()
                .unwrap()
                .downcast::<TerminalView>()
                .unwrap()
        });
        for (keystroke, axis, amount) in [
            ("super-right", Axis::Horizontal, px(40.)),
            ("super-left", Axis::Horizontal, px(-40.)),
            ("super-up", Axis::Vertical, px(40.)),
            ("super-down", Axis::Vertical, px(-40.)),
        ] {
            let before = terminal_bounds(&second_terminal, &cx);
            cx.simulate_keystrokes(keystroke);
            let after = terminal_bounds(&second_terminal, &cx);
            match axis {
                Axis::Horizontal => assert!(
                    (after.bounds.size.width - before.bounds.size.width - amount).abs() < px(0.01)
                ),
                Axis::Vertical => {
                    let change = after.bounds.size.height - before.bounds.size.height;
                    assert!((change - amount).abs() <= before.line_height);
                    assert_eq!(change.signum(), amount.signum());
                }
            }
            cx.update(|window, cx| assert!(second_view.focus_handle(cx).is_focused(window)));
            assert_eq!(
                layer.read_with(&cx, |layer, _| layer.active_pane()),
                Some(second.clone())
            );
            assert_eq!(terminal_bounds(&first_terminal, &cx), first_bounds);
            assert_eq!(
                workspace.read_with(&cx, |workspace, _| workspace
                    .bounding_box_for_pane(workspace.active_pane())),
                editor_bounds
            );
        }
        let before = terminal_bounds(&second_terminal, &cx);
        cx.simulate_keystrokes("super-right super-right super-right");
        assert!(
            (terminal_bounds(&second_terminal, &cx).bounds.size.width
                - before.bounds.size.width
                - px(120.))
            .abs()
                < px(0.01)
        );
        cx.update(|_, cx| {
            assert!(
                second_terminal
                    .update(cx, |terminal, _| terminal.take_input_log())
                    .is_empty()
            )
        });
        let before = terminal_bounds(&second_terminal, &cx);
        cx.dispatch_action(Toggle);
        cx.dispatch_action(IncreaseWidth);
        cx.dispatch_action(IncreaseHeight);
        cx.dispatch_action(Toggle);
        assert_eq!(terminal_bounds(&second_terminal, &cx), before);
        let search_editor = cx.update(|window, cx| {
            let editor = cx.new(|cx| editor::Editor::single_line(window, cx));
            let field = cx.new(|_| TestSearchField {
                editor: editor.clone(),
            });
            second
                .read(cx)
                .toolbar()
                .clone()
                .update(cx, |toolbar, cx| toolbar.add_item(field, window, cx));
            editor.focus_handle(cx).focus(window, cx);
            editor
        });
        cx.run_until_parked();
        let before = terminal_bounds(&second_terminal, &cx);
        cx.simulate_keystrokes("super-left");
        assert!(
            (terminal_bounds(&second_terminal, &cx).bounds.size.width - before.bounds.size.width
                + px(40.))
            .abs()
                < px(0.01)
        );
        cx.update(|window, cx| assert!(search_editor.focus_handle(cx).is_focused(window)));
        let before = terminal_bounds(&second_terminal, &cx);
        let modal = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| TestModal {
                    focus_handle: cx.focus_handle(),
                });
                workspace.active_modal::<TestModal>(cx).unwrap()
            })
        });
        for action in [
            IncreaseWidth.boxed_clone(),
            DecreaseWidth.boxed_clone(),
            IncreaseHeight.boxed_clone(),
            DecreaseHeight.boxed_clone(),
        ] {
            cx.update(|window, cx| window.dispatch_action(action, cx));
            cx.run_until_parked();
        }
        assert_eq!(terminal_bounds(&second_terminal, &cx), before);
        cx.update(|window, cx| assert!(modal.focus_handle(cx).is_focused(window)));
    }

    #[gpui::test]
    async fn floating_terminal_input_visibility_navigation_and_exit(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        cx.update(|cx| {
            let bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )
            .expect("default keymap");
            cx.bind_keys(bindings);
            cx.bind_keys([
                gpui::KeyBinding::new(
                    "ctrl-tab",
                    workspace::ActivateNextPane,
                    Some("Terminal && !AgentPanel"),
                ),
                gpui::KeyBinding::new(
                    "ctrl-shift-tab",
                    workspace::ActivatePreviousPane,
                    Some("Terminal && !AgentPanel"),
                ),
            ]);
            cx.bind_keys([
                gpui::KeyBinding::new("ctrl-7", Toggle, Some("Workspace")),
                gpui::KeyBinding::new("ctrl-tab", Next, Some("FloatingTerminal")),
                gpui::KeyBinding::new("ctrl-shift-tab", Previous, Some("FloatingTerminal")),
                gpui::KeyBinding::new("ctrl-shift-w", Close, Some("FloatingTerminal")),
            ]);
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let (first, pending_first) = starting_pane(&workspace, &mut cx);
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &first, &pending_first, &terminal, &mut cx).await;
        let first_view = first.read_with(&cx, |pane, _| {
            pane.active_item()
                .unwrap()
                .downcast::<TerminalView>()
                .unwrap()
        });
        cx.simulate_keystrokes("escape ctrl-c");
        cx.update(|_, cx| {
            assert_eq!(
                terminal.update(cx, |terminal, _| terminal.take_input_log()),
                vec![vec![0x1b], vec![0x03]]
            )
        });
        cx.simulate_keystrokes("ctrl-tab ctrl-shift-tab");
        cx.update(|window, cx| assert!(first_view.focus_handle(cx).is_focused(window)));
        cx.simulate_keystrokes("ctrl-7");
        assert!(!layer.read_with(&cx, |layer, _| layer.is_visible()));
        cx.update(|_, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_raw_output(b"output while hidden", cx)
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-7");
        cx.update(|window, cx| {
            assert!(first_view.focus_handle(cx).is_focused(window));
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
            assert!(
                terminal
                    .read(cx)
                    .get_content()
                    .contains("output while hidden")
            );
        });
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        let second_terminal = cx.update(|_, cx| display_terminal(cx));
        complete(
            &workspace,
            &second,
            &pending_second,
            &second_terminal,
            &mut cx,
        )
        .await;
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(
            layer.read_with(&cx, |layer, _| layer.active_pane()),
            Some(first.clone())
        );
        cx.update(|window, cx| assert!(first_view.focus_handle(cx).is_focused(window)));
        cx.simulate_keystrokes("ctrl-shift-tab");
        assert_eq!(
            layer.read_with(&cx, |layer, _| layer.active_pane()),
            Some(second.clone())
        );
        cx.update(|window, cx| assert!(second.read(cx).has_focus(window, cx)));
        cx.simulate_keystrokes("ctrl-shift-w");
        assert!(layer.read_with(&cx, |layer, _| layer.pane(second.entity_id()).is_none()));
        first_view.update(&mut cx, |_, cx| cx.emit(ItemEvent::CloseItem));
        cx.run_until_parked();
        assert!(!layer.read_with(&cx, |layer, _| layer.has_panes()));
    }

    #[gpui::test]
    async fn floating_terminal_creation_does_not_steal_focus_when_hidden_or_in_another_workspace(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let (project, workspace, window) = crate::tests::init_test_with_window(cx).await;
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let (pane, pending) = starting_pane(&workspace, &mut cx);
        cx.run_until_parked();
        cx.update(|window, cx| layer.update(cx, |layer, cx| layer.hide(window, cx)));
        let previous_focus = cx.update(|window, cx| window.focused(cx).unwrap());
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &pane, &pending, &terminal, &mut cx).await;
        cx.update(|window, cx| assert!(previous_focus.contains_focused(window, cx)));
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        cx.run_until_parked();
        let other_workspace = cx.update(|window, cx| {
            let app_state = workspace.read(cx).app_state().clone();
            let workspace = cx.new(|cx| Workspace::new(None, project, app_state, window, cx));
            window
                .root::<workspace::MultiWorkspace>()
                .flatten()
                .unwrap()
                .update(cx, |root, cx| {
                    root.activate(workspace.clone(), None, window, cx);
                });
            workspace
        });
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &second, &pending_second, &terminal, &mut cx).await;
        cx.update(|window, cx| {
            assert!(
                other_workspace
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .has_focus(window, cx)
            )
        });
        assert!(layer.read_with(&cx, |layer, _| layer.is_visible()));
    }

    #[gpui::test]
    async fn floating_terminal_pane_actions_leave_editor_layout_unchanged(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let (left, right) = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let left = workspace.active_pane().clone();
                let right = workspace.split_pane(
                    left.clone(),
                    workspace::SplitDirection::Right,
                    window,
                    cx,
                );
                (left, right)
            })
        });
        let (pane, pending) = starting_pane(&workspace, &mut cx);
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &pane, &pending, &terminal, &mut cx).await;
        let before = workspace.read_with(&cx, |workspace, _| {
            (
                workspace.bounding_box_for_pane(&left),
                workspace.bounding_box_for_pane(&right),
            )
        });
        assert!(before.0.is_some() && before.1.is_some());
        for action in [
            workspace::MovePaneLeft.boxed_clone(),
            workspace::MovePaneRight.boxed_clone(),
            workspace::MovePaneUp.boxed_clone(),
            workspace::MovePaneDown.boxed_clone(),
            workspace::SwapPaneLeft.boxed_clone(),
            workspace::SwapPaneRight.boxed_clone(),
            workspace::SwapPaneUp.boxed_clone(),
            workspace::SwapPaneDown.boxed_clone(),
            workspace::SwapPaneAdjacent.boxed_clone(),
        ] {
            cx.update(|window, cx| window.dispatch_action(action, cx));
            cx.run_until_parked();
            assert_eq!(
                workspace.read_with(&cx, |workspace, _| (
                    workspace.bounding_box_for_pane(&left),
                    workspace.bounding_box_for_pane(&right)
                )),
                before
            );
        }
        cx.dispatch_action(workspace::SplitRight::default());
        cx.dispatch_action(workspace::pane::TogglePinTab);
        assert_eq!(
            workspace.read_with(&cx, |workspace, _| workspace.panes().len()),
            2
        );
        assert_eq!(pane.read_with(&cx, |pane, _| pane.pinned_count()), 0);
    }

    struct TestModal {
        focus_handle: FocusHandle,
    }

    #[gpui::test]
    async fn floating_terminal_parallel_creation_preserves_last_focused_window(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let (first, pending_first) = starting_pane(&workspace, &mut cx);
        let (second, pending_second) = starting_pane(&workspace, &mut cx);
        cx.run_until_parked();
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &first, &pending_first, &terminal, &mut cx).await;
        cx.update(|window, cx| {
            assert_eq!(
                layer.read(cx).focused_pane(window, cx),
                Some(second.clone())
            );
            layer.update(cx, |layer, cx| layer.activate_relative(true, window, cx));
        });
        cx.run_until_parked();
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &second, &pending_second, &terminal, &mut cx).await;
        cx.update(|window, cx| {
            assert_eq!(layer.read(cx).focused_pane(window, cx), Some(first));
        });
    }
    impl Focusable for TestModal {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl EventEmitter<DismissEvent> for TestModal {}
    impl workspace::ModalView for TestModal {}
    impl Render for TestModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(200.)).h(px(80.)).track_focus(&self.focus_handle)
        }
    }

    #[gpui::test]
    async fn floating_terminal_modals_keep_focus_and_block_clicks(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        let (pane, pending) = starting_pane(&workspace, &mut cx);
        let terminal = cx.update(|_, cx| display_terminal(cx));
        complete(&workspace, &pane, &pending, &terminal, &mut cx).await;
        let modal = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| TestModal {
                    focus_handle: cx.focus_handle(),
                });
                workspace.active_modal::<TestModal>(cx).unwrap()
            })
        });
        cx.dispatch_action(Toggle);
        cx.dispatch_action(Next);
        cx.update(|window, cx| {
            assert!(modal.focus_handle(cx).is_focused(window));
            assert!(layer.read(cx).is_visible());
        });
        cx.simulate_click(gpui::point(px(500.), px(400.)), Modifiers::default());
        cx.run_until_parked();
        assert!(workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<TestModal>(cx).is_none()
        }));
        cx.update(|_, cx| {
            assert!(
                terminal
                    .update(cx, |terminal, _| terminal.take_input_log())
                    .is_empty()
            )
        });
    }

    #[gpui::test]
    async fn floating_terminal_reports_shell_creation_error(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(crate::init);
        let (_, workspace, window) = crate::tests::init_test_with_window(cx).await;
        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell =
                        Some(settings::Shell::Program(
                            "__nonexistent_floating_terminal_shell__".to_owned(),
                        ));
                });
            })
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(Toggle);
        let layer = workspace.read_with(&cx, |workspace, _| workspace.floating_panes().clone());
        cx.condition(&layer, |layer, cx| {
            layer.active_pane().is_some_and(|pane| {
                pane.read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<StartingTerminal>())
                    .is_some_and(|item| item.read(cx).error.is_some())
            })
        })
        .await;
        cx.update(|_, cx| {
            let pane = layer.read(cx).active_pane().unwrap();
            let error = pane
                .read(cx)
                .active_item()
                .unwrap()
                .downcast::<StartingTerminal>()
                .unwrap();
            assert!(
                error
                    .read(cx)
                    .error
                    .as_ref()
                    .is_some_and(|error| !error.is_empty())
            );
            assert!(layer.read(cx).is_visible());
        });
        let first_pane = layer.read_with(&cx, |layer, _| layer.active_pane().unwrap());
        cx.dispatch_action(workspace::NewTerminal::default());
        let second_pane = layer.read_with(&cx, |layer, _| layer.active_pane().unwrap());
        assert_ne!(first_pane, second_pane);
        assert!(layer.read_with(&cx, |layer, _| layer.pane(first_pane.entity_id()).is_some()));
        cx.condition(&layer, |_, cx| {
            second_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<StartingTerminal>())
                .is_some_and(|item| item.read(cx).error.is_some())
        })
        .await;
    }
}
