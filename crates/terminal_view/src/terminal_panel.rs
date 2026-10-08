use std::{any::Any, cmp, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration};

use crate::{
    TerminalView, default_working_directory,
    persistence::{
        SerializedItems, SerializedTerminalPanel, deserialize_terminal_panel, serialize_tabs,
    },
};
use breadcrumbs::Breadcrumbs;
use collections::HashMap;
use db::kvp::KeyValueStore;
use futures::{channel::oneshot, future::join_all};
use gpui::{
    Action, Anchor, AnyElement, App, AsyncApp, AsyncWindowContext, ClickEvent, Context, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, IntoElement, ParentElement, Pixels,
    Render, ScrollHandle, Styled, Subscription, Task, TaskExt, WeakEntity, Window, actions,
};
use itertools::Itertools;
use project::{Fs, Project};

use settings::{ClosePosition, Settings, ShowCloseButton, TerminalDockPosition};
use task::{RevealStrategy, RevealTarget, Shell, ShellBuilder, SpawnInTerminal, TaskId};
use terminal::{Terminal, terminal_settings::TerminalSettings};
use theme_settings::ThemeSettings;
use ui::{
    ButtonLike, Clickable, CommonAnimationExt, ContextMenu, FluentBuilder, IconButtonShape,
    Indicator, PopoverMenu, PopoverMenuHandle, SplitButton, Tab, TabBar, TabCloseSide, TabPosition,
    Toggleable, Tooltip, prelude::*,
};
use util::{ResultExt, TryFutureExt, defer};
use workspace::{
    ActivateNextPane, ActivatePane, ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight,
    ActivatePaneUp, ActivatePreviousPane, CloseAllItems, DraggedSelection, DraggedTab, ItemHandle,
    ItemId, ItemSettings, MoveItemToPane, MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft,
    MovePaneRight, MovePaneUp, Pane, PaneGroup, SplitDirection, SplitDown, SplitLeft, SplitMode,
    SplitRight, SplitUp, SwapPaneDown, SwapPaneLeft, SwapPaneRight, SwapPaneUp, ToggleZoom,
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    item::{SerializableItem, TabContentParams, TabTooltipContent},
    pane, render_item_indicator,
};

use anyhow::{Result, anyhow};
use zed_actions::assistant::InlineAssist;

const TERMINAL_PANEL_KEY: &str = "TerminalPanel";

actions!(
    terminal_panel,
    [
        /// Toggles the terminal panel.
        Toggle,
        /// Toggles focus on the terminal panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
            workspace.register_action(TerminalPanel::open_terminal);
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    if !workspace.toggle_panel_focus::<TerminalPanel>(window, cx) {
                        workspace.close_panel::<TerminalPanel>(window, cx);
                    }
                }
            });
        },
    )
    .detach();
}

struct TerminalTab {
    center: PaneGroup,
    active_pane: Entity<Pane>,
}

impl TerminalTab {
    fn new(pane: Entity<Pane>) -> Self {
        Self {
            center: PaneGroup::new(pane.clone()),
            active_pane: pane,
        }
    }
}

#[derive(Clone)]
struct DraggedTerminalTab {
    index: usize,
    item: Box<dyn ItemHandle>,
}

impl Render for DraggedTerminalTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();
        let label = self.item.tab_content(
            TabContentParams {
                detail: Some(0),
                selected: false,
                preview: false,
                deemphasized: false,
                max_title_len: None,
                truncate_title_middle: false,
            },
            window,
            cx,
        );
        Tab::new("").child(label).render(window, cx).font(ui_font)
    }
}

pub struct TerminalPanel {
    /// Each tab owns its own split layout. Never empty.
    tabs: Vec<TerminalTab>,
    active_tab: usize,
    tab_bar_scroll_handle: ScrollHandle,
    new_item_menu_handle: PopoverMenuHandle<ContextMenu>,
    split_menu_handle: PopoverMenuHandle<ContextMenu>,
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    pending_serialization: Task<Option<()>>,
    pending_terminals_to_add: usize,
    restoring: bool,
    _restoration: Task<()>,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    active: bool,
    _focus_subscription: Subscription,
}

impl TerminalPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project();
        let pane = new_terminal_pane(workspace.weak_handle(), project.clone(), false, window, cx);
        let focus_handle = cx.focus_handle();
        // The panel root receives focus on mouse down outside of a split (e.g. on the tab bar),
        // and when a focused split stops rendering because another tab was activated.
        // Without a pane or terminal in the focus path, typing and pane keybindings stop working.
        let focus_subscription = cx.on_focus(&focus_handle, window, |this, window, cx| {
            this.active_pane().focus_handle(cx).focus(window, cx);
        });
        Self {
            tabs: vec![TerminalTab::new(pane)],
            active_tab: 0,
            tab_bar_scroll_handle: ScrollHandle::new(),
            new_item_menu_handle: PopoverMenuHandle::default(),
            split_menu_handle: PopoverMenuHandle::default(),
            focus_handle,
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            pending_serialization: Task::ready(None),
            pending_terminals_to_add: 0,
            restoring: false,
            _restoration: Task::ready(()),
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            active: false,
            _focus_subscription: focus_subscription,
        }
    }

    pub fn set_assistant_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.assistant_enabled = enabled;
        cx.notify();
    }

    fn active_tab(&self) -> &TerminalTab {
        &self.tabs[self.active_tab]
    }

    pub(crate) fn active_pane(&self) -> &Entity<Pane> {
        &self.active_tab().active_pane
    }

    fn all_panes(&self) -> impl Iterator<Item = &Entity<Pane>> {
        self.tabs.iter().flat_map(|tab| tab.center.panes())
    }

    fn tab_index_for_pane(&self, pane: &Entity<Pane>) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.center.panes().contains(&pane))
    }

    fn terminal_count(&self, cx: &App) -> usize {
        self.all_panes().map(|pane| pane.read(cx).items_len()).sum()
    }

    fn activate_tab(
        &mut self,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let pane_to_focus = (focus || self.focus_handle.contains_focused(window, cx))
            .then(|| tab.active_pane.clone());
        self.active_tab = index;
        self.tab_bar_scroll_handle.scroll_to_item(index);
        if let Some(pane) = pane_to_focus {
            window.focus(&pane.focus_handle(cx), cx);
        }
        self.serialize(cx);
        cx.notify();
    }

    /// Inserts a tab with the given pane right after the active tab.
    fn insert_tab(
        &mut self,
        pane: Entity<Pane>,
        activate: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = (self.active_tab + 1).min(self.tabs.len());
        self.tabs.insert(index, TerminalTab::new(pane));
        if activate {
            self.activate_tab(index, focus, window, cx);
        } else {
            self.serialize(cx);
            cx.notify();
        }
    }

    /// Adds a terminal to a new tab, reusing the active tab when it holds no terminal yet.
    fn add_terminal_tab(
        &mut self,
        item: Box<dyn ItemHandle>,
        activate: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_tab = self.active_tab();
        if active_tab.center.panes().len() == 1 && active_tab.active_pane.read(cx).items_len() == 0
        {
            let pane = active_tab.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.add_item(item, true, focus, None, window, cx);
            });
            return;
        }

        let Ok(project) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.project().clone())
        else {
            return;
        };
        let zoomed = self.active_pane().read(cx).is_zoomed();
        let pane = new_terminal_pane(self.workspace.clone(), project, zoomed, window, cx);
        pane.update(cx, |pane, cx| {
            pane.add_item(item, true, focus, None, window, cx);
        });
        self.insert_tab(pane, activate, focus, window, cx);
    }

    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let active_tab = self
            .tabs
            .get(self.active_tab)
            .map(|tab| tab.active_pane.clone());
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(active_pane) = active_tab
            && let Some(index) = self.tab_index_for_pane(&active_pane)
        {
            self.active_tab = index;
        }
        self.serialize(cx);
        cx.notify();
    }

    /// Closes every terminal of a tab; the tab itself goes away once its last pane is removed.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let (empty_panes, panes_with_items): (Vec<_>, Vec<_>) = tab
            .center
            .panes()
            .into_iter()
            .cloned()
            .partition(|pane| pane.read(cx).items_len() == 0);
        // Closing items of an empty pane is a no-op that never emits `pane::Event::Remove`.
        for pane in empty_panes {
            self.remove_pane(&pane, None, window, cx);
        }
        for pane in panes_with_items {
            pane.update(cx, |pane, cx| {
                pane.close_all_items(
                    &CloseAllItems {
                        save_intent: None,
                        close_pinned: true,
                    },
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
        }
    }

    fn remove_pane(
        &mut self,
        pane: &Entity<Pane>,
        focus_on_pane: Option<&Entity<Pane>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_index) = self.tab_index_for_pane(pane) else {
            return;
        };
        let had_focus = self.focus_handle.contains_focused(window, cx);
        let is_active_tab = tab_index == self.active_tab;
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        match tab.center.remove(pane, cx) {
            Ok(true) => {
                if &tab.active_pane == pane {
                    tab.active_pane = focus_on_pane
                        .filter(|focus_on_pane| tab.center.panes().contains(focus_on_pane))
                        .cloned()
                        .unwrap_or_else(|| tab.center.last_pane());
                }
                if is_active_tab && had_focus {
                    window.focus(&tab.active_pane.focus_handle(cx), cx);
                }
            }
            Ok(false) => {
                if self.tabs.len() > 1 {
                    self.tabs.remove(tab_index);
                    if tab_index < self.active_tab {
                        self.active_tab -= 1;
                    } else if is_active_tab {
                        self.active_tab = tab_index.min(self.tabs.len() - 1);
                        if had_focus {
                            window.focus(&self.active_pane().focus_handle(cx), cx);
                        }
                    }
                } else {
                    pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                    cx.emit(PanelEvent::Close);
                }
            }
            Err(error) => {
                log::error!("failed to remove terminal pane: {error:#}");
            }
        }
        self.serialize(cx);
        cx.notify();
    }

    /// Installs restored tabs ahead of the terminals opened while restoring, and returns the
    /// number of restored terminals. A terminal opened while restoring stays active.
    pub(crate) fn restore_tabs(
        &mut self,
        restored_tabs: Vec<(PaneGroup, Entity<Pane>)>,
        active_restored_tab: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let restored_terminals = restored_tabs
            .iter()
            .flat_map(|(pane_group, _)| pane_group.panes())
            .map(|pane| pane.read(cx).items_len())
            .sum();
        if restored_tabs.is_empty() {
            return restored_terminals;
        }

        let interim_active_pane = (self.terminal_count(cx) > 0).then(|| self.active_pane().clone());
        let interim_tabs = std::mem::take(&mut self.tabs).into_iter().filter(|tab| {
            tab.center
                .panes()
                .into_iter()
                .any(|pane| pane.read(cx).items_len() > 0)
        });
        self.tabs = restored_tabs
            .into_iter()
            .map(|(center, active_pane)| TerminalTab {
                center,
                active_pane,
            })
            .chain(interim_tabs)
            .collect();
        let active_tab = interim_active_pane
            .and_then(|pane| self.tab_index_for_pane(&pane))
            .unwrap_or(active_restored_tab);
        self.activate_tab(active_tab.min(self.tabs.len() - 1), false, window, cx);
        restored_terminals
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{:?}-{:?}", TERMINAL_PANEL_KEY, id))
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let terminal_panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| TerminalPanel::new(workspace, window, cx))
        })?;

        workspace
            .update(&mut cx, |workspace, _| {
                workspace.set_terminal_provider(TerminalProvider(terminal_panel.clone()))
            })
            .ok();

        terminal_panel.update_in(&mut cx, |panel, window, cx| {
            panel.restoring = true;
            panel._restoration = cx.spawn_in(window, {
                let workspace = workspace.clone();
                async move |terminal_panel, cx| {
                    let restored =
                        Self::restore_serialized_state(workspace, terminal_panel.clone(), cx)
                            .await
                            .log_err()
                            .unwrap_or(false);
                    let default_shell_task = terminal_panel
                        .update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.finish_restoration(restored, window, cx)
                        })
                        .ok()
                        .flatten();
                    if let Some(task) = default_shell_task {
                        task.await.log_err();
                    }
                }
            });
        })?;

        Ok(terminal_panel)
    }

    fn finish_restoration(
        &mut self,
        restored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<WeakEntity<Terminal>>>> {
        self.restoring = false;
        if restored || self.terminal_count(cx) > 0 {
            self.serialize(cx);
        }
        cx.notify();
        if self.active && self.has_no_terminals(cx) {
            let working_directory = self
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
                .ok()
                .flatten();
            Some(self.add_terminal_shell(
                false,
                working_directory,
                RevealStrategy::Always,
                window,
                cx,
            ))
        } else {
            None
        }
    }

    async fn restore_serialized_state(
        workspace: WeakEntity<Workspace>,
        terminal_panel: WeakEntity<Self>,
        cx: &mut AsyncWindowContext,
    ) -> Result<bool> {
        let mut restored = false;
        if let Some((database_id, serialization_key, kvp)) = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .database_id()
                    .zip(TerminalPanel::serialization_key(workspace))
                    .map(|(id, key)| (id, key, KeyValueStore::global(cx)))
            })
            .ok()
            .flatten()
            && let Some(serialized_panel) = cx
                .background_spawn(async move { kvp.read_kvp(&serialization_key) })
                .await
                .log_err()
                .flatten()
                .map(|panel| serde_json::from_str::<SerializedTerminalPanel>(&panel))
                .transpose()
                .log_err()
                .flatten()
        {
            let started_at = std::time::Instant::now();
            let deserialized = workspace
                .update_in(cx, |workspace, window, cx| {
                    deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        database_id,
                        serialized_panel,
                        terminal_panel.clone(),
                        window,
                        cx,
                    )
                })?
                .await;
            if let Some(restored_terminals) = deserialized.log_err() {
                restored = restored_terminals > 0;
                log::debug!(
                    "terminal panel: restored {restored_terminals} serialized terminal(s) in {:?}",
                    started_at.elapsed()
                );
            }
        }

        // Since panels/docks are loaded outside from the workspace, we cleanup here, instead of through the workspace.
        let cleanup = workspace.update_in(cx, |workspace, window, cx| {
            let alive_item_ids = terminal_panel.upgrade().map(|terminal_panel| {
                terminal_panel
                    .read(cx)
                    .all_panes()
                    .flat_map(|pane| pane.read(cx).items())
                    .map(|item| item.item_id().as_u64() as ItemId)
                    .collect::<Vec<_>>()
            });
            alive_item_ids
                .zip(workspace.database_id())
                .map(|(alive_item_ids, workspace_id)| {
                    let cleanup_task =
                        TerminalView::cleanup(workspace_id, alive_item_ids.clone(), window, cx);
                    (cleanup_task, alive_item_ids)
                })
        })?;
        if let Some((cleanup_task, alive_item_ids)) = cleanup {
            cleanup_task.await.log_err();
            terminal_panel
                .update(cx, |terminal_panel, cx| {
                    let terminals_to_reserialize = terminal_panel
                        .all_panes()
                        .flat_map(|pane| {
                            pane.read(cx)
                                .items()
                                .filter(|item| {
                                    !alive_item_ids.contains(&(item.item_id().as_u64() as ItemId))
                                })
                                .filter_map(|item| item.act_as::<TerminalView>(cx))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    for terminal_view in terminals_to_reserialize {
                        terminal_view.update(cx, |terminal_view, cx| {
                            terminal_view.mark_needs_serialize(cx)
                        });
                    }
                })
                .ok();
        }

        let should_focus = workspace
            .update_in(cx, |workspace, window, cx| {
                !workspace.has_active_modal(window, cx)
                    && terminal_panel.upgrade().is_some_and(|terminal_panel| {
                        workspace.active_item(cx).is_none()
                            && workspace
                                .is_dock_at_position_open(terminal_panel.position(window, cx), cx)
                    })
            })
            .unwrap_or(false);
        if should_focus {
            terminal_panel
                .update_in(cx, |panel, window, cx| {
                    panel.active_pane().update(cx, |pane, cx| {
                        pane.focus_active_item(window, cx);
                    });
                })
                .ok();
        }
        Ok(restored)
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { .. } => self.serialize(cx),
            pane::Event::RemovedItem { .. } => self.serialize(cx),
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                self.serialize(cx);
            }
            pane::Event::Remove { focus_on_pane } => {
                self.remove_pane(pane, focus_on_pane.as_ref(), window, cx);
            }
            pane::Event::ZoomIn => {
                for pane in self.all_panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(true, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.all_panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            &pane::Event::Split { direction, mode } => {
                // Each split holds a single terminal, so `Pane::split` already turns `MovePane`
                // into an `EmptyPane` split.
                let clone = matches!(mode, SplitMode::ClonePane);
                self.split_pane(pane.clone(), direction, clone, window, cx);
            }
            pane::Event::Focus => {
                if let Some(tab_index) = self.tab_index_for_pane(pane)
                    && let Some(tab) = self.tabs.get_mut(tab_index)
                {
                    tab.active_pane = pane.clone();
                    self.active_tab = tab_index;
                    self.serialize(cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn split_pane(
        &mut self,
        source_pane: Entity<Pane>,
        direction: SplitDirection,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_pane = self.new_pane_with_terminal(&source_pane, clone, window, cx);
        cx.spawn_in(window, async move |panel, cx| {
            let Some(new_pane) = new_pane.await else {
                return;
            };
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.install_split(&source_pane, new_pane, direction, window, cx);
                })
                .ok();
        })
        .detach();
    }

    fn install_split(
        &mut self,
        source_pane: &Entity<Pane>,
        new_pane: Entity<Pane>,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The source split may have been closed while the new terminal was starting.
        let Some(tab_index) = self.tab_index_for_pane(source_pane) else {
            self.insert_tab(new_pane, true, false, window, cx);
            return;
        };
        let is_active_tab = tab_index == self.active_tab;
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.center.split(source_pane, &new_pane, direction, cx);
        if is_active_tab {
            window.focus(&new_pane.focus_handle(cx), cx);
        }
        self.serialize(cx);
        cx.notify();
    }

    fn new_pane_with_terminal(
        &mut self,
        source_pane: &Entity<Pane>,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let workspace = workspace.read(cx);
        let database_id = workspace.database_id();
        let weak_workspace = self.workspace.clone();
        let project = workspace.project().clone();
        let terminal_view = if clone {
            source_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
        } else {
            None
        };
        let working_directory = if clone {
            terminal_view
                .as_ref()
                .and_then(|terminal_view| {
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .working_directory()
                })
                .or_else(|| default_working_directory(workspace, cx))
        } else {
            default_working_directory(workspace, cx)
        };

        let is_zoomed = source_pane.read(cx).is_zoomed();
        cx.spawn_in(window, async move |panel, cx| {
            let terminal = project
                .update(cx, |project, cx| match terminal_view {
                    Some(view) => project.clone_terminal(
                        &view.read(cx).terminal.clone(),
                        cx,
                        working_directory,
                    ),
                    None => project.create_terminal_shell(working_directory, cx),
                })
                .await
                .log_err()?;

            panel
                .update_in(cx, move |_, window, cx| {
                    let terminal_view = Box::new(cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            weak_workspace.clone(),
                            database_id,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    }));
                    let pane = new_terminal_pane(weak_workspace, project, is_zoomed, window, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item(terminal_view, true, false, None, window, cx);
                    });
                    Some(pane)
                })
                .ok()
                .flatten()
        })
    }

    pub fn open_terminal(
        workspace: &mut Workspace,
        action: &workspace::OpenTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| {
                panel.add_terminal_shell(
                    action.local,
                    Some(action.working_directory.clone()),
                    RevealStrategy::Always,
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    pub fn spawn_task(
        &mut self,
        task: &SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow!("failed to read workspace")));
        };

        let project = workspace.read(cx).project().read(cx);

        if project.is_via_collab() {
            return Task::ready(Err(anyhow!("cannot spawn tasks as a guest")));
        }

        let remote_client = project.remote_client();
        let is_windows = project.path_style(cx).is_windows();
        let remote_shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell());

        let shell = if let Some(remote_shell) = remote_shell
            && task.shell == Shell::System
        {
            Shell::Program(remote_shell)
        } else {
            task.shell.clone()
        };

        let task = prepare_task_for_spawn(task, &shell, is_windows);

        if task.allow_concurrent_runs && task.use_new_terminal {
            return self.spawn_in_new_terminal(task, window, cx);
        }

        let mut terminals_for_task = self.terminals_for_task(&task.full_label, cx);
        let Some(existing) = terminals_for_task.pop() else {
            return self.spawn_in_new_terminal(task, window, cx);
        };

        let (existing_item_index, task_pane, existing_terminal) = existing;
        if task.allow_concurrent_runs {
            return self.replace_terminal(
                task,
                task_pane,
                existing_item_index,
                existing_terminal,
                window,
                cx,
            );
        }

        let (tx, rx) = oneshot::channel();

        self.deferred_tasks.insert(
            task.id.clone(),
            cx.spawn_in(window, async move |terminal_panel, cx| {
                wait_for_terminals_tasks(terminals_for_task, cx).await;
                let task = terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                    if task.use_new_terminal {
                        terminal_panel.spawn_in_new_terminal(task, window, cx)
                    } else {
                        terminal_panel.replace_terminal(
                            task,
                            task_pane,
                            existing_item_index,
                            existing_terminal,
                            window,
                            cx,
                        )
                    }
                });
                if let Ok(task) = task {
                    tx.send(task.await).ok();
                }
            }),
        );

        cx.spawn(async move |_, _| rx.await?)
    }

    fn spawn_in_new_terminal(
        &mut self,
        spawn_task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        match reveal_target {
            RevealTarget::Center => self
                .workspace
                .update(cx, |workspace, cx| {
                    Self::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_task(spawn_task, cx)
                    })
                })
                .unwrap_or_else(|e| Task::ready(Err(e))),
            RevealTarget::Dock => self.add_terminal_task(spawn_task, reveal, window, cx),
        }
    }

    /// Create a new Terminal in the current working directory or the user's home directory
    fn new_terminal(
        workspace: &mut Workspace,
        action: &workspace::NewTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let center_pane = workspace.active_pane();
        let center_pane_has_focus = center_pane.focus_handle(cx).contains_focused(window, cx);
        let active_center_item_is_terminal = center_pane
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<TerminalView>().is_some());

        if center_pane_has_focus && active_center_item_is_terminal {
            let working_directory = default_working_directory(workspace, cx);
            let local = action.local;
            Self::add_center_terminal(workspace, window, cx, move |project, cx| {
                if local {
                    project.create_local_terminal(cx)
                } else {
                    project.create_terminal_shell(working_directory, cx)
                }
            })
            .detach_and_log_err(cx);
            return;
        }

        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |this, cx| {
                this.add_terminal_shell(
                    action.local,
                    default_working_directory(workspace, cx),
                    RevealStrategy::Always,
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    fn terminals_for_task(
        &self,
        label: &str,
        cx: &mut App,
    ) -> Vec<(usize, Entity<Pane>, Entity<TerminalView>)> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };

        let pane_terminal_views = |pane: Entity<Pane>| {
            pane.read(cx)
                .items()
                .enumerate()
                .filter_map(|(index, item)| Some((index, item.act_as::<TerminalView>(cx)?)))
                .filter_map(|(index, terminal_view)| {
                    let task_state = terminal_view.read(cx).terminal().read(cx).task()?;
                    if &task_state.spawned_task.full_label == label {
                        Some((index, terminal_view))
                    } else {
                        None
                    }
                })
                .map(move |(index, terminal_view)| (index, pane.clone(), terminal_view))
        };

        self.all_panes()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(pane_terminal_views)
            .chain(
                workspace
                    .read(cx)
                    .panes()
                    .iter()
                    .cloned()
                    .flat_map(pane_terminal_views),
            )
            .sorted_by_key(|(_, _, terminal_view)| terminal_view.entity_id())
            .collect()
    }

    fn activate_terminal_view(
        &mut self,
        pane: &Entity<Pane>,
        item_index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Set the tab's active pane right away: a later `focus_panel` focuses it,
        // possibly before this pane's focus event arrives.
        if let Some(tab_index) = self.tab_index_for_pane(pane)
            && let Some(tab) = self.tabs.get_mut(tab_index)
        {
            tab.active_pane = pane.clone();
            self.activate_tab(tab_index, false, window, cx);
        }
        pane.update(cx, |pane, cx| {
            pane.activate_item(item_index, true, focus, window, cx)
        })
    }

    pub fn add_center_terminal(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let project = workspace.project().downgrade();
        cx.spawn_in(window, async move |workspace, cx| {
            let terminal = project.update(cx, create_terminal)?.await?;

            workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                });
                // Don't steal focus from an open modal (e.g. the command palette):
                // a background terminal can finish starting up after the user has
                // moved on, and focusing it would dismiss whatever they opened.
                let focus_item = !workspace.has_active_modal(window, cx);
                workspace.add_item_to_active_pane(
                    Box::new(terminal_view),
                    None,
                    focus_item,
                    window,
                    cx,
                );
            })?;
            Ok(terminal.downgrade())
        })
    }

    pub fn add_terminal_task(
        &mut self,
        task: SpawnInTerminal,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        self.spawn_pending_terminal(window, cx, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for remote projects");
            }
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = project
                .update(cx, |project, cx| project.create_terminal_task(task, cx))
                .await?;
            Self::add_terminal_view_to_new_tab(
                &workspace,
                &terminal_panel,
                &terminal,
                reveal_strategy,
                reveal_strategy != RevealStrategy::Never,
                cx,
            )?;
            Ok(terminal.downgrade())
        })
    }

    fn add_terminal_shell(
        &mut self,
        force_local: bool,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        self.spawn_pending_terminal(window, cx, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for collaborative projects");
            }
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = if force_local {
                project
                    .update(cx, |project, cx| project.create_local_terminal(cx))
                    .await
            } else {
                project
                    .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                    .await
            };

            match terminal {
                Ok(terminal) => {
                    Self::add_terminal_view_to_new_tab(
                        &workspace,
                        &terminal_panel,
                        &terminal,
                        reveal_strategy,
                        true,
                        cx,
                    )?;
                    Ok(terminal.downgrade())
                }
                Err(error) => {
                    terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                        let focus = terminal_panel.focus_handle.contains_focused(window, cx);
                        let failed_to_spawn = cx.new(|cx| FailedToSpawnTerminal {
                            error: error.to_string(),
                            focus_handle: cx.focus_handle(),
                        });
                        terminal_panel.add_terminal_tab(
                            Box::new(failed_to_spawn),
                            true,
                            focus,
                            window,
                            cx,
                        );
                    })?;
                    Err(error)
                }
            }
        })
    }

    fn add_terminal_view_to_new_tab(
        workspace: &WeakEntity<Workspace>,
        terminal_panel: &WeakEntity<Self>,
        terminal: &Entity<Terminal>,
        reveal_strategy: RevealStrategy,
        activate: bool,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let (terminal_view, take_focus) = workspace.update_in(cx, |workspace, window, cx| {
            let terminal_view = Box::new(cx.new(|cx| {
                TerminalView::new(
                    terminal.clone(),
                    workspace.weak_handle(),
                    workspace.database_id(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            }));
            let take_focus = reveal_strategy == RevealStrategy::Always
                && !workspace.has_active_modal(window, cx);
            (terminal_view, take_focus)
        })?;
        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            terminal_panel.add_terminal_tab(terminal_view, activate, take_focus, window, cx);
        })?;
        workspace.update_in(cx, |workspace, window, cx| match reveal_strategy {
            RevealStrategy::Always if take_focus => {
                workspace.focus_panel::<Self>(window, cx);
            }
            RevealStrategy::Always | RevealStrategy::NoFocus => {
                workspace.open_panel::<Self>(window, cx);
            }
            RevealStrategy::Never => {}
        })
    }

    fn spawn_pending_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        create_terminal: impl AsyncFnOnce(
            WeakEntity<Self>,
            &mut AsyncWindowContext,
        ) -> Result<WeakEntity<Terminal>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.pending_terminals_to_add += 1;
        cx.notify();
        let decrement_when_cancelled = defer({
            let terminal_panel = cx.weak_entity();
            let cx = cx.to_async();
            move || {
                cx.spawn(async move |cx| {
                    terminal_panel
                        .update(cx, |terminal_panel, cx| {
                            terminal_panel.finish_pending_terminal(cx)
                        })
                        .ok();
                })
                .detach();
            }
        });
        cx.spawn_in(window, async move |terminal_panel, cx| {
            let result = create_terminal(terminal_panel.clone(), cx).await;
            decrement_when_cancelled.abort();
            terminal_panel
                .update(cx, |terminal_panel, cx| {
                    terminal_panel.finish_pending_terminal(cx);
                    if result.is_ok() {
                        terminal_panel.serialize(cx);
                    }
                })
                .ok();
            result
        })
    }

    fn finish_pending_terminal(&mut self, cx: &mut Context<Self>) {
        self.pending_terminals_to_add = self.pending_terminals_to_add.saturating_sub(1);
        cx.notify();
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        if self.restoring {
            return;
        }
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                TerminalPanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.spawn(async move |terminal_panel, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let terminal_panel = terminal_panel.upgrade()?;
            let items = terminal_panel.update(cx, |terminal_panel, cx| {
                SerializedItems::WithTabs(serialize_tabs(
                    terminal_panel
                        .tabs
                        .iter()
                        .map(|tab| (&tab.center, &tab.active_pane)),
                    terminal_panel.active_tab,
                    cx,
                ))
            });
            cx.background_spawn(
                async move {
                    kvp.write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedTerminalPanel {
                            items,
                            active_item_id: None,
                        })?,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    fn replace_terminal(
        &self,
        spawn_task: SpawnInTerminal,
        task_pane: Entity<Pane>,
        terminal_item_index: usize,
        terminal_to_replace: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let task_workspace = self.workspace.clone();
        cx.spawn_in(window, async move |terminal_panel, cx| {
            let project = terminal_panel.update(cx, |this, cx| {
                this.workspace
                    .update(cx, |workspace, _| workspace.project().clone())
            })??;
            let new_terminal = project
                .update(cx, |project, cx| {
                    project.create_terminal_task(spawn_task, cx)
                })
                .await?;
            terminal_to_replace.update_in(cx, |terminal_to_replace, window, cx| {
                terminal_to_replace.set_terminal(new_terminal.clone(), window, cx);
            })?;

            let reveal_target = terminal_panel.update(cx, |panel, _| {
                if panel.tab_index_for_pane(&task_pane).is_some() {
                    RevealTarget::Dock
                } else {
                    RevealTarget::Center
                }
            })?;

            match reveal {
                RevealStrategy::Always => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            let did_activate = workspace.activate_item(
                                &terminal_to_replace,
                                true,
                                true,
                                window,
                                cx,
                            );

                            anyhow::ensure!(did_activate, "Failed to retrieve terminal pane");

                            anyhow::Ok(())
                        })??;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                true,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.focus_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::NoFocus => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            workspace.active_pane().focus_handle(cx).focus(window, cx);
                        })?;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                false,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.open_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::Never => {}
            }

            Ok(new_terminal.downgrade())
        })
    }

    fn has_no_terminals(&self, cx: &App) -> bool {
        self.terminal_count(cx) == 0 && self.pending_terminals_to_add == 0
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    /// Returns all non-empty terminal selections from all terminal views in all panes.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.all_panes()
            .flat_map(|pane| {
                pane.read(cx).items().filter_map(|item| {
                    let terminal_view = item.downcast::<crate::TerminalView>()?;
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .selection_text
                        .clone()
                        .filter(|text| !text.is_empty())
                })
            })
            .collect()
    }

    fn is_enabled(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| is_enabled_in_workspace(workspace.read(cx), cx))
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_tab = self.active_tab;
        let Some(tab) = self.tabs.get_mut(active_tab) else {
            return;
        };
        if let Some(pane) = tab
            .center
            .find_pane_in_direction(&tab.active_pane, direction, cx)
        {
            window.focus(&pane.focus_handle(cx), cx);
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_pane_in_direction(direction, window, cx)
                })
                .ok();
        }
    }

    fn swap_pane_in_direction(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        let active_tab = self.active_tab;
        let Some(tab) = self.tabs.get_mut(active_tab) else {
            return;
        };
        if let Some(to) = tab
            .center
            .find_pane_in_direction(&tab.active_pane, direction, cx)
            .cloned()
        {
            tab.center.swap(&tab.active_pane, &to, cx);
            self.serialize(cx);
            cx.notify();
        }
    }

    fn move_pane_to_border(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        let active_tab = self.active_tab;
        let Some(tab) = self.tabs.get_mut(active_tab) else {
            return;
        };
        match tab.center.move_to_border(&tab.active_pane, direction, cx) {
            Ok(true) => {
                self.serialize(cx);
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => log::error!("failed to move terminal pane: {error:#}"),
        }
    }

    fn activate_relative_tab(
        &mut self,
        offset: isize,
        wrap_around: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_count = self.tabs.len() as isize;
        let mut index = self.active_tab as isize + offset;
        if wrap_around {
            index = index.rem_euclid(tab_count);
        } else {
            index = index.clamp(0, tab_count - 1);
        }
        self.activate_tab(index as usize, true, window, cx);
    }
}

/// Prepares a `SpawnInTerminal` by computing the command, args, and command_label
/// based on the shell configuration. This is a pure function that can be tested
/// without spawning actual terminals.
pub fn prepare_task_for_spawn(
    task: &SpawnInTerminal,
    shell: &Shell,
    is_windows: bool,
) -> SpawnInTerminal {
    let builder = ShellBuilder::new(shell, is_windows);
    let command_label = builder.command_label(task.command.as_deref().unwrap_or(""));
    let (command, args) = builder.build_no_quote(task.command.clone(), &task.args);

    SpawnInTerminal {
        command_label,
        command: Some(command),
        args,
        ..task.clone()
    }
}

fn is_enabled_in_workspace(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).supports_terminal(cx)
}

pub fn new_terminal_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    zoomed: bool,
    window: &mut Window,
    cx: &mut Context<TerminalPanel>,
) -> Entity<Pane> {
    let pane = cx.new(|cx| {
        let can_drop_predicate =
            terminal_pane_can_drop_predicate(cx.weak_entity(), project.clone());
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            Some(can_drop_predicate),
            workspace::NewTerminal::default().boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_zoomed(zoomed, cx);
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_, _| false);
        pane.set_zoom_out_on_close(false);

        let toolbar = pane.toolbar().clone();
        if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
            let languages = Some(project.read(cx).languages().clone());
            (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
        }
        let breadcrumbs = cx.new(|_| Breadcrumbs::new());
        toolbar.update(cx, |toolbar, cx| {
            toolbar.add_item(breadcrumbs, window, cx);
        });

        pane
    });

    cx.subscribe_in(&pane, window, TerminalPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

/// A terminal pane holds exactly one terminal, so it only accepts drops that the terminal
/// consumes itself (pasting paths). Anything else would be opened as a new item in the pane.
fn terminal_pane_can_drop_predicate(
    pane: WeakEntity<Pane>,
    project: Entity<Project>,
) -> Arc<dyn Fn(&dyn Any, &mut Window, &mut App) -> bool> {
    Arc::new(move |dropped, _, cx| {
        let Some(terminal_view) = pane
            .upgrade()
            .and_then(|pane| pane.read(cx).active_item())
            .and_then(|item| item.downcast::<TerminalView>())
        else {
            return false;
        };
        if terminal_view.read(cx).is_read_only() {
            return false;
        }
        if dropped.is::<ExternalPaths>() {
            project.read(cx).is_local()
        } else if dropped.is::<DraggedSelection>() {
            true
        } else if let Some(tab) = dropped.downcast_ref::<DraggedTab>() {
            tab.item.downcast::<TerminalView>().is_none()
                && tab.item.project_path(cx).is_some_and(|project_path| {
                    project.read(cx).absolute_path(&project_path, cx).is_some()
                })
        } else {
            false
        }
    })
}

async fn wait_for_terminals_tasks(
    terminals_for_task: Vec<(usize, Entity<Pane>, Entity<TerminalView>)>,
    cx: &mut AsyncApp,
) {
    let pending_tasks = terminals_for_task.iter().map(|(_, _, terminal)| {
        terminal.update(cx, |terminal_view, cx| {
            terminal_view
                .terminal()
                .update(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
        })
    });
    join_all(pending_tasks).await;
}

struct FailedToSpawnTerminal {
    error: String,
    focus_handle: FocusHandle,
}

impl Focusable for FailedToSpawnTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FailedToSpawnTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover_menu = PopoverMenu::new("settings-popover")
            .trigger(
                IconButton::new("icon-button-popover", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |context_menu, _, _| {
                    context_menu
                        .action("Open Settings", zed_actions::OpenSettings.boxed_clone())
                        .action(
                            "Edit settings.json",
                            zed_actions::OpenSettingsFile.boxed_clone(),
                        )
                }))
            })
            .anchor(Anchor::TopRight)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(2.0),
            });

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w_112()
                    .items_center()
                    .justify_center()
                    .text_center()
                    .child(Label::new("Failed to spawn terminal"))
                    .child(
                        Label::new(self.error.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .mb_4(),
                    )
                    .child(SplitButton::new(
                        ButtonLike::new("open-settings-ui")
                            .child(Label::new("Edit Settings").size(LabelSize::Small))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(zed_actions::OpenSettings.boxed_clone(), cx);
                            }),
                        popover_menu.into_any_element(),
                    )),
            )
    }
}

impl EventEmitter<()> for FailedToSpawnTerminal {}

impl workspace::Item for FailedToSpawnTerminal {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::new_static("Failed to spawn terminal")
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl TerminalPanel {
    fn render_tab_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tabs = (0..self.tabs.len())
            .filter_map(|index| self.render_tab(index, window, cx))
            .collect::<Vec<_>>();
        TabBar::new("terminal-panel-tab-bar")
            .track_scroll(&self.tab_bar_scroll_handle)
            .children(tabs)
            .end_children(self.render_tab_bar_buttons(window, cx))
            .into_any_element()
    }

    fn render_tab(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tab = self.tabs.get(index)?;
        let item = tab.active_pane.read(cx).active_item().or_else(|| {
            tab.center
                .panes()
                .into_iter()
                .find_map(|pane| pane.read(cx).active_item())
        })?;
        let split_count = tab.center.panes().len();
        let is_active = index == self.active_tab;
        let label = item.tab_content(
            TabContentParams {
                detail: Some(0),
                selected: is_active,
                preview: false,
                deemphasized: !self.focus_handle.contains_focused(window, cx),
                max_title_len: None,
                truncate_title_middle: false,
            },
            window,
            cx,
        );
        let tooltip_content = item.tab_tooltip_content(cx);
        let settings = ItemSettings::get_global(cx);
        let close_side = match settings.close_position {
            ClosePosition::Left => TabCloseSide::Start,
            ClosePosition::Right => TabCloseSide::End,
        };
        let close_button = match settings.show_close_button {
            ShowCloseButton::Always => Some(IconButton::new("close-tab", IconName::Close)),
            ShowCloseButton::Hover => {
                Some(IconButton::new("close-tab", IconName::Close).visible_on_hover(""))
            }
            ShowCloseButton::Hidden => None,
        }
        .map(|button| {
            button
                .shape(IconButtonShape::Square)
                .icon_color(Color::Muted)
                .size(ButtonSize::None)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Close Tab"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_tab(index, window, cx);
                }))
        });

        let tab = Tab::new(("terminal-tab", index))
            .position(if index == 0 {
                TabPosition::First
            } else if index + 1 == self.tabs.len() {
                TabPosition::Last
            } else {
                TabPosition::Middle(index.cmp(&self.active_tab))
            })
            .close_side(close_side)
            .toggle_state(is_active)
            .on_click(cx.listener({
                let item = item.boxed_clone();
                move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() > 1
                        && let Some((_, rename_action)) = item
                            .tab_extra_context_menu_actions(window, cx)
                            .into_iter()
                            .find(|(label, _)| label.as_ref() == "Rename")
                    {
                        // Dispatch directly through the focus handle: the rename editor
                        // takes focus, and an intermediate focus change would cancel it.
                        item.item_focus_handle(cx)
                            .dispatch_action(&*rename_action, window, cx);
                        return;
                    }
                    this.activate_tab(index, true, window, cx);
                }
            }))
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_middle_click() {
                    this.close_tab(index, window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_drag(
                DraggedTerminalTab {
                    index,
                    item: item.boxed_clone(),
                },
                |tab, _, _, cx| cx.new(|_| tab.clone()),
            )
            .drag_over::<DraggedTerminalTab>(move |tab, dragged_tab, _, cx| {
                let styled_tab = tab
                    .bg(cx.theme().colors().drop_target_background)
                    .border_color(cx.theme().colors().drop_target_border)
                    .border_0();
                if index < dragged_tab.index {
                    styled_tab.border_l_2()
                } else if index > dragged_tab.index {
                    styled_tab.border_r_2()
                } else {
                    styled_tab
                }
            })
            .on_drop(
                cx.listener(move |this, dragged_tab: &DraggedTerminalTab, _, cx| {
                    this.move_tab(dragged_tab.index, index, cx);
                }),
            )
            .start_slot::<Indicator>(render_item_indicator(item.boxed_clone(), cx))
            .end_slot::<IconButton>(close_button)
            .child(
                h_flex()
                    .id(("terminal-tab-content", index))
                    .gap_1()
                    .child(label)
                    .when(split_count > 1, |this| {
                        this.child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    Icon::new(IconName::Split)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(split_count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .map(|this| match tooltip_content {
                        Some(TabTooltipContent::Text(text)) => this.tooltip(Tooltip::text(text)),
                        Some(TabTooltipContent::Custom(element_fn)) => {
                            this.tooltip(move |window, cx| element_fn(window, cx))
                        }
                        None => this,
                    }),
            );
        Some(tab.into_any_element())
    }

    fn render_tab_bar_buttons(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let has_focus = self.focus_handle.contains_focused(window, cx)
            || self.new_item_menu_handle.is_focused(window, cx)
            || self.split_menu_handle.is_focused(window, cx);
        if !has_focus {
            return None;
        }
        let active_pane = self.active_pane();
        let pane_focus_handle = active_pane.focus_handle(cx);
        let terminal_focus_handle = active_pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<TerminalView>())
            .map(|terminal_view| terminal_view.read(cx).focus_handle.clone());
        let zoomed = active_pane.read(cx).is_zoomed();
        Some(
            h_flex()
                .gap(DynamicSpacing::Base02.rems(cx))
                .child(
                    PopoverMenu::new("terminal-tab-bar-popover-menu")
                        .trigger_with_tooltip(
                            IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
                            Tooltip::text("New…"),
                        )
                        .anchor(Anchor::TopRight)
                        .with_handle(self.new_item_menu_handle.clone())
                        .menu(move |window, cx| {
                            let focus_handle = pane_focus_handle.clone();
                            let menu = ContextMenu::build(window, cx, |menu, _, _| {
                                menu.context(focus_handle.clone())
                                    .action(
                                        "New Terminal",
                                        workspace::NewTerminal::default().boxed_clone(),
                                    )
                                    // We want the focus to go back to terminal panel once task modal is dismissed,
                                    // hence we focus that first. Otherwise, we'd end up without a focused element, as
                                    // context menu will be gone the moment we spawn the modal.
                                    .action("Spawn Task", zed_actions::Spawn::modal().boxed_clone())
                            });

                            Some(menu)
                        }),
                )
                .when(self.assistant_enabled, |this| {
                    this.when_some(terminal_focus_handle.clone(), |this, focus_handle| {
                        this.child(InlineAssistTabBarButton { focus_handle })
                    })
                })
                .child(
                    PopoverMenu::new("terminal-pane-tab-bar-split")
                        .trigger_with_tooltip(
                            IconButton::new("terminal-pane-split", IconName::Split)
                                .icon_size(IconSize::Small),
                            Tooltip::text("Split Pane"),
                        )
                        .anchor(Anchor::TopRight)
                        .with_handle(self.split_menu_handle.clone())
                        .menu(move |window, cx| {
                            ContextMenu::build(window, cx, |menu, _, _| {
                                menu.when_some(
                                    terminal_focus_handle.clone(),
                                    |menu, split_context| menu.context(split_context),
                                )
                                .action("Split Right", SplitRight::default().boxed_clone())
                                .action("Split Left", SplitLeft::default().boxed_clone())
                                .action("Split Up", SplitUp::default().boxed_clone())
                                .action("Split Down", SplitDown::default().boxed_clone())
                            })
                            .into()
                        }),
                )
                .child(
                    IconButton::new("toggle_zoom", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .toggle_state(zoomed)
                        .selected_icon(IconName::Minimize)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.active_pane().clone().update(cx, |pane, cx| {
                                pane.toggle_zoom(&ToggleZoom, window, cx);
                            });
                        }))
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action(
                                if zoomed { "Zoom Out" } else { "Zoom In" },
                                &ToggleZoom,
                                cx,
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let registrar = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane().clone())
            })
            .unwrap_or_else(div);
        let waiting_for_terminals = self.restoring || self.pending_terminals_to_add > 0;
        let restoring_placeholder =
            (waiting_for_terminals && self.terminal_count(cx) == 0).then(|| {
                let label = if self.restoring {
                    "Restoring terminals…"
                } else {
                    "Starting terminal…"
                };
                h_flex()
                    .absolute()
                    .inset_0()
                    .justify_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .color(Color::Muted)
                            .size(IconSize::Small)
                            .with_rotate_animation(2),
                    )
                    .child(Label::new(label).color(Color::Muted))
            });
        let tab_bar = self.render_tab_bar(window, cx);
        self.workspace
            .update(cx, |workspace, cx| {
                let active_tab = self.active_tab();
                registrar
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(tab_bar)
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .child(active_tab.center.render(
                                workspace.zoomed_item(),
                                None,
                                &workspace::PaneRenderContext {
                                    follower_states: &HashMap::default(),
                                    active_call: workspace.active_call(),
                                    active_pane: &active_tab.active_pane,
                                    app_state: workspace.app_state(),
                                    project: workspace.project(),
                                    workspace: &workspace.weak_handle(),
                                },
                                window,
                                cx,
                            ))
                            .children(restoring_placeholder),
                    )
            })
            .ok()
            .map(|div| {
                div.capture_action(cx.listener(
                    |terminal_panel, action: &pane::ActivateItem, window, cx| {
                        if action.0 < terminal_panel.tabs.len() {
                            terminal_panel.activate_tab(action.0, true, window, cx);
                        }
                        cx.stop_propagation();
                    },
                ))
                .capture_action(cx.listener(
                    |terminal_panel, _: &pane::ActivateLastItem, window, cx| {
                        let last_index = terminal_panel.tabs.len().saturating_sub(1);
                        terminal_panel.activate_tab(last_index, true, window, cx);
                        cx.stop_propagation();
                    },
                ))
                .capture_action(cx.listener(
                    |terminal_panel, action: &pane::ActivateNextItem, window, cx| {
                        terminal_panel.activate_relative_tab(1, action.wrap_around, window, cx);
                        cx.stop_propagation();
                    },
                ))
                .capture_action(cx.listener(
                    |terminal_panel, action: &pane::ActivatePreviousItem, window, cx| {
                        terminal_panel.activate_relative_tab(-1, action.wrap_around, window, cx);
                        cx.stop_propagation();
                    },
                ))
                // Tabs can't be pinned; a pinned terminal would ignore `pane::CloseActiveItem`.
                .capture_action(cx.listener(|_, _: &pane::TogglePinTab, _, cx| {
                    cx.stop_propagation();
                }))
                .capture_action(cx.listener(|_, _: &pane::UnpinAllTabs, _, cx| {
                    cx.stop_propagation();
                }))
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneLeft, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Left, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneRight, window, cx| {
                        terminal_panel.activate_pane_in_direction(
                            SplitDirection::Right,
                            window,
                            cx,
                        );
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneUp, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Up, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneDown, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Down, window, cx);
                    })
                })
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivateNextPane, window, cx| {
                        let tab = terminal_panel.active_tab();
                        let panes = tab.center.panes();
                        if let Some(ix) = panes.iter().position(|pane| **pane == tab.active_pane) {
                            let next_ix = (ix + 1) % panes.len();
                            window.focus(&panes[next_ix].focus_handle(cx), cx);
                        }
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, _action: &ActivatePreviousPane, window, cx| {
                        let tab = terminal_panel.active_tab();
                        let panes = tab.center.panes();
                        if let Some(ix) = panes.iter().position(|pane| **pane == tab.active_pane) {
                            let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                            window.focus(&panes[prev_ix].focus_handle(cx), cx);
                        }
                    },
                ))
                .on_action(
                    cx.listener(|terminal_panel, action: &ActivatePane, window, cx| {
                        let tab = terminal_panel.active_tab();
                        if let Some(&pane) = tab.center.panes().get(action.0) {
                            window.focus(&pane.read(cx).focus_handle(cx), cx);
                        } else {
                            let active_pane = tab.active_pane.clone();
                            terminal_panel.split_pane(
                                active_pane,
                                SplitDirection::Right,
                                true,
                                window,
                                cx,
                            );
                        }
                    }),
                )
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneLeft, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneRight, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneUp, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneDown, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Down, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneLeft, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneRight, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneUp, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneDown, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Down, cx);
                }))
                // Each split holds a single terminal, so items can't be moved between splits.
                // Handling these keeps them from reaching the workspace and moving editor tabs.
                .on_action(|_: &MoveItemToPane, _, _| {})
                .on_action(|_: &MoveItemToPaneInDirection, _, _| {})
            })
            .unwrap_or_else(|| div())
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for TerminalPanel {
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane().focus_handle(cx)
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        TerminalSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).starts_open
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => TerminalDockPosition::Left,
                DockPosition::Bottom => TerminalDockPosition::Bottom,
                DockPosition::Right => TerminalDockPosition::Right,
            };
            settings.terminal.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = TerminalSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, _window: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).flexible
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.terminal.get_or_insert_default().flexible = Some(flexible);
        });
    }

    fn is_zoomed(&self, _window: &Window, cx: &App) -> bool {
        self.active_pane().read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.all_panes() {
            pane.update(cx, |pane, cx| {
                pane.set_zoomed(zoomed, cx);
            })
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let old_active = self.active;
        self.active = active;
        if !active || old_active == active || self.restoring || !self.has_no_terminals(cx) {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            let Ok(kind) = this
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
            else {
                return;
            };

            this.add_terminal_shell(false, kind, RevealStrategy::Always, window, cx)
                .detach_and_log_err(cx)
        })
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        if !TerminalSettings::get_global(cx).show_count_badge {
            return None;
        }
        let count = self.terminal_count(cx);
        if count == 0 {
            None
        } else {
            Some(count.to_string())
        }
    }

    fn persistent_name() -> &'static str {
        "TerminalPanel"
    }

    fn panel_key() -> &'static str {
        TERMINAL_PANEL_KEY
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        if (self.is_enabled(cx) || !self.has_no_terminals(cx))
            && TerminalSettings::get_global(cx).button
        {
            Some(IconName::TerminalAlt)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Terminal Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(Toggle)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane().clone())
    }

    fn activation_priority(&self) -> u32 {
        2
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.terminal.get_or_insert_default().button = Some(false);
        }))
    }
}

struct TerminalProvider(Entity<TerminalPanel>);

impl workspace::TerminalProvider for TerminalProvider {
    fn spawn(
        &self,
        task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<Result<ExitStatus>>> {
        let terminal_panel = self.0.clone();
        window.spawn(cx, async move |cx| {
            let terminal = terminal_panel
                .update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.spawn_task(&task, window, cx)
                })
                .ok()?
                .await;
            match terminal {
                Ok(terminal) => {
                    let exit_status = terminal
                        .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                        .ok()?
                        .await?;
                    Some(Ok(exit_status))
                }
                Err(e) => Some(Err(e)),
            }
        })
    }
}

#[derive(IntoElement)]
struct InlineAssistTabBarButton {
    focus_handle: FocusHandle,
}

impl RenderOnce for InlineAssistTabBarButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let focus_handle = self.focus_handle;
        IconButton::new("terminal_inline_assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .on_click({
                let focus_handle = focus_handle.clone();
                move |_, window, cx| {
                    focus_handle.dispatch_action(&InlineAssist::default(), window, cx);
                }
            })
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in("Inline Assist", &InlineAssist::default(), &focus_handle, cx)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use super::*;
    use crate::persistence::{
        SerializedAxis, SerializedPane, SerializedPaneGroup, SerializedTabs, serialize_tabs,
    };
    use gpui::{Modifiers, TestAppContext, UpdateGlobal as _, VisualTestContext};
    use pretty_assertions::assert_eq;
    use project::FakeFs;
    use settings::SettingsStore;
    use std::{cell::Cell, rc::Rc};
    use workspace::{MultiWorkspace, WorkspaceId};

    #[test]
    fn test_prepare_empty_task() {
        let input = SpawnInTerminal::default();
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let expected_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, None);
        assert_eq!(result.shell, Shell::System);
        assert_eq!(
            result.command,
            Some(expected_shell.clone()),
            "Empty tasks should spawn a -i shell"
        );
        assert_eq!(result.args, Vec::<String>::new());
        assert_eq!(
            result.command_label, expected_shell,
            "We show the shell launch for empty commands"
        );
    }

    #[gpui::test]
    async fn test_bypass_max_tabs_limit(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        set_max_tabs(cx, Some(3));

        for _ in 0..5 {
            let task = window_handle
                .update(cx, |_, window, cx| {
                    terminal_panel.update(cx, |panel, cx| {
                        panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                    })
                })
                .unwrap();
            task.await.unwrap();
        }

        cx.run_until_parked();

        let item_count = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        assert_eq!(
            item_count, 5,
            "Terminal panel should bypass max_tabs limit and have all 5 terminals"
        );
        terminal_panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tabs.len(), 5, "each terminal should get its own tab");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare_script_like_task() {
        let user_command = r#"REPO_URL=$(git remote get-url origin | sed -e \"s/^git@\\(.*\\):\\(.*\\)\\.git$/https:\\/\\/\\1\\/\\2/\"); COMMIT_SHA=$(git log -1 --format=\"%H\" -- \"${ZED_RELATIVE_FILE}\"); echo \"${REPO_URL}/blob/${COMMIT_SHA}/${ZED_RELATIVE_FILE}#L${ZED_ROW}-$(echo $(($(wc -l <<< \"$ZED_SELECTED_TEXT\") + $ZED_ROW - 1)))\" | xclip -selection clipboard"#.to_string();
        let expected_cwd = PathBuf::from("/some/work");

        let input = SpawnInTerminal {
            command: Some(user_command.clone()),
            cwd: Some(expected_cwd.clone()),
            ..SpawnInTerminal::default()
        };
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let system_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, Some(expected_cwd));
        assert_eq!(result.shell, Shell::System);
        assert_eq!(result.command, Some(system_shell.clone()));
        assert_eq!(
            result.args,
            vec!["-i".to_string(), "-c".to_string(), user_command.clone()],
            "User command should have been moved into the arguments, as we're spawning a new -i shell",
        );
        assert_eq!(
            result.command_label,
            format!(
                "{system_shell} {interactive}-c '{user_command}'",
                interactive = if cfg!(windows) { "" } else { "-i " }
            ),
            "We want to show to the user the entire command spawned"
        );
    }

    #[gpui::test]
    async fn renders_error_if_default_shell_fails(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell =
                        Some(settings::Shell::Program("__nonexistent_shell__".to_owned()));
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap_err();

        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel
                            .active_pane()
                            .read(cx)
                            .items()
                            .any(|item| item.downcast::<FailedToSpawnTerminal>().is_some()),
                        "should spawn `FailedToSpawnTerminal` pane"
                    );
                    assert_eq!(terminal_panel.pending_terminals_to_add, 0);
                })
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_failed_task_spawn_does_not_leak_pending_terminal_count(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_task(
                        SpawnInTerminal {
                            command: Some("__nonexistent_program__".to_owned()),
                            ..SpawnInTerminal::default()
                        },
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap_err();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
            assert_eq!(terminal_panel.terminal_count(cx), 0);
        });
    }

    #[gpui::test]
    async fn test_pending_terminal_count_tracks_spawn_lifecycle(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        let task = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    let task =
                        terminal_panel.add_terminal_shell(false, None, RevealStrategy::Never, window, cx);
                    assert_eq!(
                        terminal_panel.pending_terminals_to_add, 1,
                        "pending count should be incremented synchronously to avoid double default terminal spawns"
                    );
                    assert!(!terminal_panel.has_no_terminals(cx));
                    task
                })
            })
            .unwrap();
        task.await.unwrap();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
            assert_eq!(terminal_panel.terminal_count(cx), 1);
        });
    }

    #[gpui::test]
    async fn test_pending_terminal_count_resets_when_spawn_cancelled(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    let task = terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    );
                    assert_eq!(terminal_panel.pending_terminals_to_add, 1);
                    drop(task);
                })
            })
            .unwrap();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, _| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
        });
    }

    #[gpui::test]
    async fn test_load_without_serialized_state_does_not_persist_empty_state(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let serialization_key = window_handle
            .update(cx, |multi_workspace, _, cx| {
                TerminalPanel::serialization_key(multi_workspace.workspace().read(cx)).unwrap()
            })
            .unwrap();

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().downgrade();
                window.spawn(cx, async move |cx| {
                    TerminalPanel::load(workspace, cx.clone()).await
                })
            })
            .unwrap()
            .await
            .unwrap();

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert!(!terminal_panel.restoring);
            assert_eq!(terminal_panel.terminal_count(cx), 0);
        });
        let serialized_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert_eq!(serialized_state, None);
    }

    #[gpui::test]
    async fn test_terminal_added_during_restore_is_serialized_after_restore(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let serialization_key = window_handle
            .update(cx, |multi_workspace, _, cx| {
                TerminalPanel::serialization_key(multi_workspace.workspace().read(cx)).unwrap()
            })
            .unwrap();

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.restoring = true;
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap();

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let suppressed_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert_eq!(
            suppressed_state, None,
            "serialization should stay suppressed while the panel is restoring"
        );

        let default_shell_task = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.finish_restoration(false, window, cx)
                })
            })
            .unwrap();
        assert!(
            default_shell_task.is_none(),
            "no default terminal should spawn when a terminal already exists"
        );

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let serialized_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert!(
            serialized_state.is_some(),
            "terminal added during restore must be serialized once restoration finishes"
        );
    }

    #[gpui::test]
    async fn test_legacy_serialized_restore_keeps_interim_terminal_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        let interim_item_id = terminal_panel.read_with(cx, |terminal_panel, cx| {
            let pane = terminal_panel.active_pane().read(cx);
            assert_eq!(pane.items_len(), 1);
            pane.active_item().unwrap().item_id()
        });

        let restored_items = restore(
            &window_handle,
            &terminal_panel,
            SerializedTerminalPanel {
                items: SerializedItems::NoSplits(vec![12345]),
                active_item_id: Some(12345),
            },
            cx,
        )
        .await;

        assert_eq!(restored_items, 1);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(tab_layout(terminal_panel, cx), vec![vec![1], vec![1]]);
            assert_eq!(terminal_panel.active_tab, 1);
            assert_eq!(
                terminal_panel
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .map(|item| item.item_id()),
                Some(interim_item_id),
                "interim terminal must stay active after a legacy format restore"
            );
        });
    }

    #[gpui::test]
    async fn test_restore_keeps_interim_tab_after_restored_tabs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        let interim_pane =
            terminal_panel.read_with(cx, |terminal_panel, _| terminal_panel.active_pane().clone());

        let restored_items = restore(
            &window_handle,
            &terminal_panel,
            SerializedTerminalPanel {
                items: SerializedItems::WithTabs(SerializedTabs {
                    tabs: vec![
                        serialized_pane(false, Some(1)),
                        SerializedPaneGroup::Group {
                            axis: SerializedAxis(gpui::Axis::Horizontal),
                            flexes: None,
                            children: vec![
                                serialized_pane(false, Some(2)),
                                serialized_pane(true, Some(3)),
                            ],
                        },
                    ],
                    active_tab: 1,
                }),
                active_item_id: None,
            },
            cx,
        )
        .await;

        assert_eq!(restored_items, 3);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1], vec![1, 1], vec![1]]
            );
            let restored_split_tab = &terminal_panel.tabs[1];
            assert_eq!(
                restored_split_tab.active_pane,
                restored_split_tab.center.last_pane(),
                "the serialized active split must be restored"
            );
            assert_eq!(terminal_panel.tabs[2].active_pane, interim_pane);
            assert_eq!(
                terminal_panel.active_tab, 2,
                "the interim tab must stay active"
            );
        });
    }

    #[gpui::test]
    async fn test_local_terminal_in_local_project(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        let result = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        true,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await;

        assert!(
            result.is_ok(),
            "local terminal should successfully create in local project"
        );
    }

    struct FocusOnlyModal {
        focus_handle: gpui::FocusHandle,
    }
    impl gpui::EventEmitter<gpui::DismissEvent> for FocusOnlyModal {}
    impl gpui::Focusable for FocusOnlyModal {
        fn focus_handle(&self, _: &gpui::App) -> gpui::FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl Render for FocusOnlyModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().track_focus(&self.focus_handle)
        }
    }
    impl workspace::ModalView for FocusOnlyModal {}

    async fn open_center_display_terminal(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        workspace
            .update_in(cx, |workspace, window, cx| {
                TerminalPanel::add_center_terminal(workspace, window, cx, |_, cx| {
                    let terminal = cx.new(|cx| {
                        terminal::TerminalBuilder::new_display_only(
                            terminal::terminal_settings::CursorShape::default(),
                            terminal::terminal_settings::AlternateScroll::On,
                            None,
                            0,
                            cx.background_executor(),
                            util::paths::PathStyle::local(),
                        )
                        .subscribe(cx)
                    });
                    gpui::Task::ready(Ok(terminal))
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_center_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.has_active_modal(window, cx));
            assert!(
                modal_focus_handle.is_focused(window),
                "the modal should hold focus before the terminal is created"
            );
        });

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |_, window, _| {
            assert!(
                modal_focus_handle.is_focused(window),
                "a background center terminal must not steal focus from an active modal"
            );
        });
    }

    #[gpui::test]
    async fn test_center_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(!workspace.has_active_modal(window, cx));
            let terminal_view = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new center terminal should be the active item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new center terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_panel_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "a panel terminal that finishes spawning must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "a panel terminal that finishes spawning must not steal focus from an active modal"
            );
        });
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                terminal_panel.terminal_count(cx),
                1,
                "the terminal should still be added to the panel"
            );
        });
    }

    #[gpui::test]
    async fn test_panel_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            let terminal_view = terminal_panel
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new terminal should be the active panel item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new panel terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_task_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_task(echo_task(), RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a task terminal");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "a task terminal that finishes spawning must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "a task terminal that finishes spawning must not steal focus from an active modal"
            );
        });
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                terminal_panel.terminal_count(cx),
                1,
                "the task terminal should still be added to the panel"
            );
        });
    }

    #[gpui::test]
    async fn test_task_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_task(echo_task(), RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a task terminal");
        cx.run_until_parked();

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            let terminal_view = terminal_panel
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new task terminal should be the active panel item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new task terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_finished_restoration_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Never, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<TerminalPanel>(window, cx);
        });
        cx.run_until_parked();

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.active_item(cx).is_none()
                    && workspace
                        .is_dock_at_position_open(terminal_panel.read(cx).position(window, cx), cx),
                "the restoration focus conditions should hold, otherwise this test is vacuous"
            );
        });

        window_handle
            .update(cx, |_, window, cx| {
                let workspace = workspace.downgrade();
                let terminal_panel = terminal_panel.downgrade();
                window.spawn(cx, async move |cx| {
                    TerminalPanel::restore_serialized_state(workspace, terminal_panel, cx).await
                })
            })
            .expect("Failed to restore serialized state")
            .await
            .expect("Failed to restore serialized state");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "finishing restoration must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "finishing restoration must not steal focus from an active modal"
            );
        });
    }

    #[gpui::test]
    async fn test_inline_assist_tooltip_shows_keybinding_of_active_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-enter",
                InlineAssist::default(),
                Some("Terminal"),
            )])
        });

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel.update(cx, |panel, cx| panel.set_assistant_enabled(true, cx));
        terminal_panel
            .update_in(cx, |panel, window, cx| {
                panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let button_bounds = cx
            .debug_bounds("ICON-ZedAssistant")
            .expect("inline assist button should be rendered in the terminal tab bar");
        cx.simulate_mouse_move(button_bounds.center(), None, Modifiers::default());

        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("KEY_BINDING-enter").is_some(),
            "tooltip should show the InlineAssist keybinding resolved in the terminal's context"
        );
    }

    // On Windows `echo` is a shell builtin rather than an executable, so spawning it directly fails.
    fn echo_task() -> SpawnInTerminal {
        let (command, args) = if cfg!(windows) {
            ("cmd.exe", vec!["/C".to_owned(), "echo".to_owned()])
        } else {
            ("echo", Vec::new())
        };
        SpawnInTerminal {
            command: Some(command.to_owned()),
            args,
            ..SpawnInTerminal::default()
        }
    }

    async fn init_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<TerminalPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel.clone(), window, cx);
                    panel
                })
            })
            .expect("Failed to initialize workspace with terminal panel");

        (window_handle, terminal_panel)
    }

    #[gpui::test]
    async fn test_terminal_panel_starts_open_follows_setting(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        !terminal_panel.starts_open(window, cx),
                        "terminal panel should not start open by default"
                    );
                });
            })
            .expect("Failed to read terminal panel starts_open default");

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(true);
            });
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel.starts_open(window, cx),
                        "terminal panel should start open when configured"
                    );
                });
            })
            .expect("Failed to read configured terminal panel starts_open");
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_by_default(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        let panel_items_before = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");

        cx.run_until_parked();

        let panel_items_after = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "Terminal should be added to the panel when no center terminal is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        assert_eq!(center_items_before, 1, "Center pane should have 1 terminal");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let panel_items_before = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should be added to the panel when panel is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_local_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_before = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal { local: true },
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal with local=true");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New local terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused_and_center_has_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after = terminal_panel.read_with(cx, |panel, cx| panel.terminal_count(cx));
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should go to panel when panel is focused, even if center has a terminal"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal when panel is focused"
        );
    }

    #[test]
    fn test_serialized_panel_formats_parse() {
        let no_splits: SerializedTerminalPanel =
            serde_json::from_str(r#"{"items":[1,2],"active_item_id":2}"#).unwrap();
        assert!(matches!(
            no_splits.items,
            SerializedItems::NoSplits(item_ids) if item_ids == vec![1, 2]
        ));

        let with_splits: SerializedTerminalPanel = serde_json::from_str(
            r#"{"items":{"Pane":{"active":true,"children":[1],"active_item":1}},"active_item_id":null}"#,
        )
        .unwrap();
        assert!(matches!(
            with_splits.items,
            SerializedItems::WithSplits(SerializedPaneGroup::Pane(_))
        ));

        let with_tabs = serde_json::to_string(&SerializedTerminalPanel {
            items: SerializedItems::WithTabs(SerializedTabs {
                tabs: vec![
                    serialized_pane(true, Some(1)),
                    serialized_pane(true, Some(2)),
                ],
                active_tab: 1,
            }),
            active_item_id: None,
        })
        .unwrap();
        let with_tabs: SerializedTerminalPanel = serde_json::from_str(&with_tabs).unwrap();
        assert!(matches!(
            with_tabs.items,
            SerializedItems::WithTabs(SerializedTabs { tabs, active_tab: 1 }) if tabs.len() == 2
        ));
    }

    #[gpui::test]
    async fn test_split_stays_in_its_tab(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(tab_layout(terminal_panel, cx), vec![vec![1], vec![1]]);
            assert_eq!(terminal_panel.active_tab, 1);
        });

        let first_tab_pane = terminal_panel.read_with(cx, |terminal_panel, _| {
            terminal_panel.tabs[0].active_pane.clone()
        });
        split(&window_handle, &terminal_panel, &first_tab_pane, cx).await;

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1, 1], vec![1]],
                "the split must only be added to the tab it was made in"
            );
            assert_eq!(
                terminal_panel.active_tab, 1,
                "splitting an inactive tab must not switch tabs"
            );
            assert_eq!(terminal_panel.tabs[0].active_pane, first_tab_pane);
        });
    }

    #[gpui::test]
    async fn test_new_terminal_tab_is_inserted_after_active_tab(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        for _ in 0..3 {
            add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        }
        let second_tab_pane = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.activate_tab(0, false, window, cx);
                    terminal_panel.tabs[1].active_pane.clone()
                })
            })
            .unwrap();

        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1], vec![1], vec![1], vec![1]]
            );
            assert_eq!(terminal_panel.active_tab, 1);
            assert_eq!(terminal_panel.tabs[2].active_pane, second_tab_pane);
        });
    }

    #[gpui::test]
    async fn test_never_revealed_task_does_not_switch_tabs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_task(echo_task(), RevealStrategy::Never, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(tab_layout(terminal_panel, cx), vec![vec![1], vec![1]]);
            assert_eq!(terminal_panel.active_tab, 0);
        });
    }

    #[gpui::test]
    async fn test_closing_splits_and_tabs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        let panel_closed = Rc::new(Cell::new(false));
        cx.update(|cx| {
            let panel_closed = panel_closed.clone();
            cx.subscribe(&terminal_panel, move |_, event: &PanelEvent, _| {
                if matches!(event, PanelEvent::Close) {
                    panel_closed.set(true);
                }
            })
            .detach();
        });
        for _ in 0..3 {
            add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        }
        let (first_tab_pane, second_tab_pane) =
            terminal_panel.read_with(cx, |terminal_panel, _| {
                (
                    terminal_panel.tabs[0].active_pane.clone(),
                    terminal_panel.tabs[1].active_pane.clone(),
                )
            });
        split(&window_handle, &terminal_panel, &first_tab_pane, cx).await;
        split(&window_handle, &terminal_panel, &second_tab_pane, cx).await;
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1, 1], vec![1, 1], vec![1]]
            );
            assert_eq!(terminal_panel.active_tab, 2);
        });

        close_all_items(&window_handle, &first_tab_pane, cx);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1], vec![1, 1], vec![1]],
                "closing a split must only remove that split"
            );
            assert_eq!(
                terminal_panel.active_tab, 2,
                "closing a split of an inactive tab must not switch tabs"
            );
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.close_tab(1, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1], vec![1]],
                "closing a tab must close all of its splits"
            );
            assert_eq!(terminal_panel.active_tab, 1);
        });

        for _ in 0..2 {
            window_handle
                .update(cx, |_, window, cx| {
                    terminal_panel.update(cx, |terminal_panel, cx| {
                        terminal_panel.close_tab(0, window, cx)
                    })
                })
                .unwrap();
            cx.run_until_parked();
        }
        assert!(
            panel_closed.get(),
            "closing the last tab must close the panel"
        );
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(tab_layout(terminal_panel, cx), vec![vec![0]]);
            assert_eq!(terminal_panel.active_tab, 0);
        });
    }

    #[gpui::test]
    async fn test_pane_item_actions_switch_tabs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        for _ in 0..3 {
            terminal_panel
                .update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
                .await
                .unwrap();
            cx.run_until_parked();
        }
        terminal_panel.read_with(cx, |terminal_panel, _| {
            assert_eq!(terminal_panel.active_tab, 2);
        });

        cx.dispatch_action(pane::ActivateNextItem::default());
        cx.run_until_parked();
        assert_active_tab_is_focused(&terminal_panel, 0, cx);

        cx.dispatch_action(pane::ActivateItem(1));
        cx.run_until_parked();
        assert_active_tab_is_focused(&terminal_panel, 1, cx);

        cx.dispatch_action(pane::ActivatePreviousItem::default());
        cx.run_until_parked();
        assert_active_tab_is_focused(&terminal_panel, 0, cx);

        cx.dispatch_action(pane::ActivateLastItem);
        cx.run_until_parked();
        assert_active_tab_is_focused(&terminal_panel, 2, cx);
    }

    #[gpui::test]
    async fn test_cycling_splits_shows_focused_cursor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        let first_pane =
            terminal_panel.read_with(cx, |terminal_panel, _| terminal_panel.active_pane().clone());
        let second_pane = terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.new_pane_with_terminal(&first_pane, true, window, cx)
            })
            .await
            .unwrap();
        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            terminal_panel.install_split(
                &first_pane,
                second_pane.clone(),
                SplitDirection::Right,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        for (focused_pane, unfocused_pane) in [
            (&first_pane, &second_pane),
            (&second_pane, &first_pane),
            (&first_pane, &second_pane),
        ] {
            cx.dispatch_action(ActivateNextPane);
            cx.run_until_parked();
            terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                assert_eq!(terminal_panel.active_pane(), focused_pane);
                let terminal_view = |pane: &Entity<Pane>| {
                    pane.read(cx)
                        .active_item()
                        .and_then(|item| item.downcast::<TerminalView>())
                        .expect("each split should hold a terminal")
                };
                let focused_view = terminal_view(focused_pane);
                let unfocused_view = terminal_view(unfocused_pane);
                assert!(focused_view.read(cx).focus_handle.is_focused(window));
                assert_eq!(
                    focused_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .cursor
                        .shape,
                    terminal::CursorShape::Block,
                    "the focused split must not keep its hollow cursor"
                );
                assert_eq!(
                    unfocused_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .cursor
                        .shape,
                    terminal::CursorShape::HollowBlock,
                );
            });
        }
    }

    #[gpui::test]
    async fn test_tabs_serialization_round_trip(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        add_shell(&window_handle, &terminal_panel, RevealStrategy::Never, cx).await;
        let first_tab_pane = terminal_panel.read_with(cx, |terminal_panel, _| {
            terminal_panel.tabs[0].active_pane.clone()
        });
        split(&window_handle, &terminal_panel, &first_tab_pane, cx).await;

        let serialized_tabs = cx.update(|cx| {
            let terminal_panel = terminal_panel.read(cx);
            serialize_tabs(
                terminal_panel
                    .tabs
                    .iter()
                    .map(|tab| (&tab.center, &tab.active_pane)),
                terminal_panel.active_tab,
                cx,
            )
        });
        assert_eq!(serialized_tabs.active_tab, 1);
        assert_eq!(serialized_tabs.tabs.len(), 2);
        assert!(matches!(
            &serialized_tabs.tabs[0],
            SerializedPaneGroup::Group { children, .. } if children.len() == 2
        ));

        let (window_handle, restored_panel) = init_panel(cx).await;
        let restored_items = restore(
            &window_handle,
            &restored_panel,
            SerializedTerminalPanel {
                items: SerializedItems::WithTabs(serialized_tabs),
                active_item_id: None,
            },
            cx,
        )
        .await;

        assert_eq!(restored_items, 3);
        restored_panel.read_with(cx, |restored_panel, cx| {
            assert_eq!(tab_layout(restored_panel, cx), vec![vec![1, 1], vec![1]]);
            assert_eq!(restored_panel.active_tab, 1);
            let split_tab = &restored_panel.tabs[0];
            assert_eq!(split_tab.active_pane, split_tab.center.first_pane());
        });
    }

    #[gpui::test]
    async fn test_legacy_splits_restore_as_one_tab_per_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_panel(cx).await;
        let restored_items = restore(
            &window_handle,
            &terminal_panel,
            SerializedTerminalPanel {
                items: SerializedItems::WithSplits(SerializedPaneGroup::Group {
                    axis: SerializedAxis(gpui::Axis::Horizontal),
                    flexes: None,
                    children: vec![
                        SerializedPaneGroup::Pane(SerializedPane {
                            active: false,
                            children: vec![1, 2],
                            active_item: Some(1),
                            pinned_count: 1,
                        }),
                        SerializedPaneGroup::Pane(SerializedPane {
                            active: true,
                            children: vec![3, 4],
                            active_item: Some(4),
                            pinned_count: 0,
                        }),
                    ],
                }),
                active_item_id: None,
            },
            cx,
        )
        .await;

        assert_eq!(restored_items, 4);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                tab_layout(terminal_panel, cx),
                vec![vec![1], vec![1], vec![1], vec![1]]
            );
            assert_eq!(
                terminal_panel.active_tab, 3,
                "the previously active terminal must stay active"
            );
            assert!(
                terminal_panel
                    .all_panes()
                    .all(|pane| pane.read(cx).pinned_count() == 0)
            );
        });
    }

    fn assert_active_tab_is_focused(
        terminal_panel: &Entity<TerminalPanel>,
        expected_tab: usize,
        cx: &mut VisualTestContext,
    ) {
        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            assert_eq!(terminal_panel.active_tab, expected_tab);
            let terminal_view = terminal_panel
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("active tab should hold a terminal");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "the active tab's terminal should be focused"
            );
        });
    }

    async fn init_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<TerminalPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();
        (window_handle, terminal_panel)
    }

    async fn add_shell(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        terminal_panel: &Entity<TerminalPanel>,
        reveal_strategy: RevealStrategy,
        cx: &mut TestAppContext,
    ) {
        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(false, None, reveal_strategy, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
    }

    async fn split(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        terminal_panel: &Entity<TerminalPanel>,
        source_pane: &Entity<Pane>,
        cx: &mut TestAppContext,
    ) {
        let new_pane = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.new_pane_with_terminal(source_pane, true, window, cx)
                })
            })
            .unwrap()
            .await
            .expect("failed to create a terminal for the split");
        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.install_split(
                        source_pane,
                        new_pane,
                        SplitDirection::Right,
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        cx.run_until_parked();
    }

    fn close_all_items(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        pane: &Entity<Pane>,
        cx: &mut TestAppContext,
    ) {
        window_handle
            .update(cx, |_, window, cx| {
                pane.update(cx, |pane, cx| {
                    pane.close_all_items(&CloseAllItems::default(), window, cx)
                })
            })
            .unwrap()
            .detach();
        cx.run_until_parked();
    }

    async fn restore(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        terminal_panel: &Entity<TerminalPanel>,
        serialized_panel: SerializedTerminalPanel,
        cx: &mut TestAppContext,
    ) -> usize {
        let restored_items = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let project = workspace.read(cx).project().clone();
                deserialize_terminal_panel(
                    workspace.downgrade(),
                    project,
                    WorkspaceId::default(),
                    serialized_panel,
                    terminal_panel.downgrade(),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
        restored_items
    }

    fn tab_layout(terminal_panel: &TerminalPanel, cx: &App) -> Vec<Vec<usize>> {
        terminal_panel
            .tabs
            .iter()
            .map(|tab| {
                tab.center
                    .panes()
                    .into_iter()
                    .map(|pane| pane.read(cx).items_len())
                    .collect()
            })
            .collect()
    }

    fn serialized_pane(active: bool, item_id: Option<u64>) -> SerializedPaneGroup {
        SerializedPaneGroup::Pane(SerializedPane {
            active,
            children: item_id.into_iter().collect(),
            active_item: item_id,
            pinned_count: 0,
        })
    }

    fn set_max_tabs(cx: &mut TestAppContext, value: Option<usize>) {
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.workspace.max_tabs = value.map(|v| NonZero::new(v).unwrap())
            });
        });
    }

    pub fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }
}
