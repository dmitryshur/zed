use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use anyhow::Result;
use collections::HashSet;
use editor::{
    Direction, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, SelectionEffects,
    SplittableEditor,
    scroll::{Autoscroll, ScrollAmount},
};
use git::repository::RepoPath;
use gpui::{
    AnyEntity, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, Window,
};
use language::{Buffer, Capability, HighlightedText, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::prelude::*;
use util::paths::PathStyle;
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

use super::comparison::LoadedCompareFile;

struct ShownFile {
    repo_path: RepoPath,
    buffer: Entity<Buffer>,
    path_key: PathKey,
}

/// The single diff tab of a branch comparison. Selecting another file in the Compare tab swaps
/// this view's contents instead of opening a new tab.
pub(crate) struct CompareDiffView {
    editor: Entity<SplittableEditor>,
    shown_file: Option<ShownFile>,
    error: Option<SharedString>,
    comparison_title: SharedString,
    /// Working-tree buffers that had unsaved edits when another file was shown. They're kept
    /// alive (the buffer store only holds them weakly) so the edits can still be saved.
    edited_buffers: Vec<Entity<Buffer>>,
    _editor_subscription: Subscription,
}

impl CompareDiffView {
    pub(crate) fn new(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_should_serialize(false, cx);
            });
            editor
        });
        let rhs_editor = editor.read(cx).rhs_editor().clone();
        let editor_subscription = cx.subscribe(&rhs_editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone());
        });
        Self {
            editor,
            shown_file: None,
            error: None,
            comparison_title: SharedString::default(),
            edited_buffers: Vec::new(),
            _editor_subscription: editor_subscription,
        }
    }

    pub(crate) fn shown_path(&self) -> Option<&RepoPath> {
        self.shown_file.as_ref().map(|file| &file.repo_path)
    }

    #[cfg(test)]
    pub(super) fn editor(&self) -> &Entity<SplittableEditor> {
        &self.editor
    }

    pub(crate) fn scroll(
        &mut self,
        amount: &ScrollAmount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shown_file.is_none() {
            return;
        }
        let editor = self.editor.read(cx).rhs_editor().clone();
        editor.update(cx, |editor, cx| editor.scroll_screen(amount, window, cx));
    }

    pub(crate) fn show_file(
        &mut self,
        file: LoadedCompareFile,
        comparison_title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_shown_file(cx);
        let path_key = PathKey::for_buffer(&file.buffer, cx);
        let max_point = file.buffer.read(cx).max_point();
        self.editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                path_key.clone(),
                file.buffer.clone(),
                [Point::zero()..max_point],
                0,
                file.diff,
                cx,
            );
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_read_only(file.read_only);
                let snapshot = editor.snapshot(window, cx);
                editor.go_to_hunk_before_or_after_position(
                    &snapshot,
                    Point::zero(),
                    Direction::Next,
                    true,
                    window,
                    cx,
                );
            });
        });
        self.shown_file = Some(ShownFile {
            repo_path: file.repo_path,
            buffer: file.buffer,
            path_key,
        });
        self.comparison_title = comparison_title;
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    pub(crate) fn show_error(
        &mut self,
        message: SharedString,
        comparison_title: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.clear_shown_file(cx);
        self.error = Some(message);
        self.comparison_title = comparison_title;
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    pub(crate) fn reveal_row(&mut self, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.shown_file.as_ref() else {
            return;
        };
        let buffer = file.buffer.clone();
        let editor = self.editor.read(cx).rhs_editor().clone();
        editor.update(cx, |editor, cx| {
            let buffer = buffer.read(cx);
            let point = Point::new(row.min(buffer.max_point().row), 0);
            let buffer_anchor = buffer.anchor_before(point);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(anchor) = snapshot.anchor_in_buffer(buffer_anchor) else {
                return;
            };
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| {
                    selections.select_ranges([anchor..anchor]);
                },
            );
        });
    }

    fn clear_shown_file(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        if let Some(shown_file) = self.shown_file.take() {
            if shown_file.buffer.read(cx).is_dirty()
                && !self.edited_buffers.contains(&shown_file.buffer)
            {
                self.edited_buffers.push(shown_file.buffer);
            }
            self.editor.update(cx, |editor, cx| {
                editor.remove_excerpts_for_path(shown_file.path_key, cx);
            });
        }
        self.edited_buffers
            .retain(|buffer| buffer.read(cx).is_dirty());
    }

    fn buffers(&self) -> impl Iterator<Item = &Entity<Buffer>> {
        self.shown_file
            .as_ref()
            .map(|file| &file.buffer)
            .into_iter()
            .chain(self.edited_buffers.iter())
    }
}

impl EventEmitter<EditorEvent> for CompareDiffView {}

impl Focusable for CompareDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CompareDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.shown_file
            .as_ref()
            .and_then(|file| file.repo_path.file_name())
            .map(|file_name| SharedString::from(file_name.to_string()))
            .unwrap_or_else(|| "Branch Comparison".into())
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(match &self.shown_file {
            Some(file) => format!(
                "{} · {}",
                file.repo_path.display(PathStyle::local()),
                self.comparison_title
            )
            .into(),
            None => self.comparison_title.clone(),
        })
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            None
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        for buffer in self.buffers() {
            f(buffer.entity_id(), buffer.read(cx));
        }
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffers().any(|buffer| buffer.read(cx).is_dirty())
    }

    fn can_save(&self, cx: &App) -> bool {
        self.buffers()
            .any(|buffer| buffer.read(cx).capability() == Capability::ReadWrite)
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let dirty_buffers = self
            .buffers()
            .filter(|buffer| buffer.read(cx).is_dirty())
            .cloned()
            .collect::<HashSet<_>>();
        project.update(cx, |project, cx| project.save_buffers(dirty_buffers, cx))
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        let file = self.shown_file.as_ref()?;
        Some((
            vec![HighlightedText {
                text: format!(
                    "{} · {}",
                    file.repo_path.display(PathStyle::local()),
                    self.comparison_title
                )
                .into(),
                highlights: Vec::new(),
            }],
            Some(
                theme_settings::ThemeSettings::get_global(cx)
                    .buffer_font
                    .clone(),
            ),
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx)
            })
        });
    }
}

impl Render for CompareDiffView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.error {
            Some(error) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new(error.clone()).color(Color::Muted))
                .into_any_element(),
            None => self.editor.clone().into_any_element(),
        }
    }
}
