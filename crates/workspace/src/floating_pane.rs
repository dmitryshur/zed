use collections::HashMap;
use gpui::{
    Action, Axis, Bounds, CursorStyle, DragMoveEvent, Entity, EntityId, FocusHandle, Focusable,
    MouseButton, Pixels, Point, Size, Subscription, Task, WeakFocusHandle, point, size,
};
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt;

use crate::{CloseAllItems, ItemHandle, Pane, PaneSearchBarCallbacks, item::ItemEvent, pane};

const MAX_FLOATING_PANES: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FloatingPaneLayout {
    #[default]
    Stacked,
    Tiled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FloatingPaneGeometry {
    desired_bounds: Option<Bounds<Pixels>>,
    restore_bounds: Option<Bounds<Pixels>>,
}

struct FloatingPane {
    pane: Entity<Pane>,
    stacked: FloatingPaneGeometry,
    tiled: FloatingPaneGeometry,
    new_action: Box<dyn Action>,
    _creation: Task<()>,
    _subscriptions: Vec<Subscription>,
    item_subscriptions: HashMap<EntityId, Subscription>,
}

pub struct FloatingPaneLayer {
    layout: FloatingPaneLayout,
    panes: Vec<FloatingPane>,
    stacking_order: Vec<EntityId>,
    active_pane: Option<EntityId>,
    visible: bool,
    viewport: Size<Pixels>,
    previous_focus: Option<WeakFocusHandle>,
    fallback_focus: WeakFocusHandle,
}

#[derive(Clone, Copy, Debug)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone)]
struct FloatingPaneDrag {
    layer_id: EntityId,
    layout: FloatingPaneLayout,
    pane_id: EntityId,
    bounds: Bounds<Pixels>,
    mouse_position: Point<Pixels>,
    edge: Option<Edge>,
}

struct DragPreview;

impl FloatingPane {
    fn geometry(&self, layout: FloatingPaneLayout) -> &FloatingPaneGeometry {
        match layout {
            FloatingPaneLayout::Stacked => &self.stacked,
            FloatingPaneLayout::Tiled => &self.tiled,
        }
    }

    fn geometry_mut(&mut self, layout: FloatingPaneLayout) -> &mut FloatingPaneGeometry {
        match layout {
            FloatingPaneLayout::Stacked => &mut self.stacked,
            FloatingPaneLayout::Tiled => &mut self.tiled,
        }
    }

    fn bounds(
        &self,
        layout: FloatingPaneLayout,
        viewport: Size<Pixels>,
        count: usize,
        index: usize,
    ) -> Bounds<Pixels> {
        let geometry = self.geometry(layout);
        if geometry.restore_bounds.is_some() {
            enlarged_bounds(viewport)
        } else {
            let default = match layout {
                FloatingPaneLayout::Stacked => default_bounds(viewport, index),
                FloatingPaneLayout::Tiled => tiled_bounds(viewport, count, index),
            };
            clamp_bounds_with_minimum(
                geometry.desired_bounds.unwrap_or(default),
                viewport,
                layout_minimum_size(layout, viewport, count),
            )
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl FloatingPaneLayer {
    pub(crate) fn new(fallback_focus: FocusHandle) -> Self {
        Self {
            layout: FloatingPaneLayout::Stacked,
            panes: Vec::new(),
            stacking_order: Vec::new(),
            active_pane: None,
            visible: false,
            viewport: Size::default(),
            previous_focus: None,
            fallback_focus: fallback_focus.downgrade(),
        }
    }

    pub fn has_panes(&self) -> bool {
        !self.panes.is_empty()
    }

    pub fn can_add_pane(&self) -> bool {
        self.panes.len() < MAX_FLOATING_PANES
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn active_pane(&self) -> Option<Entity<Pane>> {
        self.active_pane.and_then(|id| self.pane(id))
    }

    pub fn pane(&self, id: EntityId) -> Option<Entity<Pane>> {
        self.panes
            .iter()
            .find(|entry| entry.pane.entity_id() == id)
            .map(|entry| entry.pane.clone())
    }

    pub fn focused_pane(&self, window: &Window, cx: &App) -> Option<Entity<Pane>> {
        if !self.visible {
            return None;
        }
        self.panes
            .iter()
            .find(|entry| entry.pane.read(cx).has_focus(window, cx))
            .map(|entry| entry.pane.clone())
    }

    pub(crate) fn set_fallback_focus(&mut self, focus: FocusHandle) {
        self.fallback_focus = focus.downgrade();
    }

    pub(crate) fn set_viewport(&mut self, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        self.viewport = viewport;
        for (index, entry) in self.panes.iter_mut().enumerate() {
            if entry.stacked.desired_bounds.is_none()
                && viewport.width > px(0.)
                && viewport.height > px(0.)
            {
                entry.stacked.desired_bounds = Some(default_bounds(viewport, index));
            }
        }
        // Bounds arrive during prepaint, when notify alone cannot request another frame.
        let layer_id = cx.entity_id();
        cx.defer(move |cx| cx.notify(layer_id));
    }

    pub fn add_pane(
        &mut self,
        pane: Entity<Pane>,
        new_action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.can_add_pane() {
            return false;
        }
        let pane_id = pane.entity_id();
        let subscriptions = vec![
            cx.subscribe_in(&pane, window, |this, pane, event, window, cx| match event {
                pane::Event::Focus => this.raise(pane.entity_id(), cx),
                pane::Event::Remove { .. } => this.remove_pane(pane.entity_id(), window, cx),
                pane::Event::AddItem { item } => {
                    this.subscribe_item(pane.entity_id(), item.as_ref(), window, cx)
                }
                pane::Event::RemovedItem { item } => {
                    if let Some(entry) = this.panes.iter_mut().find(|entry| entry.pane == *pane) {
                        entry.item_subscriptions.remove(&item.item_id());
                    }
                    cx.notify();
                }
                _ => cx.notify(),
            }),
            cx.observe(&pane, |_, _, cx| cx.notify()),
        ];
        let desired_bounds = (self.viewport.width > px(0.) && self.viewport.height > px(0.))
            .then(|| default_bounds(self.viewport, self.panes.len()));
        self.panes.push(FloatingPane {
            pane: pane.clone(),
            stacked: FloatingPaneGeometry {
                desired_bounds,
                restore_bounds: None,
            },
            tiled: FloatingPaneGeometry::default(),
            new_action,
            _creation: Task::ready(()),
            _subscriptions: subscriptions,
            item_subscriptions: HashMap::default(),
        });
        self.reset_tiled_geometry();
        for item in pane
            .read(cx)
            .items()
            .map(|item| item.boxed_clone())
            .collect::<Vec<_>>()
        {
            self.subscribe_item(pane_id, item.as_ref(), window, cx);
        }
        self.stacking_order.push(pane_id);
        self.active_pane = Some(pane_id);
        self.show(window, cx);
        true
    }

    fn subscribe_item(
        &mut self,
        pane_id: EntityId,
        item: &dyn ItemHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane(pane_id) else {
            return;
        };
        let pane = pane.downgrade();
        let layer = cx.weak_entity();
        let item_id = item.item_id();
        // Floating sessions intentionally bypass workspace item registration and persistence,
        // but still need the item events that normally arrive through that registration.
        let subscription = item.subscribe_to_item_events(
            window,
            cx,
            Box::new(move |event, window, cx| {
                let Some(pane) = pane.upgrade() else {
                    return;
                };
                match event {
                    ItemEvent::CloseItem => pane
                        .update(cx, |pane, cx| {
                            pane.close_item_by_id(item_id, crate::SaveIntent::Close, window, cx)
                        })
                        .detach_and_log_err(cx),
                    ItemEvent::UpdateBreadcrumbs => {
                        let active_item = pane.read(cx).active_item();
                        let toolbar = pane.read(cx).toolbar().clone();
                        toolbar.update(cx, |toolbar, cx| {
                            toolbar.set_active_item(active_item.as_deref(), window, cx)
                        });
                    }
                    ItemEvent::UpdateTab | ItemEvent::Edit => {}
                }
                layer.update(cx, |_, cx| cx.notify()).log_err();
            }),
        );
        if let Some(entry) = self
            .panes
            .iter_mut()
            .find(|entry| entry.pane.entity_id() == pane_id)
        {
            entry.item_subscriptions.insert(item_id, subscription);
        }
    }

    pub fn set_creation_task(&mut self, pane_id: EntityId, task: Task<()>) {
        if let Some(entry) = self
            .panes
            .iter_mut()
            .find(|entry| entry.pane.entity_id() == pane_id)
        {
            entry._creation = task;
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_panes() {
            return;
        }
        if self.focused_pane(window, cx).is_none() {
            self.previous_focus = window.focused(cx).map(|focus| focus.downgrade());
        }
        self.visible = true;
        if let Some(pane) = self.active_pane() {
            self.focus_pane(pane.entity_id(), window, cx);
        }
        cx.notify();
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self.focused_pane(window, cx).is_some();
        self.visible = false;
        if had_focus {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self
            .previous_focus
            .as_ref()
            .and_then(WeakFocusHandle::upgrade)
            .or_else(|| self.fallback_focus.upgrade())
        {
            window.focus(&focus, cx);
        }
    }

    pub fn activate_relative(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible || self.panes.is_empty() {
            return;
        }
        let current = self
            .panes
            .iter()
            .position(|entry| Some(entry.pane.entity_id()) == self.active_pane)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % self.panes.len()
        } else {
            (current + self.panes.len() - 1) % self.panes.len()
        };
        if let Some(entry) = self.panes.get(next) {
            self.focus_pane(entry.pane.entity_id(), window, cx);
        }
    }

    pub fn resize_active(&mut self, axis: Axis, amount: Pixels, cx: &mut Context<Self>) {
        if !self.visible || self.viewport.width <= px(0.) || self.viewport.height <= px(0.) {
            return;
        }
        let count = self.panes.len();
        let Some((index, entry)) = self
            .panes
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| Some(entry.pane.entity_id()) == self.active_pane)
        else {
            return;
        };
        if entry.geometry(self.layout).restore_bounds.is_some() {
            return;
        }
        let bounds = entry.bounds(self.layout, self.viewport, count, index);
        let resized = resized_bounds(
            bounds,
            axis,
            amount,
            self.viewport,
            layout_minimum_size(self.layout, self.viewport, count),
        );
        if resized != bounds {
            entry.geometry_mut(self.layout).desired_bounds = Some(resized);
            cx.notify();
        }
    }

    pub fn move_active(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.visible || self.viewport.width <= px(0.) || self.viewport.height <= px(0.) {
            return;
        }
        let count = self.panes.len();
        let Some((index, entry)) = self
            .panes
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| Some(entry.pane.entity_id()) == self.active_pane)
        else {
            return;
        };
        if entry.geometry(self.layout).restore_bounds.is_some() {
            return;
        }
        let bounds = entry.bounds(self.layout, self.viewport, count, index);
        let moved = dragged_bounds(
            bounds,
            delta,
            None,
            self.viewport,
            layout_minimum_size(self.layout, self.viewport, count),
        );
        if moved != bounds {
            entry.geometry_mut(self.layout).desired_bounds = Some(moved);
            cx.notify();
        }
    }

    pub fn reset_positions(&mut self, cx: &mut Context<Self>) {
        if self.viewport.width <= px(0.) || self.viewport.height <= px(0.) {
            return;
        }
        if self.layout == FloatingPaneLayout::Tiled {
            self.reset_tiled_geometry();
            cx.notify();
            return;
        }
        for (index, pane_id) in self.stacking_order.iter().enumerate() {
            if let Some(entry) = self
                .panes
                .iter_mut()
                .find(|entry| entry.pane.entity_id() == *pane_id)
            {
                entry.stacked.restore_bounds = None;
                entry.stacked.desired_bounds = Some(default_bounds(self.viewport, index));
            }
        }
        cx.notify();
    }

    pub fn toggle_layout(&mut self, cx: &mut Context<Self>) {
        self.layout = match self.layout {
            FloatingPaneLayout::Stacked => FloatingPaneLayout::Tiled,
            FloatingPaneLayout::Tiled => FloatingPaneLayout::Stacked,
        };
        cx.notify();
    }

    fn reset_tiled_geometry(&mut self) {
        for entry in &mut self.panes {
            entry.tiled = FloatingPaneGeometry::default();
        }
    }

    pub fn toggle_maximize(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.viewport.width <= px(0.) || self.viewport.height <= px(0.) {
            return;
        }
        let Some(pane) = self.focused_pane(window, cx) else {
            return;
        };
        let count = self.panes.len();
        if let Some((index, entry)) = self
            .panes
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.pane == pane)
        {
            let bounds = entry.bounds(self.layout, self.viewport, count, index);
            let geometry = entry.geometry_mut(self.layout);
            if let Some(bounds) = geometry.restore_bounds.take() {
                if geometry.desired_bounds.is_some() {
                    geometry.desired_bounds = Some(bounds);
                }
            } else {
                geometry.restore_bounds = Some(bounds);
            }
            cx.notify();
        }
    }

    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible
            && let Some(id) = self.active_pane
        {
            self.close_pane(id, window, cx);
        }
    }

    fn close_pane(&mut self, pane_id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane(pane_id) {
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

    fn remove_pane(&mut self, pane_id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self
            .pane(pane_id)
            .is_some_and(|pane| pane.focus_handle(cx).contains_focused(window, cx));
        self.panes.retain(|entry| entry.pane.entity_id() != pane_id);
        self.reset_tiled_geometry();
        self.stacking_order.retain(|id| *id != pane_id);
        if self.active_pane == Some(pane_id) {
            self.active_pane = self.stacking_order.last().copied();
        }
        if self.panes.is_empty() {
            self.visible = false;
            if had_focus {
                self.restore_focus(window, cx);
            }
        } else if had_focus && let Some(id) = self.active_pane {
            self.focus_pane(id, window, cx);
        }
        cx.notify();
    }

    fn raise(&mut self, pane_id: EntityId, cx: &mut Context<Self>) {
        if self.pane(pane_id).is_none() {
            return;
        }
        self.stacking_order.retain(|id| *id != pane_id);
        self.stacking_order.push(pane_id);
        self.active_pane = Some(pane_id);
        cx.notify();
    }

    fn focus_pane(&mut self, pane_id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        self.raise(pane_id, cx);
        if let Some(pane) = self.pane(pane_id) {
            window.focus(&pane.focus_handle(cx), cx);
        }
    }

    fn render_handle(drag: FloatingPaneDrag, cx: &mut Context<Self>) -> AnyElement {
        let edge = drag.edge;
        div()
            .id(format!("resize-{edge:?}"))
            .absolute()
            .occlude()
            .map(|handle| match edge {
                Some(Edge::Left) => handle
                    .left_0()
                    .top(px(8.))
                    .bottom(px(8.))
                    .w(px(6.))
                    .cursor(CursorStyle::ResizeColumn),
                Some(Edge::Right) => handle
                    .right_0()
                    .top(px(8.))
                    .bottom(px(8.))
                    .w(px(6.))
                    .cursor(CursorStyle::ResizeColumn),
                Some(Edge::Top) => handle
                    .top_0()
                    .left(px(8.))
                    .right(px(8.))
                    .h(px(6.))
                    .cursor(CursorStyle::ResizeRow),
                Some(Edge::Bottom) => handle
                    .bottom_0()
                    .left(px(8.))
                    .right(px(8.))
                    .h(px(6.))
                    .cursor(CursorStyle::ResizeRow),
                Some(Edge::TopLeft) => handle
                    .top_0()
                    .left_0()
                    .size(px(8.))
                    .cursor(CursorStyle::ResizeUpLeftDownRight),
                Some(Edge::TopRight) => handle
                    .top_0()
                    .right_0()
                    .size(px(8.))
                    .cursor(CursorStyle::ResizeUpRightDownLeft),
                Some(Edge::BottomLeft) => handle
                    .bottom_0()
                    .left_0()
                    .size(px(8.))
                    .cursor(CursorStyle::ResizeUpRightDownLeft),
                Some(Edge::BottomRight) => handle
                    .bottom_0()
                    .right_0()
                    .size(px(8.))
                    .cursor(CursorStyle::ResizeUpLeftDownRight),
                None => handle,
            })
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_drag(drag, |_, _, _, cx| cx.new(|_| DragPreview))
            .on_drag_move::<FloatingPaneDrag>(cx.listener(Self::drag_move))
            .into_any_element()
    }

    fn drag_move(
        &mut self,
        event: &DragMoveEvent<FloatingPaneDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        if drag.layer_id != cx.entity_id() || drag.layout != self.layout {
            return;
        }
        let bounds = dragged_bounds(
            drag.bounds,
            event.event.position - drag.mouse_position,
            drag.edge,
            self.viewport,
            layout_minimum_size(self.layout, self.viewport, self.panes.len()),
        );
        if let Some(entry) = self
            .panes
            .iter_mut()
            .find(|entry| entry.pane.entity_id() == drag.pane_id)
        {
            let geometry = entry.geometry_mut(self.layout);
            if geometry.restore_bounds.is_none() {
                geometry.desired_bounds = Some(bounds);
                cx.notify();
            }
        }
        cx.stop_propagation();
    }
}

impl Render for FloatingPaneLayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .absolute()
            .inset_0()
            .key_context("FloatingTerminal")
            .on_action(
                cx.listener(|this, _: &crate::ActivateNextPane, window, cx| {
                    this.activate_relative(true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::ActivatePreviousPane, window, cx| {
                    this.activate_relative(false, window, cx);
                }),
            )
            .on_drag_move::<FloatingPaneDrag>(cx.listener(Self::drag_move))
            .capture_action(|_: &pane::TogglePinTab, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MovePaneLeft, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MovePaneRight, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MovePaneUp, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MovePaneDown, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::SwapPaneLeft, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::SwapPaneRight, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::SwapPaneUp, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::SwapPaneDown, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::SwapPaneAdjacent, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MoveItemToPane, _, cx| cx.stop_propagation())
            .capture_action(|_: &crate::MoveItemToPaneInDirection, _, cx| cx.stop_propagation());
        root = root.capture_action(
            cx.listener(|this, action: &crate::NewTerminal, window, cx| {
                if action.local {
                    cx.propagate();
                    return;
                }
                if let Some(entry) = this
                    .panes
                    .iter()
                    .find(|entry| Some(entry.pane.entity_id()) == this.active_pane)
                {
                    window.dispatch_action(entry.new_action.boxed_clone(), cx);
                }
                cx.stop_propagation();
            }),
        );
        if !self.visible {
            return div().into_any_element();
        }
        for pane_id in &self.stacking_order {
            let Some((index, entry)) = self
                .panes
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.pane.entity_id() == *pane_id)
            else {
                continue;
            };
            let pane_id = *pane_id;
            let bounds = entry.bounds(self.layout, self.viewport, self.panes.len(), index);
            let maximized = entry.geometry(self.layout).restore_bounds.is_some();
            let drag = FloatingPaneDrag {
                layer_id: cx.entity_id(),
                layout: self.layout,
                pane_id,
                bounds,
                mouse_position: window.mouse_position(),
                edge: None,
            };
            let item = entry.pane.read(cx).active_item();
            let title = item.map(|item| {
                item.tab_content(
                    crate::item::TabContentParams {
                        selected: self.active_pane == Some(pane_id),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            let new_action = entry.new_action.boxed_clone();
            let body = cx
                .try_global::<PaneSearchBarCallbacks>()
                .map(|callbacks| {
                    (callbacks.wrap_div_with_search_actions)(div(), entry.pane.clone())
                })
                .unwrap_or_else(div);
            root = root.child(
                v_flex()
                    .id(("floating-pane", pane_id))
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y)
                    .w(bounds.size.width)
                    .h(bounds.size.height)
                    .occlude()
                    .bg(cx.theme().colors().panel_background)
                    .border_1()
                    .border_color(if self.active_pane == Some(pane_id) {
                        cx.theme().colors().border_focused
                    } else {
                        cx.theme().colors().border
                    })
                    .rounded_md()
                    .shadow_lg()
                    .capture_any_mouse_down(
                        cx.listener(move |this, _, window, cx| {
                            this.focus_pane(pane_id, window, cx)
                        }),
                    )
                    .child(
                        h_flex()
                            .id("title-bar")
                            .h(px(32.))
                            .flex_none()
                            .px_2()
                            .gap_2()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.focus_pane(pane_id, window, cx)
                                }),
                            )
                            .when(!maximized, |title_bar| {
                                title_bar
                                    .cursor(CursorStyle::PointingHand)
                                    .on_drag(drag.clone(), |_, _, _, cx| cx.new(|_| DragPreview))
                                    .on_drag_move::<FloatingPaneDrag>(cx.listener(Self::drag_move))
                            })
                            .child(div().flex_1().overflow_hidden().children(title))
                            .child(
                                IconButton::new("new-floating-terminal", IconName::Plus)
                                    .disabled(!self.can_add_pane())
                                    .tooltip(Tooltip::text("New Floating Terminal"))
                                    .on_click(move |_, window, cx| {
                                        window.dispatch_action(new_action.boxed_clone(), cx)
                                    }),
                            )
                            .child(
                                IconButton::new("hide-floating-terminals", IconName::Minimize)
                                    .tooltip(Tooltip::text("Hide Floating Terminals"))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.hide(window, cx)),
                                    ),
                            )
                            .child(
                                IconButton::new("close-floating-terminal", IconName::Close)
                                    .tooltip(Tooltip::text("Close Floating Terminal"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_pane(pane_id, window, cx)
                                    })),
                            ),
                    )
                    .child(
                        body.flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(entry.pane.clone()),
                    )
                    .when(!maximized, |pane| {
                        pane.children(
                            [
                                Edge::Left,
                                Edge::Right,
                                Edge::Top,
                                Edge::Bottom,
                                Edge::TopLeft,
                                Edge::TopRight,
                                Edge::BottomLeft,
                                Edge::BottomRight,
                            ]
                            .into_iter()
                            .map(|edge| {
                                Self::render_handle(
                                    FloatingPaneDrag {
                                        edge: Some(edge),
                                        ..drag.clone()
                                    },
                                    cx,
                                )
                            }),
                        )
                    }),
            );
        }
        root.into_any_element()
    }
}

fn default_bounds(viewport: Size<Pixels>, index: usize) -> Bounds<Pixels> {
    let dimensions = size(viewport.width * 0.7, viewport.height * 0.6);
    let offset = px(index as f32 * 24.);
    clamp_bounds(
        Bounds::new(
            point(
                (viewport.width - dimensions.width) / 2. + offset,
                (viewport.height - dimensions.height) / 2. + offset,
            ),
            dimensions,
        ),
        viewport,
    )
}

fn enlarged_bounds(viewport: Size<Pixels>) -> Bounds<Pixels> {
    let dimensions = size(viewport.width * 0.8, viewport.height * 0.8);
    clamp_bounds(
        Bounds::new(
            point(
                (viewport.width - dimensions.width) / 2.,
                (viewport.height - dimensions.height) / 2.,
            ),
            dimensions,
        ),
        viewport,
    )
}

fn tiled_bounds(viewport: Size<Pixels>, count: usize, index: usize) -> Bounds<Pixels> {
    let count = count.max(1);
    let columns = if count == 4 { 2 } else { count };
    let rows = if count == 4 { 2 } else { 1 };
    let region = size(viewport.width * 0.9, viewport.height * 0.9);
    let dimensions = size(region.width / columns as f32, region.height / rows as f32);
    Bounds::new(
        point(
            (viewport.width - region.width) / 2. + dimensions.width * (index % columns) as f32,
            (viewport.height - region.height) / 2. + dimensions.height * (index / columns) as f32,
        ),
        dimensions,
    )
}

fn layout_minimum_size(
    layout: FloatingPaneLayout,
    viewport: Size<Pixels>,
    count: usize,
) -> Size<Pixels> {
    let minimum = size(px(320.), px(200.));
    match layout {
        FloatingPaneLayout::Stacked => minimum,
        FloatingPaneLayout::Tiled => {
            let tile = tiled_bounds(viewport, count, 0);
            size(
                minimum.width.min(tile.size.width),
                minimum.height.min(tile.size.height),
            )
        }
    }
}

fn clamp_bounds(bounds: Bounds<Pixels>, viewport: Size<Pixels>) -> Bounds<Pixels> {
    clamp_bounds_with_minimum(bounds, viewport, size(px(320.), px(200.)))
}

fn clamp_bounds_with_minimum(
    mut bounds: Bounds<Pixels>,
    viewport: Size<Pixels>,
    minimum: Size<Pixels>,
) -> Bounds<Pixels> {
    bounds.size.width = bounds.size.width.max(minimum.width).min(viewport.width);
    bounds.size.height = bounds.size.height.max(minimum.height).min(viewport.height);
    bounds.origin.x = bounds
        .origin
        .x
        .max(px(0.))
        .min((viewport.width - bounds.size.width).max(px(0.)));
    bounds.origin.y = bounds
        .origin
        .y
        .max(px(0.))
        .min((viewport.height - bounds.size.height).max(px(0.)));
    bounds
}

fn resized_bounds(
    bounds: Bounds<Pixels>,
    axis: Axis,
    amount: Pixels,
    viewport: Size<Pixels>,
    minimum: Size<Pixels>,
) -> Bounds<Pixels> {
    let bounds = clamp_bounds_with_minimum(bounds, viewport, minimum);
    let mut resized = bounds;
    match axis {
        Axis::Horizontal => resized.size.width += amount,
        Axis::Vertical => resized.size.height += amount,
    }
    resized = clamp_bounds_with_minimum(resized, viewport, minimum);
    if resized.size == bounds.size {
        return bounds;
    }
    resized.origin = bounds.origin
        + point(
            (bounds.size.width - resized.size.width) / 2.,
            (bounds.size.height - resized.size.height) / 2.,
        );
    clamp_bounds_with_minimum(resized, viewport, minimum)
}

fn dragged_bounds(
    bounds: Bounds<Pixels>,
    delta: Point<Pixels>,
    edge: Option<Edge>,
    viewport: Size<Pixels>,
    minimum: Size<Pixels>,
) -> Bounds<Pixels> {
    let Some(edge) = edge else {
        return clamp_bounds_with_minimum(
            Bounds::new(bounds.origin + delta, bounds.size),
            viewport,
            minimum,
        );
    };
    let minimum_width = minimum.width.min(viewport.width);
    let minimum_height = minimum.height.min(viewport.height);
    let mut left = bounds.left();
    let mut right = bounds.right();
    let mut top = bounds.top();
    let mut bottom = bounds.bottom();
    if matches!(edge, Edge::Left | Edge::TopLeft | Edge::BottomLeft) {
        left = (left + delta.x).max(px(0.)).min(right - minimum_width);
    }
    if matches!(edge, Edge::Right | Edge::TopRight | Edge::BottomRight) {
        right = (right + delta.x)
            .max(left + minimum_width)
            .min(viewport.width);
    }
    if matches!(edge, Edge::Top | Edge::TopLeft | Edge::TopRight) {
        top = (top + delta.y).max(px(0.)).min(bottom - minimum_height);
    }
    if matches!(edge, Edge::Bottom | Edge::BottomLeft | Edge::BottomRight) {
        bottom = (bottom + delta.y)
            .max(top + minimum_height)
            .min(viewport.height);
    }
    Bounds::new(point(left, top), size(right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Workspace, item::ItemEvent};
    use gpui::{EventEmitter, Modifiers, TestAppContext};
    use project::Project;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn floating_pane_geometry_resizes_all_edges_and_clamps() {
        let viewport = size(px(1000.), px(800.));
        let original = Bounds::new(point(px(200.), px(200.)), size(px(500.), px(400.)));
        for (edge, expected) in [
            (
                Edge::Left,
                Bounds::new(point(px(220.), px(200.)), size(px(480.), px(400.))),
            ),
            (
                Edge::Right,
                Bounds::new(point(px(200.), px(200.)), size(px(520.), px(400.))),
            ),
            (
                Edge::Top,
                Bounds::new(point(px(200.), px(230.)), size(px(500.), px(370.))),
            ),
            (
                Edge::Bottom,
                Bounds::new(point(px(200.), px(200.)), size(px(500.), px(430.))),
            ),
            (
                Edge::TopLeft,
                Bounds::new(point(px(220.), px(230.)), size(px(480.), px(370.))),
            ),
            (
                Edge::TopRight,
                Bounds::new(point(px(200.), px(230.)), size(px(520.), px(370.))),
            ),
            (
                Edge::BottomLeft,
                Bounds::new(point(px(220.), px(200.)), size(px(480.), px(430.))),
            ),
            (
                Edge::BottomRight,
                Bounds::new(point(px(200.), px(200.)), size(px(520.), px(430.))),
            ),
        ] {
            assert_eq!(
                dragged_bounds(
                    original,
                    point(px(20.), px(30.)),
                    Some(edge),
                    viewport,
                    size(px(320.), px(200.))
                ),
                expected
            );
            for delta in [
                point(px(-10000.), px(-10000.)),
                point(px(10000.), px(10000.)),
            ] {
                let resized = dragged_bounds(
                    original,
                    delta,
                    Some(edge),
                    viewport,
                    size(px(320.), px(200.)),
                );
                assert!(resized.left() >= px(0.) && resized.top() >= px(0.));
                assert!(resized.right() <= viewport.width && resized.bottom() <= viewport.height);
                assert!(resized.size.width >= px(320.) && resized.size.height >= px(200.));
            }
        }
        let moved = dragged_bounds(
            original,
            point(px(10000.), px(-10000.)),
            None,
            viewport,
            size(px(320.), px(200.)),
        );
        assert_eq!(moved.origin, point(px(500.), px(0.)));
        assert_eq!(moved.size, original.size);
        let small = size(px(100.), px(80.));
        assert_eq!(
            clamp_bounds(original, small),
            Bounds::new(Point::default(), small)
        );
        assert_eq!(clamp_bounds(original, viewport), original);
        for (index, expected) in [
            (0, point(px(150.), px(160.))),
            (1, point(px(174.), px(184.))),
        ] {
            let origin = default_bounds(viewport, index).origin;
            assert!((origin.x - expected.x).abs() < px(0.01));
            assert!((origin.y - expected.y).abs() < px(0.01));
        }
    }

    struct TestTerminal {
        focus_handle: FocusHandle,
    }

    #[test]
    fn floating_pane_keyboard_resize_keeps_center_and_respects_boundaries() {
        let viewport = size(px(1000.), px(800.));
        let original = Bounds::new(point(px(200.), px(200.)), size(px(500.), px(400.)));
        for (axis, amount, expected) in [
            (
                Axis::Horizontal,
                px(20.),
                Bounds::new(point(px(190.), px(200.)), size(px(520.), px(400.))),
            ),
            (
                Axis::Horizontal,
                px(-20.),
                Bounds::new(point(px(210.), px(200.)), size(px(480.), px(400.))),
            ),
            (
                Axis::Vertical,
                px(20.),
                Bounds::new(point(px(200.), px(190.)), size(px(500.), px(420.))),
            ),
            (
                Axis::Vertical,
                px(-20.),
                Bounds::new(point(px(200.), px(210.)), size(px(500.), px(380.))),
            ),
        ] {
            let resized =
                resized_bounds(original, axis, amount, viewport, size(px(320.), px(200.)));
            assert_eq!(resized, expected);
            assert_eq!(resized.center(), original.center());
        }
        for (origin, axis, expected_origin) in [
            (
                point(px(0.), px(200.)),
                Axis::Horizontal,
                point(px(0.), px(200.)),
            ),
            (
                point(px(500.), px(200.)),
                Axis::Horizontal,
                point(px(480.), px(200.)),
            ),
            (
                point(px(200.), px(0.)),
                Axis::Vertical,
                point(px(200.), px(0.)),
            ),
            (
                point(px(200.), px(400.)),
                Axis::Vertical,
                point(px(200.), px(380.)),
            ),
        ] {
            let resized = resized_bounds(
                Bounds::new(origin, original.size),
                axis,
                px(20.),
                viewport,
                size(px(320.), px(200.)),
            );
            assert_eq!(resized.origin, expected_origin);
            assert!(resized.left() >= px(0.) && resized.top() >= px(0.));
            assert!(resized.right() <= viewport.width && resized.bottom() <= viewport.height);
        }
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let minimum = resized_bounds(
                original,
                axis,
                px(-10000.),
                viewport,
                size(px(320.), px(200.)),
            );
            let maximum = resized_bounds(
                original,
                axis,
                px(10000.),
                viewport,
                size(px(320.), px(200.)),
            );
            match axis {
                Axis::Horizontal => {
                    assert_eq!(minimum.size, size(px(320.), original.size.height));
                    assert_eq!(maximum.size, size(viewport.width, original.size.height));
                }
                Axis::Vertical => {
                    assert_eq!(minimum.size, size(original.size.width, px(200.)));
                    assert_eq!(maximum.size, size(original.size.width, viewport.height));
                }
            }
            assert_eq!(
                resized_bounds(minimum, axis, px(-20.), viewport, size(px(320.), px(200.))),
                minimum
            );
            assert_eq!(
                resized_bounds(maximum, axis, px(20.), viewport, size(px(320.), px(200.))),
                maximum
            );
            let small = size(px(100.), px(80.));
            for amount in [px(20.), px(-20.)] {
                assert_eq!(
                    resized_bounds(original, axis, amount, small, size(px(320.), px(200.))),
                    Bounds::new(Point::default(), small)
                );
            }
        }
    }
    impl Focusable for TestTerminal {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl EventEmitter<ItemEvent> for TestTerminal {}
    impl crate::Item for TestTerminal {
        type Event = ItemEvent;
        fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
            "Test terminal".into()
        }
        fn to_item_events(event: &ItemEvent, callback: &mut dyn FnMut(ItemEvent)) {
            callback(*event);
        }
    }
    impl Render for TestTerminal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().track_focus(&self.focus_handle)
        }
    }

    fn add_pane(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) -> Entity<Pane> {
        let project = workspace.read(cx).project().clone();
        let pane = cx.new(|cx| {
            let mut pane = Pane::new(
                workspace.downgrade(),
                project,
                Default::default(),
                None,
                crate::NewFile.boxed_clone(),
                false,
                window,
                cx,
            );
            pane.set_should_display_tab_bar(|_, _| false);
            pane
        });
        let item = cx.new(|cx| TestTerminal {
            focus_handle: cx.focus_handle(),
        });
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), true, false, None, window, cx)
        });
        workspace
            .read(cx)
            .floating_panes()
            .clone()
            .update(cx, |layer, cx| {
                assert!(layer.add_pane(pane.clone(), crate::NewFile.boxed_clone(), window, cx));
            });
        pane
    }

    #[gpui::test]
    async fn floating_panes_preserve_sessions_focus_order_and_geometry(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        let original_focus = cx.update(|window, cx| window.focused(cx).unwrap());
        let first = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let second = cx.update(|window, cx| add_pane(&workspace, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                assert_eq!(layer.focused_pane(window, cx), Some(second.clone()));
                layer.activate_relative(true, window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                assert_eq!(layer.focused_pane(window, cx), Some(first.clone()));
                assert_eq!(
                    layer.stacking_order,
                    vec![second.entity_id(), first.entity_id()]
                );
                layer.activate_relative(false, window, cx);
                layer.hide(window, cx);
                assert!(original_focus.contains_focused(window, cx));
                assert_eq!(layer.panes.len(), 2);
                layer.show(window, cx);
            })
        });
        cx.run_until_parked();
        let bounds = cx.update(|_, cx| layer.read(cx).panes[0].stacked.desired_bounds.unwrap());
        cx.update(|window, cx| {
            original_focus.focus(window, cx);
            assert!(layer.read(cx).is_visible());
            layer.update(cx, |layer, cx| {
                layer.hide(window, cx);
                assert!(original_focus.contains_focused(window, cx));
                layer.set_viewport(size(px(100.), px(80.)), cx);
                assert_eq!(layer.panes[0].stacked.desired_bounds, Some(bounds));
                layer.set_viewport(size(px(1000.), px(800.)), cx);
                layer.show(window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(workspace.read(cx).focused_pane(window, cx), second);
            layer.update(cx, |layer, cx| layer.close_active(window, cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(layer.read(cx).active_pane(), Some(first));
            layer.update(cx, |layer, cx| layer.close_active(window, cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!layer.read(cx).has_panes());
            assert!(!layer.read(cx).is_visible());
            assert!(original_focus.contains_focused(window, cx));
        });
    }

    #[gpui::test]
    async fn floating_pane_dragging_and_closing_cancel_creation(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let pane = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.run_until_parked();
        let bounds = cx.update(|_, cx| layer.read(cx).panes[0].stacked.desired_bounds.unwrap());
        let start = bounds.origin + point(px(60.), px(16.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start + point(px(10.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            start + point(px(30.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            start + point(px(30.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                layer.read(cx).panes[0].stacked.desired_bounds,
                Some(Bounds::new(
                    bounds.origin + point(px(30.), px(40.)),
                    bounds.size
                ))
            )
        });
        let moved = cx.update(|_, cx| layer.read(cx).panes[0].stacked.desired_bounds.unwrap());
        let start = point(moved.right() - px(3.), moved.bottom() - px(3.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start + point(px(5.), px(5.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            start + point(px(20.), px(30.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            start + point(px(20.), px(30.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        cx.update(|_, cx| {
            let resized = layer.read(cx).panes[0].stacked.desired_bounds.unwrap();
            assert_eq!(resized.origin, moved.origin);
            assert!((resized.size.width - moved.size.width - px(20.)).abs() < px(0.001));
            assert!((resized.size.height - moved.size.height - px(30.)).abs() < px(0.001));
        });
        let mouse_resized =
            cx.update(|_, cx| layer.read(cx).panes[0].stacked.desired_bounds.unwrap());
        cx.update(|_, cx| {
            layer.update(cx, |layer, cx| {
                layer.resize_active(Axis::Horizontal, px(20.), cx);
                layer.resize_active(Axis::Vertical, px(-20.), cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let resized = layer.read(cx).panes[0].stacked.desired_bounds.unwrap();
            assert!((resized.size.width - mouse_resized.size.width - px(20.)).abs() < px(0.001));
            assert!((resized.size.height - mouse_resized.size.height + px(20.)).abs() < px(0.001));
            assert!((resized.center().x - mouse_resized.center().x).abs() < px(0.001));
            assert!((resized.center().y - mouse_resized.center().y).abs() < px(0.001));
        });
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                let dropped = dropped.clone();
                let task = cx.spawn(async move |_, _| {
                    let _guard = Dropped(dropped);
                    futures::future::pending::<()>().await;
                });
                layer.set_creation_task(pane.entity_id(), task);
                layer.show(window, cx);
            })
        });
        cx.run_until_parked();
        assert!(!dropped.load(Ordering::SeqCst));
        cx.update(|window, cx| layer.update(cx, |layer, cx| layer.close_active(window, cx)));
        cx.run_until_parked();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[gpui::test]
    async fn floating_pane_keyboard_resize_preserves_active_window_focus_and_visibility(
        cx: &mut TestAppContext,
    ) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.update(|_, cx| {
            layer.update(cx, |layer, cx| {
                layer.resize_active(Axis::Horizontal, px(20.), cx)
            })
        });
        let first = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let second = cx.update(|window, cx| add_pane(&workspace, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                let first_bounds = layer.panes[0].stacked.desired_bounds;
                let second_bounds = layer.panes[1].stacked.desired_bounds.unwrap();
                let stacking_order = layer.stacking_order.clone();
                for _ in 0..3 {
                    layer.resize_active(Axis::Horizontal, px(20.), cx);
                }
                let resized = layer.panes[1].stacked.desired_bounds.unwrap();
                assert_eq!(layer.panes[0].stacked.desired_bounds, first_bounds);
                assert_eq!(resized.size.width, second_bounds.size.width + px(60.));
                assert_eq!(resized.size.height, second_bounds.size.height);
                assert_eq!(layer.stacking_order, stacking_order);
                assert_eq!(layer.focused_pane(window, cx), Some(second.clone()));
                layer.hide(window, cx);
                layer.resize_active(Axis::Vertical, px(20.), cx);
                assert_eq!(layer.panes[1].stacked.desired_bounds, Some(resized));
                layer.show(window, cx);
                assert_eq!(layer.panes[1].stacked.desired_bounds, Some(resized));
                assert_eq!(layer.pane(first.entity_id()), Some(first));
                let viewport = layer.viewport;
                for smaller_viewport in [size(px(0.), px(0.)), size(px(100.), px(80.))] {
                    layer.set_viewport(smaller_viewport, cx);
                    layer.resize_active(Axis::Horizontal, px(20.), cx);
                    layer.resize_active(Axis::Vertical, px(-20.), cx);
                    assert_eq!(layer.panes[1].stacked.desired_bounds, Some(resized));
                }
                layer.set_viewport(size(px(800.), px(500.)), cx);
                layer.resize_active(Axis::Horizontal, px(-20.), cx);
                assert_eq!(
                    layer.panes[1].stacked.desired_bounds,
                    Some(Bounds::new(
                        point(px(10.), px(0.)),
                        size(px(780.), px(500.))
                    ))
                );
                layer.set_viewport(viewport, cx);
            })
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn floating_pane_movement_enlargement_reset_and_capacity(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        let panes = (0..4)
            .map(|_| cx.update(|window, cx| add_pane(&workspace, window, cx)))
            .collect::<Vec<_>>();
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                layer.set_viewport(size(px(1000.), px(800.)), cx);
                layer.reset_positions(cx);
                let original = Bounds::new(point(px(240.), px(210.)), size(px(400.), px(300.)));
                layer.panes[3].stacked.desired_bounds = Some(original);
                let others = layer
                    .panes
                    .iter()
                    .take(3)
                    .map(|entry| entry.stacked.desired_bounds)
                    .collect::<Vec<_>>();
                let stacking_order = layer.stacking_order.clone();
                for delta in [
                    point(px(-20.), px(0.)),
                    point(px(0.), px(20.)),
                    point(px(0.), px(-20.)),
                    point(px(20.), px(0.)),
                ] {
                    let before =
                        layer.panes[3].bounds(layer.layout, layer.viewport, layer.panes.len(), 3);
                    layer.move_active(delta, cx);
                    assert_eq!(
                        layer.panes[3].stacked.desired_bounds,
                        Some(Bounds::new(before.origin + delta, original.size))
                    );
                    assert_eq!(layer.focused_pane(window, cx), Some(panes[3].clone()));
                    assert_eq!(layer.stacking_order, stacking_order);
                }
                assert_eq!(layer.panes[3].stacked.desired_bounds, Some(original));
                layer.move_active(point(px(10000.), px(10000.)), cx);
                assert_eq!(
                    layer.panes[3].stacked.desired_bounds,
                    Some(Bounds::new(point(px(600.), px(500.)), original.size))
                );
                layer.move_active(point(px(-10000.), px(-10000.)), cx);
                assert_eq!(
                    layer.panes[3].stacked.desired_bounds,
                    Some(Bounds::new(Point::default(), original.size))
                );
                layer.panes[3].stacked.desired_bounds = Some(original);
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[3].stacked.restore_bounds, Some(original));
                let enlarged =
                    layer.panes[3].bounds(layer.layout, layer.viewport, layer.panes.len(), 3);
                assert_eq!(
                    enlarged,
                    Bounds::new(point(px(100.), px(80.)), size(px(800.), px(640.)))
                );
                layer.move_active(point(px(20.), px(20.)), cx);
                layer.resize_active(Axis::Horizontal, px(20.), cx);
                assert_eq!(
                    layer.panes[3].bounds(layer.layout, layer.viewport, layer.panes.len(), 3),
                    enlarged
                );
                assert_eq!(layer.panes[3].stacked.restore_bounds, Some(original));
                layer.set_viewport(size(px(1200.), px(1000.)), cx);
                assert_eq!(
                    layer.panes[3].bounds(layer.layout, layer.viewport, layer.panes.len(), 3),
                    enlarged_bounds(layer.viewport)
                );
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[3].stacked.desired_bounds, Some(original));
                assert!(layer.panes[3].stacked.restore_bounds.is_none());
                assert_eq!(
                    layer
                        .panes
                        .iter()
                        .take(3)
                        .map(|entry| entry.stacked.desired_bounds)
                        .collect::<Vec<_>>(),
                    others
                );
                layer.focus_pane(panes[0].entity_id(), window, cx);
                layer.toggle_maximize(window, cx);
                layer.focus_pane(panes[3].entity_id(), window, cx);
                layer.toggle_maximize(window, cx);
                assert!(layer.panes[0].stacked.restore_bounds.is_some());
                assert!(layer.panes[3].stacked.restore_bounds.is_some());
                layer.hide(window, cx);
                layer.move_active(point(px(20.), px(0.)), cx);
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[3].stacked.restore_bounds, Some(original));
                layer.show(window, cx);
                layer.set_viewport(size(px(500.), px(400.)), cx);
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[3].stacked.desired_bounds, Some(original));
                assert_eq!(
                    layer.panes[3].bounds(layer.layout, layer.viewport, layer.panes.len(), 3),
                    clamp_bounds(original, layer.viewport)
                );
                layer.set_viewport(size(px(1200.), px(1000.)), cx);
                let stacking_order = layer.stacking_order.clone();
                layer.reset_positions(cx);
                assert_eq!(layer.stacking_order, stacking_order);
                assert_eq!(layer.focused_pane(window, cx), Some(panes[3].clone()));
                for (index, id) in stacking_order.iter().enumerate() {
                    let entry = layer
                        .panes
                        .iter()
                        .find(|entry| entry.pane.entity_id() == *id)
                        .unwrap();
                    assert_eq!(
                        entry.stacked.desired_bounds,
                        Some(default_bounds(layer.viewport, index))
                    );
                    assert!(entry.stacked.restore_bounds.is_none());
                }
                assert!(!layer.can_add_pane());
                assert!(!layer.add_pane(
                    panes[0].clone(),
                    crate::NewFile.boxed_clone(),
                    window,
                    cx
                ));
                assert_eq!(layer.panes.len(), 4);
                layer.close_active(window, cx);
            })
        });
        cx.run_until_parked();
        assert!(layer.read_with(cx, |layer, _| layer.can_add_pane()));
        cx.update(|window, cx| add_pane(&workspace, window, cx));
        assert!(!layer.read_with(cx, |layer, _| layer.can_add_pane()));
    }

    #[gpui::test]
    async fn floating_pane_enlargement_disables_mouse_movement_and_resize(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        cx.update(|window, cx| add_pane(&workspace, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.run_until_parked();
        let original = cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                let original = layer.panes[0].stacked.desired_bounds;
                layer.toggle_maximize(window, cx);
                original
            })
        });
        cx.run_until_parked();
        let enlarged = cx.update(|_, cx| {
            let layer = layer.read(cx);
            layer.panes[0].bounds(layer.layout, layer.viewport, layer.panes.len(), 0)
        });
        for start in [
            enlarged.origin + point(px(60.), px(16.)),
            point(enlarged.right() - px(3.), enlarged.bottom() - px(3.)),
        ] {
            cx.simulate_mouse_move(start, None, Modifiers::default());
            cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(
                start + point(px(10.), px(10.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.simulate_mouse_move(
                start + point(px(40.), px(40.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.simulate_mouse_up(
                start + point(px(40.), px(40.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            cx.update(|_, cx| {
                let layer = layer.read(cx);
                assert_eq!(
                    layer.panes[0].bounds(layer.layout, layer.viewport, layer.panes.len(), 0),
                    enlarged
                );
                assert_eq!(layer.panes[0].stacked.restore_bounds, original);
            });
        }
        cx.update(|window, cx| layer.update(cx, |layer, cx| layer.toggle_maximize(window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(layer.read(cx).panes[0].stacked.desired_bounds, original));
    }

    #[test]
    fn floating_pane_tiled_defaults_cover_equal_space_without_overlap() {
        for viewport in [size(px(1200.), px(1000.)), size(px(480.), px(300.))] {
            let region = Bounds::new(
                point(viewport.width * 0.05, viewport.height * 0.05),
                size(viewport.width * 0.9, viewport.height * 0.9),
            );
            for count in 1..=4 {
                let tiles = (0..count)
                    .map(|index| tiled_bounds(viewport, count, index))
                    .collect::<Vec<_>>();
                let minimum = layout_minimum_size(FloatingPaneLayout::Tiled, viewport, count);
                let expected_area =
                    f32::from(region.size.width) * f32::from(region.size.height) / count as f32;
                for (index, tile) in tiles.iter().enumerate() {
                    assert_eq!(clamp_bounds_with_minimum(*tile, viewport, minimum), *tile);
                    assert!(
                        (f32::from(tile.size.width) * f32::from(tile.size.height) - expected_area)
                            .abs()
                            < 0.1
                    );
                    assert!(
                        tile.left() >= region.left() - px(0.001)
                            && tile.top() >= region.top() - px(0.001)
                    );
                    assert!(
                        tile.right() <= region.right() + px(0.001)
                            && tile.bottom() <= region.bottom() + px(0.001)
                    );
                    for other in tiles.iter().skip(index + 1) {
                        let overlap = tile.intersect(other);
                        assert!(
                            overlap.size.width <= px(0.001) || overlap.size.height <= px(0.001)
                        );
                    }
                }
                if count == 4 {
                    assert_eq!(tiles[0].top(), tiles[1].top());
                    assert_eq!(tiles[2].top(), tiles[3].top());
                    assert!(tiles[2].top() > tiles[0].top());
                } else {
                    assert!(
                        tiles
                            .iter()
                            .all(|tile| tile.size.height == region.size.height)
                    );
                }
            }
        }
    }

    #[gpui::test]
    async fn floating_pane_layouts_remember_edits_enlargement_and_viewport_changes(
        cx: &mut TestAppContext,
    ) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let first = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let second = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                layer.set_viewport(size(px(1200.), px(1000.)), cx);
                layer.reset_positions(cx);
                layer.move_active(point(px(40.), px(40.)), cx);
                layer.resize_active(Axis::Horizontal, px(-40.), cx);
                let stacked = layer.panes[1].stacked;
                let stacking_order = layer.stacking_order.clone();
                layer.toggle_maximize(window, cx);
                let stacked_enlarged = layer.panes[1].stacked;
                layer.toggle_layout(cx);
                assert_eq!(layer.layout, FloatingPaneLayout::Tiled);
                assert_eq!(layer.panes[1].tiled, FloatingPaneGeometry::default());
                assert_eq!(
                    layer.panes[1].bounds(layer.layout, layer.viewport, 2, 1),
                    tiled_bounds(layer.viewport, 2, 1)
                );
                layer.move_active(point(px(-40.), px(40.)), cx);
                layer.resize_active(Axis::Vertical, px(-40.), cx);
                let tiled = layer.panes[1].tiled;
                layer.toggle_maximize(window, cx);
                let tiled_enlarged = layer.panes[1].tiled;
                for _ in 0..3 {
                    layer.toggle_layout(cx);
                    assert_eq!(layer.panes[1].stacked, stacked_enlarged);
                    assert_eq!(layer.panes[1].tiled, tiled_enlarged);
                    assert_eq!(layer.focused_pane(window, cx), Some(second.clone()));
                    assert_eq!(layer.stacking_order, stacking_order);
                    layer.toggle_layout(cx);
                }
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[1].tiled, tiled);
                layer.toggle_layout(cx);
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[1].stacked, stacked);
                layer.toggle_layout(cx);
                layer.hide(window, cx);
                layer.show(window, cx);
                assert_eq!(layer.panes[1].tiled, tiled);
                layer.reset_positions(cx);
                assert_eq!(layer.panes[1].tiled, FloatingPaneGeometry::default());
                assert_eq!(layer.panes[1].stacked, stacked);
                layer.toggle_maximize(window, cx);
                layer.toggle_maximize(window, cx);
                assert_eq!(layer.panes[1].tiled, FloatingPaneGeometry::default());
                layer.set_viewport(size(px(800.), px(500.)), cx);
                assert_eq!(
                    layer.panes[1].bounds(layer.layout, layer.viewport, 2, 1),
                    tiled_bounds(layer.viewport, 2, 1)
                );
                layer.move_active(point(px(-40.), px(0.)), cx);
                let custom_tiled = layer.panes[1].tiled;
                layer.set_viewport(size(px(400.), px(300.)), cx);
                assert_eq!(layer.panes[1].tiled, custom_tiled);
                let displayed = layer.panes[1].bounds(layer.layout, layer.viewport, 2, 1);
                assert!(
                    displayed.right() <= layer.viewport.width
                        && displayed.bottom() <= layer.viewport.height
                );
                layer.set_viewport(size(px(800.), px(500.)), cx);
                assert_eq!(
                    layer.panes[1].bounds(layer.layout, layer.viewport, 2, 1),
                    custom_tiled.desired_bounds.unwrap()
                );
                layer.toggle_layout(cx);
                assert_eq!(layer.panes[1].stacked, stacked);
                assert_eq!(layer.pane(first.entity_id()), Some(first));
            })
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn floating_pane_count_changes_reset_only_tiled_geometry(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let first = cx.update(|window, cx| add_pane(&workspace, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.run_until_parked();
        let stacked = cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                layer.move_active(point(px(40.), px(0.)), cx);
                let stacked = layer.panes[0].stacked;
                layer.toggle_layout(cx);
                layer.resize_active(Axis::Horizontal, px(-40.), cx);
                layer.toggle_maximize(window, cx);
                assert!(layer.panes[0].tiled.restore_bounds.is_some());
                stacked
            })
        });
        let second = cx.update(|window, cx| add_pane(&workspace, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            layer.update(cx, |layer, cx| {
                assert_eq!(layer.layout, FloatingPaneLayout::Tiled);
                assert!(
                    layer
                        .panes
                        .iter()
                        .all(|entry| entry.tiled == FloatingPaneGeometry::default())
                );
                assert_eq!(layer.panes[0].stacked, stacked);
                layer.resize_active(Axis::Horizontal, px(-40.), cx);
                layer.toggle_layout(cx);
                layer.resize_active(Axis::Vertical, px(40.), cx);
                layer.close_active(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let layer = layer.read(cx);
            assert_eq!(layer.panes.len(), 1);
            assert_eq!(layer.pane(first.entity_id()), Some(first));
            assert!(layer.pane(second.entity_id()).is_none());
            assert_eq!(layer.panes[0].stacked, stacked);
            assert_eq!(layer.panes[0].tiled, FloatingPaneGeometry::default());
            assert_eq!(
                layer.panes[0].bounds(FloatingPaneLayout::Tiled, layer.viewport, 1, 0),
                tiled_bounds(layer.viewport, 1, 0)
            );
        });
    }

    #[gpui::test]
    async fn floating_pane_mouse_geometry_is_retained_per_layout(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        cx.update(|window, cx| add_pane(&workspace, window, cx));
        let layer = workspace.read_with(cx, |workspace, _| workspace.floating_panes().clone());
        cx.run_until_parked();
        let stacked = cx.update(|_, cx| layer.read(cx).panes[0].stacked);
        cx.update(|_, cx| layer.update(cx, |layer, cx| layer.toggle_layout(cx)));
        cx.run_until_parked();
        let original_tile = cx.update(|_, cx| {
            let layer = layer.read(cx);
            layer.panes[0].bounds(layer.layout, layer.viewport, 1, 0)
        });
        let start = original_tile.origin + point(px(60.), px(16.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start + point(px(5.), px(5.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            start + point(px(30.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            start + point(px(30.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let moved = cx.update(|_, cx| layer.read(cx).panes[0].tiled.desired_bounds.unwrap());
        assert_eq!(moved.size, original_tile.size);
        assert!((moved.origin.x - original_tile.origin.x - px(30.)).abs() < px(0.01));
        assert!((moved.origin.y - original_tile.origin.y - px(40.)).abs() < px(0.01));
        let start = point(moved.right() - px(3.), moved.bottom() - px(3.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start - point(px(5.), px(5.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            start - point(px(40.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            start - point(px(40.), px(40.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let edited = cx.update(|_, cx| layer.read(cx).panes[0].tiled);
        let resized = edited.desired_bounds.unwrap();
        assert_eq!(resized.origin, moved.origin);
        assert!((resized.size.width - moved.size.width + px(40.)).abs() < px(0.01));
        assert!((resized.size.height - moved.size.height + px(40.)).abs() < px(0.01));
        cx.update(|_, cx| {
            layer.update(cx, |layer, cx| {
                layer.toggle_layout(cx);
                assert_eq!(layer.panes[0].stacked, stacked);
                layer.move_active(point(px(-40.), px(-40.)), cx);
                let edited_stack = layer.panes[0].stacked;
                layer.toggle_layout(cx);
                assert_eq!(layer.panes[0].tiled, edited);
                layer.toggle_layout(cx);
                assert_eq!(layer.panes[0].stacked, edited_stack);
            })
        });
        cx.run_until_parked();
    }
}
