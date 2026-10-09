use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel,
    SharedString, Task, Window, rems,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::ModalView;

use super::{
    github::ReviewVerdict,
    review::PullRequestReview,
    threads::{CancelComment, SubmitComment},
};

/// Finishes the viewer's pending review with a verdict and an optional summary.
pub(crate) struct SubmitReviewModal {
    review: Entity<PullRequestReview>,
    editor: Entity<Editor>,
    pending_task: Option<Task<()>>,
    error: Option<SharedString>,
}

impl SubmitReviewModal {
    pub(crate) fn new(
        review: Entity<PullRequestReview>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 12, window, cx);
            editor.set_placeholder_text("Leave a summary (optional)", window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_use_modal_editing(true);
            editor
        });
        Self {
            review,
            editor,
            pending_task: None,
            error: None,
        }
    }

    fn submit(&mut self, verdict: ReviewVerdict, cx: &mut Context<Self>) {
        if self.pending_task.is_some() {
            return;
        }
        let body = self.editor.read(cx).text(cx).trim().to_string();
        let task = self
            .review
            .update(cx, |review, cx| review.submit(verdict, body, cx));
        self.pending_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.pending_task = None;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
        self.error = None;
        cx.notify();
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard your pending review?",
            Some("Its comments will be deleted from GitHub."),
            &["Discard", "Cancel"],
            cx,
        );
        let review = self.review.clone();
        self.pending_task = Some(cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                this.update(cx, |this, _| this.pending_task = None)
                    .log_err();
                return;
            }
            let result = review
                .update(cx, |review, cx| review.discard_pending_review(cx))
                .await;
            this.update(cx, |this, cx| {
                this.pending_task = None;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    fn submit_comment(&mut self, _: &SubmitComment, _: &mut Window, cx: &mut Context<Self>) {
        self.submit(ReviewVerdict::Comment, cx);
    }

    fn cancel(&mut self, _: &CancelComment, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for SubmitReviewModal {}
impl EventEmitter<DismissEvent> for SubmitReviewModal {}

impl Focusable for SubmitReviewModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SubmitReviewModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.review.read(cx);
        let details = review.details();
        let pending_count = review.pending_comment_count();
        let has_pending_review = details.pending_review.is_some();
        let is_own = details.viewer_did_author;
        let title = format!("Submit review · #{} {}", details.number, details.title);
        let busy = self.pending_task.is_some();
        let border_color = cx.theme().colors().border;
        let editor_background = cx.theme().colors().editor_background;
        v_flex()
            .key_context("SubmitPullRequestReview")
            .on_action(cx.listener(Self::dismiss))
            .w(rems(36.))
            .elevation_3(cx)
            .p_3()
            .gap_2()
            .child(Label::new(title).truncate())
            .child(
                Label::new(match pending_count {
                    0 => "No pending comments".to_string(),
                    1 => "1 pending comment".to_string(),
                    count => format!("{count} pending comments"),
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .key_context("PullRequestComposer")
                    .on_action(cx.listener(Self::submit_comment))
                    .on_action(cx.listener(Self::cancel))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border_color)
                    .bg(editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(has_pending_review, |row| {
                        row.child(
                            Button::new("discard-review", "Discard Review")
                                .label_size(LabelSize::Small)
                                .color(Color::Error)
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.discard(window, cx)),
                                ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("submit-comment", "Comment")
                            .label_size(LabelSize::Small)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit(ReviewVerdict::Comment, cx)
                            })),
                    )
                    .when(!is_own, |row| {
                        row.child(
                            Button::new("submit-request-changes", "Request Changes")
                                .label_size(LabelSize::Small)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit(ReviewVerdict::RequestChanges, cx)
                                })),
                        )
                        .child(
                            Button::new("submit-approve", "Approve")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Filled)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit(ReviewVerdict::Approve, cx)
                                })),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }
}
