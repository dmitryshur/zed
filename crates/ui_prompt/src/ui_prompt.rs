use gpui::{
    App, Decorations, Entity, EventEmitter, FocusHandle, Focusable, PromptButton, PromptHandle,
    PromptLevel, PromptResponse, RenderablePromptHandle, SharedString, TextStyleRefinement, Window,
    div, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::{Settings, SettingsStore};
use theme::ClientDecorationsExt;
use theme_settings::ThemeSettings;
use ui::{FluentBuilder, prelude::*};
use workspace::WorkspaceSettings;

pub fn init(cx: &mut App) {
    process_settings(cx);

    cx.observe_global::<SettingsStore>(process_settings)
        .detach();
}

fn process_settings(cx: &mut App) {
    let settings = WorkspaceSettings::get_global(cx);
    if settings.use_system_prompts && cfg!(not(any(target_os = "linux", target_os = "freebsd"))) {
        cx.reset_prompt_builder();
    } else {
        cx.set_prompt_builder(zed_prompt_renderer);
    }
}

/// Use this function in conjunction with [App::set_prompt_builder] to force
/// GPUI to use the internal prompt system.
fn zed_prompt_renderer(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let renderer = cx.new({
        |cx| ZedPromptRenderer {
            _level: level,
            message: cx.new(|cx| Markdown::new(SharedString::new(message), None, None, cx)),
            actions: actions.iter().map(|a| a.label().to_string()).collect(),
            focus: cx.focus_handle(),
            active_action_id: 0,
            detail: detail
                .filter(|text| !text.is_empty())
                .map(|text| cx.new(|cx| Markdown::new(SharedString::new(text), None, None, cx))),
        }
    });

    handle.with_view(renderer, window, cx)
}

pub struct ZedPromptRenderer {
    _level: PromptLevel,
    message: Entity<Markdown>,
    actions: Vec<String>,
    focus: FocusHandle,
    active_action_id: usize,
    detail: Option<Entity<Markdown>>,
}

impl ZedPromptRenderer {
    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(self.active_action_id));
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.actions.iter().position(|a| a == "Cancel") {
            cx.emit(PromptResponse(ix));
        }
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_action_id = 0;
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id = self.actions.len().saturating_sub(1);
        cx.notify();
    }

    // Moving stops at the first and last buttons: wrapping from the default (often destructive)
    // first button to the last one is easy to miss and confirms the wrong answer.
    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id =
            (self.active_action_id + 1).min(self.actions.len().saturating_sub(1));
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_action_id = self.active_action_id.saturating_sub(1);
        cx.notify();
    }
}

impl Render for ZedPromptRenderer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);

        let dialog = v_flex()
            .key_context("Prompt")
            .cursor_default()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .w_80()
            .p_4()
            .gap_4()
            .elevation_3(cx)
            .overflow_hidden()
            .font_family(settings.ui_font.family.clone())
            .child(div().w_full().child(MarkdownElement::new(
                self.message.clone(),
                markdown_style(true, window, cx),
            )))
            .children(self.detail.clone().map(|detail| {
                div().w_full().text_xs().child(MarkdownElement::new(
                    detail,
                    markdown_style(false, window, cx),
                ))
            }))
            .child(
                v_flex()
                    .gap_1()
                    .children(self.actions.iter().enumerate().map(|(ix, action)| {
                        let is_active = ix == self.active_action_id;
                        let button = Button::new(ix, action.clone())
                            .full_width()
                            .style(if is_active {
                                // Transparent, so the hover background below shows through.
                                ButtonStyle::OutlinedCustom(cx.theme().colors().border)
                            } else {
                                ButtonStyle::Outlined
                            })
                            .tab_index(ix as isize)
                            .on_click(cx.listener(move |_, _, _window, cx| {
                                cx.emit(PromptResponse(ix));
                            }));
                        // The keyboard selection looks like a hovered button, so it's as easy
                        // to see as the mouse's.
                        div()
                            .w_full()
                            .rounded_sm()
                            .when(is_active, |this| {
                                this.bg(cx.theme().colors().ghost_element_hover)
                            })
                            .child(button)
                    })),
            );

        let decorations = window.window_decorations();
        let inset = window.client_inset().unwrap_or(Pixels::ZERO);

        div().size_full().child(
            v_flex()
                .occlude()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.2))
                .map(|this| match decorations {
                    Decorations::Server => this,
                    Decorations::Client { tiling } => this
                        .when(!tiling.top, |this| this.top(inset))
                        .when(!tiling.bottom, |this| this.bottom(inset))
                        .when(!tiling.left, |this| this.left(inset))
                        .when(!tiling.right, |this| this.right(inset))
                        .rounded_client_corners(tiling),
                })
                .items_center()
                .justify_center()
                .child(dialog),
        )
    }
}

fn markdown_style(main_message: bool, window: &Window, cx: &App) -> MarkdownStyle {
    let mut base_text_style = window.text_style();
    let settings = ThemeSettings::get_global(cx);
    let font_size = settings.ui_font_size(cx).into();

    let color = if main_message {
        Color::Default.color(cx)
    } else {
        Color::Muted.color(cx)
    };

    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(settings.ui_font.family.clone()),
        font_size: Some(font_size),
        color: Some(color),
        ..Default::default()
    });

    MarkdownStyle {
        base_text_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        ..Default::default()
    }
}

impl EventEmitter<PromptResponse> for ZedPromptRenderer {}

impl Focusable for ZedPromptRenderer {
    fn focus_handle(&self, _: &crate::App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
    use gpui::{KeyBinding, TestAppContext, VisualTestContext};

    struct EmptyView;

    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn test_keyboard_selection_stops_at_the_ends(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
            cx.bind_keys([
                KeyBinding::new("enter", menu::Confirm, None),
                KeyBinding::new("ctrl-j", menu::SelectNext, None),
                KeyBinding::new("ctrl-k", menu::SelectPrevious, None),
            ]);
        });
        let window = cx.add_window(|_, _| EmptyView);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        for (keys, expected_answer) in [
            ("enter", 0),
            ("ctrl-k enter", 0),
            ("ctrl-j ctrl-j enter", 1),
            ("ctrl-j ctrl-k ctrl-k enter", 0),
        ] {
            let answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    "Delete this comment?",
                    None,
                    &["Delete", "Cancel"],
                    cx,
                )
            });
            cx.run_until_parked();
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert_eq!(
                answer.now_or_never(),
                Some(Ok(expected_answer)),
                "pressing {keys}"
            );
        }
    }
}
