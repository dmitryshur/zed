use collections::HashMap;
use gpui::{
    Action, Bounds, CursorStyle, DragMoveEvent, Entity, EntityId, FocusHandle, Focusable,
    MouseButton, Pixels, Point, Size, Subscription, Task, WeakFocusHandle, point, size,
};
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt;

use crate::{CloseAllItems, ItemHandle, Pane, PaneSearchBarCallbacks, item::ItemEvent, pane};

struct FloatingPane {
    pane: Entity<Pane>,
    desired_bounds: Option<Bounds<Pixels>>,
    new_action: Box<dyn Action>,
    _creation: Task<()>,
    _subscriptions: Vec<Subscription>,
    item_subscriptions: HashMap<EntityId, Subscription>,
}

pub struct FloatingPaneLayer {
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
    pane_id: EntityId,
    bounds: Bounds<Pixels>,
    mouse_position: Point<Pixels>,
    edge: Option<Edge>,
}

struct DragPreview;

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl FloatingPaneLayer {
    pub(crate) fn new(fallback_focus: FocusHandle) -> Self {
        Self {
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
            if entry.desired_bounds.is_none() && viewport.width > px(0.) && viewport.height > px(0.)
            {
                entry.desired_bounds = Some(default_bounds(viewport, index));
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
    ) {
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
            desired_bounds,
            new_action,
            _creation: Task::ready(()),
            _subscriptions: subscriptions,
            item_subscriptions: HashMap::default(),
        });
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
        if drag.layer_id != cx.entity_id() {
            return;
        }
        let bounds = dragged_bounds(
            drag.bounds,
            event.event.position - drag.mouse_position,
            drag.edge,
            self.viewport,
        );
        if let Some(entry) = self
            .panes
            .iter_mut()
            .find(|entry| entry.pane.entity_id() == drag.pane_id)
        {
            entry.desired_bounds = Some(bounds);
            cx.notify();
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
            let Some(entry) = self
                .panes
                .iter()
                .find(|entry| entry.pane.entity_id() == *pane_id)
            else {
                continue;
            };
            let pane_id = *pane_id;
            let bounds = clamp_bounds(
                entry
                    .desired_bounds
                    .unwrap_or_else(|| default_bounds(self.viewport, 0)),
                self.viewport,
            );
            let drag = FloatingPaneDrag {
                layer_id: cx.entity_id(),
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
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.focus_pane(pane_id, window, cx)
                                }),
                            )
                            .on_drag(drag.clone(), |_, _, _, cx| cx.new(|_| DragPreview))
                            .on_drag_move::<FloatingPaneDrag>(cx.listener(Self::drag_move))
                            .child(div().flex_1().overflow_hidden().children(title))
                            .child(
                                IconButton::new("new-floating-terminal", IconName::Plus)
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
                    .children(
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
                    ),
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

fn clamp_bounds(mut bounds: Bounds<Pixels>, viewport: Size<Pixels>) -> Bounds<Pixels> {
    bounds.size.width = bounds.size.width.max(px(320.)).min(viewport.width);
    bounds.size.height = bounds.size.height.max(px(200.)).min(viewport.height);
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

fn dragged_bounds(
    bounds: Bounds<Pixels>,
    delta: Point<Pixels>,
    edge: Option<Edge>,
    viewport: Size<Pixels>,
) -> Bounds<Pixels> {
    let Some(edge) = edge else {
        return clamp_bounds(Bounds::new(bounds.origin + delta, bounds.size), viewport);
    };
    let minimum_width = px(320.).min(viewport.width);
    let minimum_height = px(200.).min(viewport.height);
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
                dragged_bounds(original, point(px(20.), px(30.)), Some(edge), viewport),
                expected
            );
            for delta in [
                point(px(-10000.), px(-10000.)),
                point(px(10000.), px(10000.)),
            ] {
                let resized = dragged_bounds(original, delta, Some(edge), viewport);
                assert!(resized.left() >= px(0.) && resized.top() >= px(0.));
                assert!(resized.right() <= viewport.width && resized.bottom() <= viewport.height);
                assert!(resized.size.width >= px(320.) && resized.size.height >= px(200.));
            }
        }
        let moved = dragged_bounds(original, point(px(10000.), px(-10000.)), None, viewport);
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
                layer.add_pane(pane.clone(), crate::NewFile.boxed_clone(), window, cx);
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
        let bounds = cx.update(|_, cx| layer.read(cx).panes[0].desired_bounds.unwrap());
        cx.update(|window, cx| {
            original_focus.focus(window, cx);
            assert!(layer.read(cx).is_visible());
            layer.update(cx, |layer, cx| {
                layer.hide(window, cx);
                assert!(original_focus.contains_focused(window, cx));
                layer.set_viewport(size(px(100.), px(80.)), cx);
                assert_eq!(layer.panes[0].desired_bounds, Some(bounds));
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
        let bounds = cx.update(|_, cx| layer.read(cx).panes[0].desired_bounds.unwrap());
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
                layer.read(cx).panes[0].desired_bounds,
                Some(Bounds::new(
                    bounds.origin + point(px(30.), px(40.)),
                    bounds.size
                ))
            )
        });
        let moved = cx.update(|_, cx| layer.read(cx).panes[0].desired_bounds.unwrap());
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
            let resized = layer.read(cx).panes[0].desired_bounds.unwrap();
            assert_eq!(resized.origin, moved.origin);
            assert!((resized.size.width - moved.size.width - px(20.)).abs() < px(0.001));
            assert!((resized.size.height - moved.size.height - px(30.)).abs() < px(0.001));
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
}
