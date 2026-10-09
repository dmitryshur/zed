use std::sync::Arc;

use collections::HashMap;
use futures::future::Shared;
use fuzzy_nucleo::StringMatchCandidate;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use time::OffsetDateTime;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, TintColor, prelude::*};
use workspace::ModalView;

use super::github::{GitHubClient, GitHubRepository, PullRequestFilter, PullRequestSummary};

actions!(
    pull_request_picker,
    [
        /// Shows the next filter of the pull request picker.
        NextFilter,
        /// Shows the previous filter of the pull request picker.
        PreviousFilter,
    ]
);

pub(crate) type ClientTask = Shared<Task<Result<GitHubClient, SharedString>>>;

type PickCallback = Arc<dyn Fn(PullRequestSummary, &mut Window, &mut App)>;

/// A modal listing the repository's open pull requests, by filter.
pub(crate) struct PullRequestPicker {
    picker: Entity<Picker<PullRequestPickerDelegate>>,
    _subscription: Subscription,
}

impl PullRequestPicker {
    pub(crate) fn new(
        client: ClientTask,
        github_repository: GitHubRepository,
        on_pick: impl Fn(PullRequestSummary, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = PullRequestPickerDelegate {
            client,
            github_repository,
            filter: PullRequestFilter::ReviewRequested,
            states: HashMap::default(),
            fetch_tasks: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            on_pick: Arc::new(on_pick),
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(40.))
                .show_scrollbar(true);
            let filter = picker.delegate.filter;
            picker.delegate.load(filter, window, cx);
            picker
        });
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }

    fn next_filter(&mut self, _: &NextFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.move_filter(true, window, cx);
    }

    fn previous_filter(&mut self, _: &PreviousFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.move_filter(false, window, cx);
    }

    /// Moves to the neighboring filter chip, stopping at the first and last ones.
    fn move_filter(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            let filters = PullRequestFilter::ALL;
            let Some(current) = filters
                .iter()
                .position(|filter| *filter == picker.delegate.filter)
            else {
                return;
            };
            let target = if forward {
                current.checked_add(1)
            } else {
                current.checked_sub(1)
            };
            if let Some(filter) = target.and_then(|index| filters.get(index)) {
                picker.delegate.set_filter(*filter, window, cx);
            }
        });
    }
}

#[cfg(test)]
impl PullRequestPicker {
    pub(super) fn picker(&self) -> &Entity<Picker<PullRequestPickerDelegate>> {
        &self.picker
    }
}

#[cfg(test)]
impl PullRequestPickerDelegate {
    pub(super) fn filter(&self) -> PullRequestFilter {
        self.filter
    }

    pub(super) fn matched_numbers(&self) -> Vec<u64> {
        self.matches
            .iter()
            .map(|pull_request_match| pull_request_match.pull_request.number)
            .collect()
    }
}

impl ModalView for PullRequestPicker {}
impl EventEmitter<DismissEvent> for PullRequestPicker {}

impl Focusable for PullRequestPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for PullRequestPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PullRequestPicker")
            .w(rems(40.))
            .on_action(cx.listener(Self::next_filter))
            .on_action(cx.listener(Self::previous_filter))
            .child(self.picker.clone())
    }
}

enum FilterState {
    Loading,
    Loaded(Vec<PullRequestSummary>),
    Failed(SharedString),
}

struct PullRequestMatch {
    pull_request: PullRequestSummary,
    positions: Vec<usize>,
}

pub(crate) struct PullRequestPickerDelegate {
    client: ClientTask,
    github_repository: GitHubRepository,
    filter: PullRequestFilter,
    states: HashMap<PullRequestFilter, FilterState>,
    fetch_tasks: Vec<Task<()>>,
    matches: Vec<PullRequestMatch>,
    selected_index: usize,
    on_pick: PickCallback,
}

impl PullRequestPickerDelegate {
    fn set_filter(
        &mut self,
        filter: PullRequestFilter,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.load(filter, window, cx);
        cx.notify();
    }

    /// Fetches a filter's pull requests the first time it's shown.
    fn load(
        &mut self,
        filter: PullRequestFilter,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if self.states.contains_key(&filter) {
            cx.defer_in(window, |picker, window, cx| picker.refresh(window, cx));
            return;
        }
        self.states.insert(filter, FilterState::Loading);
        self.matches.clear();
        let client = self.client.clone();
        let github_repository = self.github_repository.clone();
        self.fetch_tasks
            .push(cx.spawn_in(window, async move |picker, cx| {
                let state = match client.await {
                    Ok(client) => match client
                        .search_pull_requests(&github_repository, filter)
                        .await
                    {
                        Ok(pull_requests) => FilterState::Loaded(pull_requests),
                        Err(error) => FilterState::Failed(format!("{error:#}").into()),
                    },
                    Err(error) => FilterState::Failed(error),
                };
                picker
                    .update_in(cx, |picker, window, cx| {
                        picker.delegate.states.insert(filter, state);
                        if picker.delegate.filter == filter {
                            picker.refresh(window, cx);
                        }
                    })
                    .ok();
            }));
    }

    fn pull_requests(&self) -> &[PullRequestSummary] {
        match self.states.get(&self.filter) {
            Some(FilterState::Loaded(pull_requests)) => pull_requests,
            Some(FilterState::Loading | FilterState::Failed(_)) | None => &[],
        }
    }
}

fn display_text(pull_request: &PullRequestSummary) -> String {
    format!("#{} {}", pull_request.number, pull_request.title)
}

fn relative_time(timestamp: OffsetDateTime) -> String {
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    time_format::format_localized_timestamp(
        timestamp,
        OffsetDateTime::now_utc(),
        local_offset,
        time_format::TimestampFormat::Relative,
    )
}

impl PickerDelegate for PullRequestPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "pull request picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search pull requests…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match self.states.get(&self.filter) {
            Some(FilterState::Loading) | None => "Loading pull requests…".into(),
            Some(FilterState::Failed(error)) => error.clone(),
            Some(FilterState::Loaded(pull_requests)) if pull_requests.is_empty() => {
                "No open pull requests".into()
            }
            Some(FilterState::Loaded(_)) => "No matching pull requests".into(),
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let pull_requests = self.pull_requests().to_vec();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                pull_requests
                    .into_iter()
                    .map(|pull_request| PullRequestMatch {
                        pull_request,
                        positions: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            } else {
                let candidates = pull_requests
                    .iter()
                    .enumerate()
                    .map(|(ix, pull_request)| {
                        StringMatchCandidate::new(
                            ix,
                            &format!(
                                "{} @{} {}",
                                display_text(pull_request),
                                pull_request.author.as_deref().unwrap_or_default(),
                                pull_request.head_ref_name
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                fuzzy_nucleo::match_strings_async(
                    &candidates,
                    &query,
                    fuzzy_nucleo::Case::Smart,
                    fuzzy_nucleo::LengthPenalty::On,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
                .into_iter()
                .filter_map(|candidate| {
                    let pull_request = pull_requests.get(candidate.candidate_id)?.clone();
                    let display_len = display_text(&pull_request).len();
                    Some(PullRequestMatch {
                        positions: candidate
                            .positions
                            .into_iter()
                            .filter(|position| *position < display_len)
                            .collect(),
                        pull_request,
                    })
                })
                .collect()
            };
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(pull_request_match) = self.matches.get(self.selected_index) else {
            return;
        };
        (self.on_pick)(pull_request_match.pull_request.clone(), window, cx);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_header(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            h_flex()
                .debug_selector(|| "pull-request-filters".into())
                .w_full()
                .px_2()
                .py_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .children(PullRequestFilter::ALL.into_iter().map(|filter| {
                    Button::new(filter.label(), filter.label())
                        .label_size(LabelSize::Small)
                        .toggle_state(filter == self.filter)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(cx.listener(move |picker, _, window, cx| {
                            picker.delegate.set_filter(filter, window, cx);
                        }))
                }))
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let pull_request_match = self.matches.get(ix)?;
        let pull_request = &pull_request_match.pull_request;
        let byline = match &pull_request.author {
            Some(author) => format!("@{author} · {}", relative_time(pull_request.updated_at)),
            None => relative_time(pull_request.updated_at),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .justify_between()
                        .child(
                            div().min_w_0().child(
                                HighlightedLabel::new(
                                    display_text(pull_request),
                                    pull_request_match.positions.clone(),
                                )
                                .truncate(),
                            ),
                        )
                        .child(
                            Label::new(byline)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                ),
        )
    }
}
