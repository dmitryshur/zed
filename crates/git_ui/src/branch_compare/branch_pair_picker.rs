use std::sync::Arc;

use fuzzy_nucleo::StringMatchCandidate;
use git::repository::Branch;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, Window, rems,
};
use picker::{Picker, PickerDelegate};
use project::git_store::{Repository, RepositoryEvent};
use time::OffsetDateTime;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use workspace::ModalView;

type CompareCallback = Arc<dyn Fn(Branch, Branch, &mut Window, &mut App)>;

/// A modal listing local and remote branches with checkboxes. The first checked branch is the
/// base, the second is compared against it.
pub(crate) struct BranchPairPicker {
    picker: Entity<Picker<BranchPairDelegate>>,
    _subscriptions: Vec<Subscription>,
}

impl BranchPairPicker {
    pub(crate) fn new(
        repository: Entity<Repository>,
        on_compare: impl Fn(Branch, Branch, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_snapshot = repository.read(cx);
        let delegate = BranchPairDelegate {
            branches: sorted_by_recency(&repository_snapshot.branch_list),
            branch_list_error: repository_snapshot.branch_list_error.clone(),
            matches: Vec::new(),
            selected_index: 0,
            selection: PairSelection::default(),
            on_compare: Arc::new(on_compare),
            focus_handle: cx.focus_handle(),
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(34.))
                .show_scrollbar(true)
        });
        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle;
        });

        let subscriptions = vec![
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            cx.subscribe_in(
                &repository,
                window,
                |this, repository, event: &RepositoryEvent, window, cx| {
                    if !matches!(event, RepositoryEvent::BranchListChanged) {
                        return;
                    }
                    let repository = repository.read(cx);
                    let branches = sorted_by_recency(&repository.branch_list);
                    let branch_list_error = repository.branch_list_error.clone();
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.selection.retain_existing(&branches);
                        picker.delegate.branches = branches;
                        picker.delegate.branch_list_error = branch_list_error;
                        picker.refresh(window, cx);
                    });
                },
            ),
        ];

        Self {
            picker,
            _subscriptions: subscriptions,
        }
    }
}

impl ModalView for BranchPairPicker {}
impl EventEmitter<DismissEvent> for BranchPairPicker {}

impl Focusable for BranchPairPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for BranchPairPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CompareBranchesPicker")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

/// The checked items in the order they were checked, keyed by name so they survive filtering.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PairSelection {
    pub(super) checked: Vec<SharedString>,
}

impl PairSelection {
    /// Unchecks a checked item, or checks it if fewer than two are checked.
    pub(super) fn toggle(&mut self, ref_name: SharedString) {
        if let Some(position) = self.checked.iter().position(|checked| checked == &ref_name) {
            self.checked.remove(position);
        } else if self.checked.len() < 2 {
            self.checked.push(ref_name);
        }
    }

    pub(super) fn contains(&self, ref_name: &SharedString) -> bool {
        self.checked.contains(ref_name)
    }

    fn base(&self) -> Option<&SharedString> {
        self.checked.first()
    }

    fn compared(&self) -> Option<&SharedString> {
        self.checked.get(1)
    }

    fn retain_existing(&mut self, branches: &[Branch]) {
        self.checked
            .retain(|ref_name| branches.iter().any(|branch| &branch.ref_name == ref_name));
    }
}

fn sorted_by_recency(branches: &[Branch]) -> Vec<Branch> {
    let mut branches = branches.to_vec();
    branches.sort_by(|left, right| {
        let left_timestamp = left
            .most_recent_commit
            .as_ref()
            .map(|commit| commit.commit_timestamp);
        let right_timestamp = right
            .most_recent_commit
            .as_ref()
            .map(|commit| commit.commit_timestamp);
        right_timestamp
            .cmp(&left_timestamp)
            .then_with(|| left.name().cmp(right.name()))
    });
    branches
}

fn branch_display_name(ref_name: &SharedString, branches: &[Branch]) -> SharedString {
    branches
        .iter()
        .find(|branch| &branch.ref_name == ref_name)
        .map(|branch| SharedString::from(branch.name().to_string()))
        .unwrap_or_else(|| ref_name.clone())
}

fn footer_label(selection: &PairSelection, branches: &[Branch]) -> SharedString {
    match (selection.base(), selection.compared()) {
        (None, _) => "Check the base branch".into(),
        (Some(base), None) => format!(
            "Base: {} — check the branch to compare",
            branch_display_name(base, branches)
        )
        .into(),
        (Some(base), Some(compared)) => format!(
            "{} since {}",
            branch_display_name(compared, branches),
            branch_display_name(base, branches)
        )
        .into(),
    }
}

struct BranchMatch {
    branch: Branch,
    positions: Vec<usize>,
}

pub(crate) struct BranchPairDelegate {
    branches: Vec<Branch>,
    branch_list_error: Option<SharedString>,
    matches: Vec<BranchMatch>,
    selected_index: usize,
    selection: PairSelection,
    on_compare: CompareCallback,
    focus_handle: FocusHandle,
}

impl BranchPairDelegate {
    fn checked_branch(&self, ref_name: Option<&SharedString>) -> Option<Branch> {
        let ref_name = ref_name?;
        self.branches
            .iter()
            .find(|branch| &branch.ref_name == ref_name)
            .cloned()
    }

    fn compare(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let (Some(base), Some(compared)) = (
            self.checked_branch(self.selection.base()),
            self.checked_branch(self.selection.compared()),
        ) else {
            return;
        };
        (self.on_compare)(base, compared, window, cx);
        cx.emit(DismissEvent);
    }

    fn render_branch(
        &self,
        ix: usize,
        selected: bool,
        checkbox: Option<AnyElement>,
    ) -> Option<ListItem> {
        let branch_match = self.matches.get(ix)?;
        let branch = &branch_match.branch;
        let icon = if branch.is_remote() {
            IconName::Server
        } else {
            IconName::GitBranch
        };
        let commit_summary = branch.most_recent_commit.as_ref().map(|commit| {
            let commit_time = OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
                .unwrap_or_else(|_| OffsetDateTime::now_utc());
            let local_offset =
                time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            let relative_time = time_format::format_localized_timestamp(
                commit_time,
                OffsetDateTime::now_utc(),
                local_offset,
                time_format::TimestampFormat::Relative,
            );
            format!("{relative_time} • {}", commit.subject)
        });
        let role = if self.selection.base() == Some(&branch.ref_name) {
            Some("base")
        } else if self.selection.compared() == Some(&branch.ref_name) {
            Some("compared")
        } else {
            None
        };

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<AnyElement>(checkbox)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2p5()
                        .child(
                            Icon::new(icon)
                                .color(if branch.is_head {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                })
                                .size(IconSize::Small),
                        )
                        .child(
                            v_flex()
                                .w_full()
                                .min_w_0()
                                .child(
                                    HighlightedLabel::new(
                                        branch.name().to_string(),
                                        branch_match.positions.clone(),
                                    )
                                    .truncate(),
                                )
                                .children(commit_summary.map(|summary| {
                                    Label::new(summary)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate()
                                })),
                        ),
                )
                .end_slot::<Label>(
                    role.map(|role| Label::new(role).size(LabelSize::Small).color(Color::Accent)),
                ),
        )
    }
}

impl PickerDelegate for BranchPairDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "compare branches picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Check two branches to compare…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(
            self.branch_list_error
                .clone()
                .unwrap_or_else(|| "No branches found".into()),
        )
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
        let branches = self.branches.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                branches
                    .into_iter()
                    .map(|branch| BranchMatch {
                        branch,
                        positions: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            } else {
                let candidates = branches
                    .iter()
                    .enumerate()
                    .map(|(ix, branch)| StringMatchCandidate::new(ix, branch.name()))
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
                    Some(BranchMatch {
                        branch: branches.get(candidate.candidate_id)?.clone(),
                        positions: candidate.positions,
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
        self.compare(window, cx);
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn is_multi_select_persistent(&self) -> bool {
        true
    }

    fn is_item_selected(&self, ix: usize) -> bool {
        self.matches
            .get(ix)
            .is_some_and(|branch_match| self.selection.contains(&branch_match.branch.ref_name))
    }

    fn toggle_item_selected(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(branch_match) = self.matches.get(ix) {
            self.selection.toggle(branch_match.branch.ref_name.clone());
            cx.notify();
        }
    }

    fn selected_item_count(&self) -> usize {
        self.selection.checked.len()
    }

    fn clear_selection(&mut self, _cx: &mut Context<Picker<Self>>) {
        self.selection = PairSelection::default();
    }

    fn confirm_multi(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.compare(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        self.render_branch(ix, selected, None)
    }

    fn render_match_with_checkbox(
        &self,
        ix: usize,
        selected: bool,
        checkbox: AnyElement,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        self.render_branch(ix, selected, Some(checkbox))
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let can_compare = self.selection.compared().is_some();
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_2()
                .justify_between()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(footer_label(&self.selection, &self.branches))
                        .size(LabelSize::Small)
                        .color(if can_compare {
                            Color::Default
                        } else {
                            Color::Muted
                        })
                        .truncate(),
                )
                .child(
                    Button::new("compare-branches", "Compare")
                        .disabled(!can_compare)
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Confirm, &self.focus_handle, cx)
                                .map(|key_binding| key_binding.size(rems_from_px(12_f32))),
                        )
                        .on_click(cx.listener(|picker, _, window, cx| {
                            picker.delegate.compare(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_compare::comparison::tests::setup;
    use git::repository::CommitSummary;
    use gpui::TestAppContext;
    use std::{cell::RefCell, rc::Rc};

    #[gpui::test]
    async fn test_compare_requires_two_checked_branches(cx: &mut TestAppContext) {
        let (_project, repository, _fs) = setup("main", cx).await;
        let compared = Rc::new(RefCell::new(None));
        let (picker, cx) = cx.add_window_view({
            let compared = compared.clone();
            move |window, cx| {
                BranchPairPicker::new(
                    repository,
                    move |base, compared_branch, _, _| {
                        compared.replace(Some((
                            base.name().to_string(),
                            compared_branch.name().to_string(),
                        )));
                    },
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| picker.focus_handle(cx).focus(window, cx));

        cx.dispatch_action(picker::MultiSelectNext);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            *compared.borrow(),
            None,
            "one branch isn't enough to compare"
        );
        picker.read_with(cx, |picker, cx| {
            assert_eq!(
                picker.picker.read(cx).delegate.selection.checked,
                vec![SharedString::from("refs/heads/feature")]
            );
        });

        cx.dispatch_action(picker::MultiSelectNext);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            *compared.borrow(),
            Some(("feature".to_string(), "main".to_string())),
            "the first checked branch is the base"
        );
    }

    fn branch(ref_name: &str, commit_timestamp: Option<i64>) -> Branch {
        Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: commit_timestamp.map(|commit_timestamp| CommitSummary {
                sha: "0000000".into(),
                subject: "subject".into(),
                commit_timestamp,
                author_name: "Author".into(),
                has_parent: true,
            }),
        }
    }

    #[test]
    fn test_pair_selection_keeps_check_order() {
        let mut selection = PairSelection::default();
        selection.toggle("refs/heads/main".into());
        selection.toggle("refs/heads/feature".into());
        assert_eq!(
            selection.base().map(|r| r.as_ref()),
            Some("refs/heads/main")
        );
        assert_eq!(
            selection.compared().map(|r| r.as_ref()),
            Some("refs/heads/feature")
        );

        selection.toggle("refs/heads/other".into());
        assert_eq!(
            selection.checked.len(),
            2,
            "a third branch can't be checked"
        );

        selection.toggle("refs/heads/main".into());
        assert_eq!(
            selection.base().map(|r| r.as_ref()),
            Some("refs/heads/feature"),
            "unchecking the base promotes the compared branch"
        );
        assert_eq!(selection.compared(), None);

        selection.retain_existing(&[branch("refs/heads/main", None)]);
        assert_eq!(selection, PairSelection::default());
    }

    #[test]
    fn test_footer_label() {
        let branches = [
            branch("refs/heads/main", None),
            branch("refs/remotes/origin/feature/tabs", None),
        ];
        let mut selection = PairSelection::default();
        assert_eq!(footer_label(&selection, &branches), "Check the base branch");
        selection.toggle("refs/heads/main".into());
        assert_eq!(
            footer_label(&selection, &branches),
            "Base: main — check the branch to compare"
        );
        selection.toggle("refs/remotes/origin/feature/tabs".into());
        assert_eq!(
            footer_label(&selection, &branches),
            "origin/feature/tabs since main"
        );
    }

    #[test]
    fn test_sorted_by_recency() {
        let branches = sorted_by_recency(&[
            branch("refs/heads/old", Some(1)),
            branch("refs/heads/no-commit", None),
            branch("refs/heads/new", Some(3)),
            branch("refs/heads/also-new", Some(3)),
        ]);
        assert_eq!(
            branches.iter().map(Branch::name).collect::<Vec<_>>(),
            ["also-new", "new", "old", "no-commit"]
        );
    }
}
