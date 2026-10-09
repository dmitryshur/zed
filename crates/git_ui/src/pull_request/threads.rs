use std::{
    any::Any,
    ops::{Range, RangeInclusive},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use editor::{
    Addon, Anchor, DiffReviewProvider, Editor, RowInfo, SelectionEffects, SplittableEditor,
    ToPoint as _,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    scroll::Autoscroll,
};
use git::{Oid, repository::RepoPath};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable as _, KeyContext,
    PromptLevel, SharedString, Subscription, Task, WeakEntity, WeakFocusHandle, Window, actions,
};
use language::{Buffer, BufferEvent, LanguageRegistry, Point};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::MultiBufferRow;
use time::OffsetDateTime;
use ui::{Chip, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use super::{
    AddPullRequestComment, DeletePullRequestComment, EditPullRequestComment, NextPullRequestThread,
    PreviousPullRequestThread, TogglePullRequestThreadResolved,
    github::{DiffSide, NewThread, ReviewComment, ReviewThread, SubjectType},
    line_mapping::{LineMap, locate_by_diff_hunk},
    review::{PullRequestReview, PullRequestReviewEvent, ReviewRange},
};

actions!(
    pull_request_review,
    [
        /// Adds the comment being written to the pull request review.
        SubmitComment,
        /// Discards the comment being written.
        CancelComment,
    ]
);

const NOT_IN_DIFF: &str = "GitHub only allows comments within the pull request's diff";

/// What the review comments of a file shown from a pull request comparison attach to.
#[derive(Clone)]
pub(crate) struct ReviewFileContext {
    pub(crate) review: Entity<PullRequestReview>,
    pub(crate) repo_path: RepoPath,
    pub(crate) range: ReviewRange,
    /// The commit the shown file comes from, or `None` for the working tree.
    pub(crate) view_commit: Option<Oid>,
}

#[derive(Clone, Copy)]
enum CommentChange {
    Edit,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EditorSide {
    /// The new side, which also shows removed lines in the unified view.
    Rhs,
    /// The old side of the split view.
    Lhs,
}

struct AttachedEditor {
    side: EditorSide,
    editor: WeakEntity<Editor>,
    _actions: Vec<Subscription>,
}

/// The texts GitHub's line numbers refer to.
struct ReferenceTexts {
    head_commit: Oid,
    /// The file at the pull request's head; `None` when it's deleted there.
    head: Option<String>,
    /// The file at the commits outdated threads were made on.
    originals: HashMap<Oid, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Placement {
    thread_id: String,
    side: EditorSide,
    /// The thread's last row in the view's version of the file (or its old version).
    row: u32,
    on_old_text: bool,
    rows_above: u32,
    above_first_row: bool,
    outdated: bool,
}

struct PlacedBlock {
    editor: WeakEntity<Editor>,
    block: CustomBlockId,
}

struct Draft {
    view: Entity<NewCommentView>,
    block: Option<PlacedBlock>,
}

/// Connects a pull request review to the editors of the Compare diff view showing one of its
/// files: draws the file's threads and lets new comments be written on its lines.
pub(crate) struct ReviewEditorBinding {
    context: ReviewFileContext,
    splittable: WeakEntity<SplittableEditor>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    workspace: WeakEntity<Workspace>,
    language_registry: Arc<LanguageRegistry>,
    texts: Option<ReferenceTexts>,
    /// View rows of either side that GitHub accepts comments on.
    commentable_rows: HashSet<(DiffSide, u32)>,
    editors: Vec<AttachedEditor>,
    lhs_editor_id: Option<EntityId>,
    placements: Vec<Placement>,
    /// Where each placed thread's block is attached; anchors follow edits, rows don't.
    thread_anchors: HashMap<String, Anchor>,
    blocks: Vec<PlacedBlock>,
    thread_views: HashMap<String, Entity<ThreadView>>,
    draft: Option<Draft>,
    load_task: Task<()>,
    commentable_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewEditorBinding {
    pub(crate) fn new(
        context: ReviewFileContext,
        splittable: &Entity<SplittableEditor>,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(
                &context.review,
                |this, _, _: &PullRequestReviewEvent, cx| this.reload(cx),
            ),
            cx.observe(splittable, |this, splittable, cx| {
                let lhs_editor_id = splittable.read(cx).lhs_editor().map(|lhs| lhs.entity_id());
                if lhs_editor_id != this.lhs_editor_id {
                    this.lhs_editor_id = lhs_editor_id;
                    this.attach_editors(cx);
                    // Splitting hides the removed lines that left-side threads may sit under.
                    this.place_blocks(true, cx);
                }
            }),
            cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.schedule_commentable_rows_update(cx);
                }
            }),
        ];
        let lhs_editor_id = splittable.read(cx).lhs_editor().map(|lhs| lhs.entity_id());
        let mut this = Self {
            context,
            splittable: splittable.downgrade(),
            buffer,
            diff,
            workspace,
            language_registry,
            texts: None,
            commentable_rows: HashSet::default(),
            editors: Vec::new(),
            lhs_editor_id,
            placements: Vec::new(),
            thread_anchors: HashMap::default(),
            blocks: Vec::new(),
            thread_views: HashMap::default(),
            draft: None,
            load_task: Task::ready(()),
            commentable_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.attach_editors(cx);
        this.reload(cx);
        this
    }

    /// Removes everything this binding added to the editors.
    pub(crate) fn detach(&mut self, cx: &mut Context<Self>) {
        self.remove_blocks(cx);
        self.close_draft(cx);
        for attached in self.editors.drain(..) {
            if let Some(editor) = attached.editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    editor.set_diff_review_provider(None, cx);
                    editor.unregister_addon::<PullRequestReviewAddon>();
                });
            }
        }
    }

    fn rhs_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        Some(self.splittable.upgrade()?.read(cx).rhs_editor().clone())
    }

    fn lhs_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.splittable.upgrade()?.read(cx).lhs_editor().cloned()
    }

    fn editor_for_side(&self, side: EditorSide, cx: &App) -> Option<Entity<Editor>> {
        match side {
            EditorSide::Rhs => self.rhs_editor(cx),
            EditorSide::Lhs => self.lhs_editor(cx),
        }
    }

    fn attach_editors(&mut self, cx: &mut Context<Self>) {
        let rhs = self.rhs_editor(cx);
        let lhs = self.lhs_editor(cx);
        self.editors.retain(|attached| {
            let current = match attached.side {
                EditorSide::Rhs => rhs.as_ref(),
                EditorSide::Lhs => lhs.as_ref(),
            };
            current.is_some_and(|editor| editor.entity_id() == attached.editor.entity_id())
        });
        for (side, editor) in [(EditorSide::Rhs, rhs), (EditorSide::Lhs, lhs)] {
            let Some(editor) = editor else {
                continue;
            };
            if self.editors.iter().any(|attached| attached.side == side) {
                continue;
            }
            self.attach(side, editor, cx);
        }
    }

    fn attach(&mut self, side: EditorSide, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let binding = cx.entity().downgrade();
        let provider = Arc::new(ReviewRowsProvider {
            binding: binding.clone(),
            side,
        });
        let actions = editor.update(cx, |editor, cx| {
            editor.set_diff_review_provider(Some(provider), cx);
            editor.register_addon(PullRequestReviewAddon);
            vec![
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &AddPullRequestComment, window, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.comment_at_cursor(side, window, cx)
                            })
                            .log_err();
                    }
                }),
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &TogglePullRequestThreadResolved, _, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.toggle_resolved_at_cursor(side, cx)
                            })
                            .log_err();
                    }
                }),
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &EditPullRequestComment, window, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.change_own_comment_at_cursor(
                                    side,
                                    CommentChange::Edit,
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    }
                }),
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &DeletePullRequestComment, window, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.change_own_comment_at_cursor(
                                    side,
                                    CommentChange::Delete,
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    }
                }),
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &NextPullRequestThread, window, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.go_to_thread(side, true, window, cx)
                            })
                            .log_err();
                    }
                }),
                editor.register_action({
                    let binding = binding.clone();
                    move |_: &PreviousPullRequestThread, window, cx| {
                        binding
                            .update(cx, |binding, cx| {
                                binding.go_to_thread(side, false, window, cx)
                            })
                            .log_err();
                    }
                }),
            ]
        });
        self.editors.push(AttachedEditor {
            side,
            editor: editor.downgrade(),
            _actions: actions,
        });
    }

    fn show_error(&self, error: impl Into<anyhow::Error>, cx: &mut App) {
        let error = error.into();
        self.workspace
            .update(cx, |workspace, cx| workspace.show_error(error, cx))
            .log_err();
    }

    /// Loads the texts GitHub's line numbers refer to, then places the threads.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let review = self.context.review.read(cx);
        let head_commit = review.details().head_ref_oid;
        let path = self.context.repo_path.clone();
        let original_commits = review
            .threads_for_path(&path)
            .filter(|thread| thread.is_outdated)
            .filter_map(|thread| thread.comments.first()?.original_commit)
            .collect::<HashSet<_>>();
        let loaded = self.texts.as_ref().is_some_and(|texts| {
            texts.head_commit == head_commit
                && original_commits
                    .iter()
                    .all(|commit| texts.originals.contains_key(commit))
        });
        if loaded {
            self.update_commentable_rows(cx);
            self.place_blocks(false, cx);
            return;
        }
        let head = review.load_text(head_commit, &path, cx);
        let originals = original_commits
            .into_iter()
            .map(|commit| (commit, review.load_text(commit, &path, cx)))
            .collect::<Vec<_>>();
        self.load_task = cx.spawn(async move |this, cx| {
            let head = head.await;
            let mut loaded_originals = HashMap::default();
            for (commit, text) in originals {
                loaded_originals.insert(commit, text.await);
            }
            this.update(cx, |this, cx| {
                this.texts = Some(ReferenceTexts {
                    head_commit,
                    head,
                    originals: loaded_originals,
                });
                this.update_commentable_rows(cx);
                this.place_blocks(false, cx);
            })
            .log_err();
        });
    }

    fn schedule_commentable_rows_update(&mut self, cx: &mut Context<Self>) {
        self.commentable_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| this.update_commentable_rows(cx))
                .log_err();
        });
    }

    fn update_commentable_rows(&mut self, cx: &mut Context<Self>) {
        self.commentable_rows.clear();
        let review = self.context.review.read(cx);
        let Some(commentable) = review
            .file(&self.context.repo_path)
            .and_then(|file| file.commentable.as_ref())
        else {
            return;
        };
        if let Some(head) = self.texts.as_ref().and_then(|texts| texts.head.as_deref()) {
            let view_text = self.buffer.read(cx).text();
            let map = LineMap::new(head, &view_text);
            let max_row = self.buffer.read(cx).max_point().row;
            for row in 0..=max_row {
                if let Some(head_row) = map.new_to_old(row).exact()
                    && commentable.contains(DiffSide::Right, head_row)
                {
                    self.commentable_rows.insert((DiffSide::Right, row));
                }
            }
        }
        if self.context.range == ReviewRange::All {
            let base_max_row = self
                .diff
                .read(cx)
                .base_text_buffer()
                .read(cx)
                .max_point()
                .row;
            for row in 0..=base_max_row {
                if commentable.contains(DiffSide::Left, row) {
                    self.commentable_rows.insert((DiffSide::Left, row));
                }
            }
        }
        cx.notify();
    }

    fn row_side(&self, row_info: &RowInfo, cx: &App) -> Option<(DiffSide, u32)> {
        let buffer_id = row_info.buffer_id?;
        let row = row_info.buffer_row?;
        if buffer_id == self.buffer.read(cx).remote_id() {
            Some((DiffSide::Right, row))
        } else if buffer_id == self.diff.read(cx).base_text_buffer().read(cx).remote_id() {
            Some((DiffSide::Left, row))
        } else {
            None
        }
    }

    fn compute_placements(&self, cx: &App) -> Vec<Placement> {
        let Some(texts) = self.texts.as_ref() else {
            return Vec::new();
        };
        let review = self.context.review.read(cx);
        let view_text = self.buffer.read(cx).text();
        let head_map = texts
            .head
            .as_deref()
            .map(|head| LineMap::new(head, &view_text));
        let show_all = self.context.range == ReviewRange::All;
        let is_split = self.lhs_editor(cx).is_some();
        let mut base_text = None::<String>;
        let mut base_map = None::<LineMap>;
        let mut placements = Vec::new();
        for thread in review.threads_for_path(&self.context.repo_path) {
            let rows_above = match (thread.start_line, thread.line) {
                (Some(start), Some(end)) if thread.start_diff_side.is_some() => {
                    end.saturating_sub(start)
                }
                _ => 0,
            };
            let mut placement = Placement {
                thread_id: thread.id.clone(),
                side: EditorSide::Rhs,
                row: 0,
                on_old_text: false,
                rows_above,
                above_first_row: false,
                outdated: thread.is_outdated,
            };
            if thread.subject_type == SubjectType::File {
                placement.above_first_row = true;
                placements.push(placement);
                continue;
            }
            match thread.diff_side {
                DiffSide::Right => {
                    let row = if let (false, Some(line), Some(head_map)) =
                        (thread.is_outdated, thread.line, head_map.as_ref())
                    {
                        let mapped = head_map.old_to_new(line.saturating_sub(1));
                        if show_all {
                            Some(mapped.nearest())
                        } else {
                            mapped.exact()
                        }
                    } else if show_all {
                        self.outdated_row(thread, DiffSide::Right, &view_text, texts)
                    } else {
                        None
                    };
                    let Some(row) = row else {
                        continue;
                    };
                    placement.row = row;
                }
                DiffSide::Left => {
                    if !show_all {
                        continue;
                    }
                    let base_text = base_text.get_or_insert_with(|| {
                        self.diff.read(cx).base_text_string(cx).unwrap_or_default()
                    });
                    let base_row = match (thread.is_outdated, thread.line) {
                        (false, Some(line)) => Some(line.saturating_sub(1)),
                        _ => self.outdated_row(thread, DiffSide::Left, base_text, texts),
                    };
                    let Some(base_row) = base_row else {
                        continue;
                    };
                    if is_split {
                        placement.side = EditorSide::Lhs;
                        placement.row = base_row;
                        placement.on_old_text = true;
                    } else if self.removed_row_anchor(base_row, cx).is_some() {
                        placement.row = base_row;
                        placement.on_old_text = true;
                    } else {
                        // An unchanged line: show the thread under its new-side copy.
                        let map =
                            base_map.get_or_insert_with(|| LineMap::new(base_text, &view_text));
                        placement.row = map.old_to_new(base_row).nearest();
                    }
                }
            }
            placements.push(placement);
        }
        placements
    }

    fn outdated_row(
        &self,
        thread: &ReviewThread,
        side: DiffSide,
        text: &str,
        texts: &ReferenceTexts,
    ) -> Option<u32> {
        let original_row = thread.original_line?.saturating_sub(1);
        let comment = thread.comments.first()?;
        let mapped = match side {
            DiffSide::Right => comment
                .original_commit
                .and_then(|commit| texts.originals.get(&commit)?.as_deref())
                .map(|original| {
                    LineMap::new(original, text)
                        .old_to_new(original_row)
                        .nearest()
                }),
            DiffSide::Left => None,
        };
        let max_row = u32::try_from(text.lines().count().saturating_sub(1)).unwrap_or(0);
        Some(
            mapped
                .or_else(|| locate_by_diff_hunk(&comment.diff_hunk, side, text, original_row))
                .unwrap_or(original_row)
                .min(max_row),
        )
    }

    /// The anchor below a removed line in the unified view, whose row is in the old text.
    fn removed_row_anchor(&self, base_row: u32, cx: &App) -> Option<Anchor> {
        let rhs = self.rhs_editor(cx)?;
        let base_buffer_id = self.diff.read(cx).base_text_buffer().read(cx).remote_id();
        let snapshot = rhs.read(cx).buffer().read(cx).snapshot(cx);
        let multibuffer_row = snapshot
            .row_infos(MultiBufferRow(0))
            .find(|info| {
                info.buffer_id == Some(base_buffer_id) && info.buffer_row == Some(base_row)
            })?
            .multibuffer_row?;
        Some(snapshot.anchor_after(Point::new(
            multibuffer_row.0,
            snapshot.line_len(multibuffer_row),
        )))
    }

    fn placement_anchor(&self, placement: &Placement, cx: &App) -> Option<Anchor> {
        let editor = self.editor_for_side(placement.side, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        if placement.above_first_row {
            return Some(snapshot.anchor_before(Point::zero()));
        }
        match (placement.side, placement.on_old_text) {
            (EditorSide::Rhs, true) => self.removed_row_anchor(placement.row, cx),
            (side, on_old_text) => {
                let buffer = if side == EditorSide::Lhs || on_old_text {
                    self.diff.read(cx).base_text_buffer().clone()
                } else {
                    self.buffer.clone()
                };
                let buffer = buffer.read(cx);
                let row = placement.row.min(buffer.max_point().row);
                let text_anchor = buffer.anchor_after(Point::new(row, buffer.line_len(row)));
                snapshot.anchor_in_buffer(text_anchor)
            }
        }
    }

    fn remove_blocks(&mut self, cx: &mut Context<Self>) {
        let mut by_editor =
            HashMap::<EntityId, (WeakEntity<Editor>, HashSet<CustomBlockId>)>::default();
        for placed in self.blocks.drain(..) {
            by_editor
                .entry(placed.editor.entity_id())
                .or_insert_with(|| (placed.editor.clone(), HashSet::default()))
                .1
                .insert(placed.block);
        }
        for (editor, blocks) in by_editor.into_values() {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| editor.remove_blocks(blocks, None, cx));
            }
        }
    }

    /// Draws a block under each thread. Blocks are only rebuilt when threads move.
    fn place_blocks(&mut self, force: bool, cx: &mut Context<Self>) {
        let placements = self.compute_placements(cx);
        let review = self.context.review.read(cx);
        let threads = review
            .threads_for_path(&self.context.repo_path)
            .map(|thread| (thread.id.clone(), thread.clone()))
            .collect::<HashMap<_, _>>();
        self.thread_views
            .retain(|thread_id, _| threads.contains_key(thread_id));
        for placement in &placements {
            let Some(thread) = threads.get(&placement.thread_id) else {
                continue;
            };
            let editor_focus = self
                .editor_for_side(placement.side, cx)
                .map(|editor| editor.focus_handle(cx).downgrade());
            match self.thread_views.get(&placement.thread_id) {
                Some(view) => view.update(cx, |view, cx| {
                    view.set_thread(thread.clone(), placement.outdated, editor_focus, cx)
                }),
                None => {
                    let review = self.context.review.downgrade();
                    let language_registry = self.language_registry.clone();
                    let thread = thread.clone();
                    let outdated = placement.outdated;
                    let view = cx.new(|cx| {
                        ThreadView::new(
                            review,
                            language_registry,
                            thread,
                            outdated,
                            editor_focus,
                            cx,
                        )
                    });
                    self.thread_views.insert(placement.thread_id.clone(), view);
                }
            }
        }
        if !force && placements == self.placements && !self.blocks.is_empty() {
            return;
        }
        self.remove_blocks(cx);
        self.thread_anchors.clear();
        let mut blocks_by_side = HashMap::<EditorSide, Vec<BlockProperties<Anchor>>>::default();
        for placement in &placements {
            let (Some(anchor), Some(view)) = (
                self.placement_anchor(placement, cx),
                self.thread_views.get(&placement.thread_id),
            ) else {
                continue;
            };
            self.thread_anchors
                .insert(placement.thread_id.clone(), anchor);
            let comment_count = threads
                .get(&placement.thread_id)
                .map_or(1, |thread| thread.comments.len());
            blocks_by_side
                .entry(placement.side)
                .or_default()
                .push(block_properties(
                    if placement.above_first_row {
                        BlockPlacement::Above(anchor)
                    } else {
                        BlockPlacement::Below(anchor)
                    },
                    u32::try_from(2 + comment_count * 3).unwrap_or(u32::MAX),
                    view.clone(),
                ));
        }
        for (side, blocks) in blocks_by_side {
            let Some(editor) = self.editor_for_side(side, cx) else {
                continue;
            };
            let ids = editor.update(cx, |editor, cx| editor.insert_blocks(blocks, None, cx));
            self.blocks.extend(ids.into_iter().map(|block| PlacedBlock {
                editor: editor.downgrade(),
                block,
            }));
        }
        self.placements = placements;
        // The draft's block was removed with the others when it lived on a removed editor.
        if let Some(draft) = &self.draft
            && draft
                .block
                .as_ref()
                .is_some_and(|block| block.editor.upgrade().is_none())
        {
            self.close_draft(cx);
        }
    }

    fn comment_at_cursor(&mut self, side: EditorSide, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor_for_side(side, cx) else {
            return;
        };
        let selection = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot)
        });
        let start = selection.start.row;
        let mut end = selection.end.row;
        // A line-wise selection ends at the start of the next line.
        if end > start && selection.end.column == 0 {
            end -= 1;
        }
        if start == end
            && let Some(thread_id) = self.thread_at_row(side, start, cx)
            && let Some(view) = self.thread_views.get(&thread_id).cloned()
        {
            view.update(cx, |view, cx| view.start_reply(window, cx));
            return;
        }
        // Leave visual mode, so returning to the diff doesn't extend the selection.
        if let Ok(action) = cx.build_action("vim::SwitchToNormalMode", None) {
            editor
                .focus_handle(cx)
                .dispatch_action(action.as_ref(), window, cx);
        }
        self.open_draft(side, editor, start..=end, window, cx);
    }

    fn open_draft_for_range(
        &mut self,
        side: EditorSide,
        editor: WeakEntity<Editor>,
        range: Range<Anchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = editor.upgrade() else {
            return;
        };
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let start = range.start.to_point(&snapshot).row;
        let end = range.end.to_point(&snapshot).row;
        self.open_draft(side, editor, start.min(end)..=start.max(end), window, cx);
    }

    fn open_draft(
        &mut self,
        side: EditorSide,
        editor: Entity<Editor>,
        rows: RangeInclusive<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = &self.draft {
            let draft_editor = draft.view.read(cx).editor.clone();
            if !draft_editor.read(cx).text(cx).trim().is_empty() {
                focus_composer(&draft_editor, window, cx);
                self.show_error(
                    anyhow!("Add or cancel the comment you're writing first"),
                    cx,
                );
                return;
            }
            self.close_draft(cx);
        }
        let (thread, label) = match self.comment_target(side, &editor, rows.clone(), cx) {
            Ok(target) => target,
            Err(error) => {
                self.show_error(error, cx);
                return;
            }
        };
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let end_row = MultiBufferRow(*rows.end());
        let anchor = snapshot.anchor_after(Point::new(end_row.0, snapshot.line_len(end_row)));
        let binding = cx.entity().downgrade();
        let review = self.context.review.downgrade();
        let editor_focus = editor.focus_handle(cx).downgrade();
        let language_registry = self.language_registry.clone();
        let view = cx.new(|cx| {
            NewCommentView::new(
                binding,
                review,
                thread,
                label,
                editor_focus,
                &language_registry,
                window,
                cx,
            )
        });
        let block = editor
            .update(cx, |editor, cx| {
                editor.insert_blocks(
                    [block_properties(
                        BlockPlacement::Below(anchor),
                        6,
                        view.clone(),
                    )],
                    Some(Autoscroll::fit()),
                    cx,
                )
            })
            .into_iter()
            .next()
            .map(|block| PlacedBlock {
                editor: editor.downgrade(),
                block,
            });
        let composer = view.read(cx).editor.clone();
        self.draft = Some(Draft { view, block });
        focus_composer(&composer, window, cx);
    }

    pub(crate) fn close_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        if let Some(block) = draft.block
            && let Some(editor) = block.editor.upgrade()
        {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(HashSet::from_iter([block.block]), None, cx)
            });
        }
    }

    /// Where a comment on the given rows of an editor goes on GitHub, with a label for it.
    fn comment_target(
        &self,
        side: EditorSide,
        editor: &Entity<Editor>,
        rows: RangeInclusive<u32>,
        cx: &App,
    ) -> Result<(NewThread, SharedString)> {
        let review = self.context.review.read(cx);
        let file = review
            .file(&self.context.repo_path)
            .context("This file isn't part of the pull request")?;
        let commentable = file
            .commentable
            .as_ref()
            .context("GitHub shows no diff for this file, so its lines can't be commented on")?;
        let texts = self
            .texts
            .as_ref()
            .context("The pull request's version of this file is still loading")?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let main_buffer_id = self.buffer.read(cx).remote_id();
        let view_text = self.buffer.read(cx).text();
        let head_map = texts
            .head
            .as_deref()
            .map(|head| LineMap::new(head, &view_text));
        let resolve = |multibuffer_row: u32| -> Result<(DiffSide, u32)> {
            let (buffer, point) = snapshot
                .point_to_buffer_point(Point::new(multibuffer_row, 0))
                .context("Select a line of the file")?;
            if side == EditorSide::Rhs && buffer.remote_id() == main_buffer_id {
                let head_row = head_map
                    .as_ref()
                    .and_then(|map| map.new_to_old(point.row).exact())
                    .with_context(|| match self.context.view_commit {
                        None => "This line was changed locally, so it isn't on GitHub",
                        Some(_) => "This line was changed by a later commit of the pull request",
                    })?;
                anyhow::ensure!(commentable.contains(DiffSide::Right, head_row), NOT_IN_DIFF);
                Ok((DiffSide::Right, head_row))
            } else {
                anyhow::ensure!(
                    self.context.range == ReviewRange::All,
                    "Removed lines can only be commented on while viewing all changes"
                );
                anyhow::ensure!(commentable.contains(DiffSide::Left, point.row), NOT_IN_DIFF);
                Ok((DiffSide::Left, point.row))
            }
        };
        let start = resolve(*rows.start())?;
        let end = resolve(*rows.end())?;
        if start.0 == DiffSide::Right && end.0 == DiffSide::Left {
            bail!("Select from removed lines down to added ones");
        }
        let side_label = |side: DiffSide| match side {
            DiffSide::Left => " (old)",
            DiffSide::Right => "",
        };
        let (thread_start, label) = if start == end {
            (
                None,
                format!("Comment on line {}{}", end.1 + 1, side_label(end.0)),
            )
        } else {
            (
                Some((start.0, start.1 + 1)),
                format!(
                    "Comment on lines {}{}–{}{}",
                    start.1 + 1,
                    side_label(start.0),
                    end.1 + 1,
                    side_label(end.0)
                ),
            )
        };
        Ok((
            NewThread {
                path: self.context.repo_path.as_unix_str().to_string(),
                body: String::new(),
                side: end.0,
                line: end.1 + 1,
                start: thread_start,
            },
            label.into(),
        ))
    }

    /// The thread whose lines include the row, in an editor's multibuffer rows.
    fn thread_at_row(&self, side: EditorSide, row: u32, cx: &App) -> Option<String> {
        let editor = self.editor_for_side(side, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        self.placements
            .iter()
            .filter(|placement| placement.side == side)
            .find(|placement| {
                self.thread_anchors
                    .get(&placement.thread_id)
                    .is_some_and(|anchor| {
                        let end = anchor.to_point(&snapshot).row;
                        let start = end.saturating_sub(placement.rows_above);
                        (start..=end).contains(&row)
                    })
            })
            .map(|placement| placement.thread_id.clone())
    }

    fn thread_view_at_cursor(
        &self,
        side: EditorSide,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ThreadView>> {
        let editor = self.editor_for_side(side, cx)?;
        let row = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head().row
        });
        let thread_id = self.thread_at_row(side, row, cx)?;
        self.thread_views.get(&thread_id).cloned()
    }

    fn toggle_resolved_at_cursor(&mut self, side: EditorSide, cx: &mut Context<Self>) {
        match self.thread_view_at_cursor(side, cx) {
            Some(view) => view.update(cx, |view, cx| view.toggle_resolved(cx)),
            None => self.show_error(anyhow!("There's no comment thread on this line"), cx),
        }
    }

    fn change_own_comment_at_cursor(
        &mut self,
        side: EditorSide,
        change: CommentChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.thread_view_at_cursor(side, cx) else {
            self.show_error(anyhow!("There's no comment thread on this line"), cx);
            return;
        };
        let result = view.update(cx, |view, cx| match change {
            CommentChange::Edit => view.edit_last_own(window, cx),
            CommentChange::Delete => view.delete_last_own(window, cx),
        });
        if let Err(error) = result {
            self.show_error(error, cx);
        }
    }

    fn go_to_thread(
        &mut self,
        side: EditorSide,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor_for_side(side, cx) else {
            return;
        };
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let mut rows = self
            .placements
            .iter()
            .filter(|placement| placement.side == side)
            .filter_map(|placement| {
                let end = self
                    .thread_anchors
                    .get(&placement.thread_id)?
                    .to_point(&snapshot)
                    .row;
                Some(end.saturating_sub(placement.rows_above))
            })
            .collect::<Vec<_>>();
        rows.sort_unstable();
        rows.dedup();
        let cursor_row = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head().row
        });
        let target = if forward {
            rows.into_iter().find(|row| *row > cursor_row)
        } else {
            rows.into_iter().rev().find(|row| *row < cursor_row)
        };
        let Some(target) = target else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let point = Point::new(target, 0);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([point..point]),
            );
        });
    }
}

#[cfg(test)]
impl ReviewEditorBinding {
    /// Each placed thread's id, whether it's on the split view's old side, and its row there.
    pub(crate) fn placed_threads(&self) -> Vec<(String, bool, u32)> {
        self.placements
            .iter()
            .map(|placement| {
                (
                    placement.thread_id.clone(),
                    placement.side == EditorSide::Lhs,
                    placement.row,
                )
            })
            .collect()
    }

    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn draft_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        Some(self.draft.as_ref()?.view.read(cx).editor.clone())
    }

    pub(crate) fn draft_label(&self, cx: &App) -> Option<SharedString> {
        Some(self.draft.as_ref()?.view.read(cx).label.clone())
    }

    pub(crate) fn draft_view(&self) -> Option<Entity<NewCommentView>> {
        Some(self.draft.as_ref()?.view.clone())
    }

    pub(crate) fn is_commentable(&self, side: DiffSide, row: u32) -> bool {
        self.commentable_rows.contains(&(side, row))
    }

    /// The comments of a thread the viewer can edit.
    pub(crate) fn editable_comment_ids(&self, thread_id: &str, cx: &App) -> Vec<String> {
        self.thread_views
            .get(thread_id)
            .map(|view| {
                view.read(cx)
                    .thread
                    .comments
                    .iter()
                    .filter(|comment| can_edit(comment))
                    .map(|comment| comment.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn editing_comment(
        &self,
        thread_id: &str,
        cx: &App,
    ) -> Option<(String, Entity<Editor>)> {
        let view = self.thread_views.get(thread_id)?.read(cx);
        match view.composer.as_ref()? {
            ThreadComposer {
                target: ComposerTarget::Edit { comment_id },
                editor,
            } => Some((comment_id.clone(), editor.clone())),
            ThreadComposer {
                target: ComposerTarget::Reply,
                ..
            } => None,
        }
    }

    pub(crate) fn is_replying(&self, thread_id: &str, cx: &App) -> bool {
        self.thread_views.get(thread_id).is_some_and(|view| {
            matches!(
                view.read(cx).composer,
                Some(ThreadComposer {
                    target: ComposerTarget::Reply,
                    ..
                })
            )
        })
    }
}

fn block_properties<V: Render>(
    placement: BlockPlacement<Anchor>,
    height: u32,
    view: Entity<V>,
) -> BlockProperties<Anchor> {
    BlockProperties {
        placement,
        height: Some(height),
        style: BlockStyle::Sticky,
        render: Arc::new(move |cx: &mut BlockContext| {
            div()
                .w_full()
                .pl(cx.margins.gutter.full_width())
                .pr_4()
                .py_1()
                .child(view.clone())
                .into_any_element()
        }),
        priority: 0,
    }
}

struct ReviewRowsProvider {
    binding: WeakEntity<ReviewEditorBinding>,
    side: EditorSide,
}

impl DiffReviewProvider for ReviewRowsProvider {
    fn can_review_row(&self, row_info: &RowInfo, cx: &App) -> bool {
        self.binding.upgrade().is_some_and(|binding| {
            let binding = binding.read(cx);
            binding
                .row_side(row_info, cx)
                .is_some_and(|row| binding.commentable_rows.contains(&row))
        })
    }

    fn review_rows(
        &self,
        editor: WeakEntity<Editor>,
        range: Range<Anchor>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let side = self.side;
        self.binding
            .update(cx, |binding, cx| {
                binding.open_draft_for_range(side, editor, range, window, cx)
            })
            .log_err();
    }
}

/// Adds the `pull_request_review` key context to the editors of a reviewed file.
struct PullRequestReviewAddon;

impl Addon for PullRequestReviewAddon {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add("pull_request_review");
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

fn composer_editor(
    placeholder: &str,
    text: &str,
    language_registry: &Arc<LanguageRegistry>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let editor = cx.new(|cx| {
        let mut editor = Editor::auto_height(3, 16, window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_show_gutter(false, cx);
        editor.set_use_modal_editing(true);
        editor.set_text(text, window, cx);
        editor
    });
    if let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() {
        let language = language_registry.language_for_name("Markdown");
        cx.spawn(async move |cx| {
            if let Some(language) = language.await.log_err() {
                buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            }
        })
        .detach();
    }
    editor
}

/// Focuses a composer and, with vim mode, starts typing in insert mode.
fn focus_composer(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let focus_handle = editor.focus_handle(cx);
    window.focus(&focus_handle, cx);
    // The vim action only reaches the editor once it has been rendered in its block.
    window.on_next_frame(move |window, cx| {
        if let Ok(action) = cx.build_action("vim::SwitchToInsertMode", None) {
            focus_handle.dispatch_action(action.as_ref(), window, cx);
        }
    });
}

fn return_focus(editor_focus: Option<&WeakFocusHandle>, window: &mut Window, cx: &mut App) {
    if let Some(focus_handle) = editor_focus.and_then(WeakFocusHandle::upgrade) {
        window.focus(&focus_handle, cx);
    }
}

/// Asks before discarding a non-empty composer, then runs `discard`.
fn confirm_discard(
    editor: &Entity<Editor>,
    window: &mut Window,
    cx: &mut App,
    discard: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    if editor.read(cx).text(cx).trim().is_empty() {
        discard(window, cx);
        return;
    }
    let answer = window.prompt(
        PromptLevel::Info,
        "Discard this comment?",
        None,
        &["Discard", "Keep Editing"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await == Ok(0) {
                cx.update(|window, cx| discard(window, cx)).log_err();
            }
        })
        .detach();
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

/// Only the viewer's own comments are edited or deleted, even where GitHub would allow more
/// (e.g. maintainers deleting others' comments).
fn can_edit(comment: &ReviewComment) -> bool {
    comment.viewer_did_author && comment.viewer_can_update
}

fn can_delete(comment: &ReviewComment) -> bool {
    comment.viewer_did_author && comment.viewer_can_delete
}

fn tag(label: &'static str, color: Color) -> Chip {
    Chip::new(label)
        .label_size(LabelSize::XSmall)
        .label_color(color)
}

enum ComposerTarget {
    Reply,
    Edit { comment_id: String },
}

struct ThreadComposer {
    target: ComposerTarget,
    editor: Entity<Editor>,
}

/// A review thread drawn under its line.
pub(crate) struct ThreadView {
    review: WeakEntity<PullRequestReview>,
    language_registry: Arc<LanguageRegistry>,
    thread: ReviewThread,
    outdated: bool,
    expanded: bool,
    bodies: Vec<(String, Entity<Markdown>)>,
    composer: Option<ThreadComposer>,
    editor_focus: Option<WeakFocusHandle>,
    pending_task: Option<Task<()>>,
    error: Option<SharedString>,
}

impl ThreadView {
    fn new(
        review: WeakEntity<PullRequestReview>,
        language_registry: Arc<LanguageRegistry>,
        thread: ReviewThread,
        outdated: bool,
        editor_focus: Option<WeakFocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            review,
            language_registry,
            thread: thread.clone(),
            outdated,
            expanded: !thread.is_resolved,
            bodies: Vec::new(),
            composer: None,
            editor_focus,
            pending_task: None,
            error: None,
        };
        this.set_thread(thread, outdated, this.editor_focus.clone(), cx);
        this
    }

    fn set_thread(
        &mut self,
        thread: ReviewThread,
        outdated: bool,
        editor_focus: Option<WeakFocusHandle>,
        cx: &mut Context<Self>,
    ) {
        if thread.is_resolved != self.thread.is_resolved {
            self.expanded = !thread.is_resolved;
        }
        let mut previous = std::mem::take(&mut self.bodies)
            .into_iter()
            .collect::<HashMap<_, _>>();
        self.bodies = thread
            .comments
            .iter()
            .map(|comment| {
                let markdown = match previous.remove(&comment.id) {
                    Some(markdown) if markdown.read(cx).source().as_ref() == comment.body => {
                        markdown
                    }
                    _ => {
                        let body = comment.body.clone();
                        let language_registry = self.language_registry.clone();
                        cx.new(|cx| Markdown::new(body.into(), Some(language_registry), None, cx))
                    }
                };
                (comment.id.clone(), markdown)
            })
            .collect();
        self.thread = thread;
        self.outdated = outdated;
        self.editor_focus = editor_focus;
        cx.notify();
    }

    fn start_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerTarget::Reply, "", window, cx);
    }

    fn start_edit(&mut self, comment_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let body = self
            .thread
            .comments
            .iter()
            .find(|comment| comment.id == comment_id)
            .map(|comment| comment.body.clone())
            .unwrap_or_default();
        self.open_composer(ComposerTarget::Edit { comment_id }, &body, window, cx);
    }

    fn open_composer(
        &mut self,
        target: ComposerTarget,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.expanded = true;
        if let Some(composer) = &self.composer
            && !composer.editor.read(cx).text(cx).trim().is_empty()
        {
            focus_composer(&composer.editor.clone(), window, cx);
            return;
        }
        let placeholder = match &target {
            ComposerTarget::Reply => "Reply… (ctrl-enter adds it to your review)",
            ComposerTarget::Edit { comment_id } if self.is_pending(comment_id) => {
                "Edit your pending comment… (ctrl-enter saves)"
            }
            ComposerTarget::Edit { .. } => "Edit your comment… (ctrl-enter saves it on GitHub)",
        };
        let editor = composer_editor(placeholder, text, &self.language_registry, window, cx);
        focus_composer(&editor, window, cx);
        self.composer = Some(ThreadComposer { target, editor });
        self.error = None;
        cx.notify();
    }

    fn submit(&mut self, _: &SubmitComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        let body = composer.editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() || self.pending_task.is_some() {
            return;
        }
        let Some(review) = self.review.upgrade() else {
            return;
        };
        let thread_id = self.thread.id.clone();
        let task = review.update(cx, |review, cx| match &composer.target {
            ComposerTarget::Reply => review.reply(thread_id, body, cx),
            ComposerTarget::Edit { comment_id } => {
                review.edit_comment(comment_id.clone(), body, cx)
            }
        });
        self.pending_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.pending_task = None;
                match result {
                    Ok(()) => {
                        let had_focus = this.composer.take().is_some_and(|composer| {
                            composer.editor.focus_handle(cx).is_focused(window)
                        });
                        if had_focus {
                            return_focus(this.editor_focus.as_ref(), window, cx);
                        }
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        let editor = composer.editor.clone();
        let this = cx.entity().downgrade();
        confirm_discard(&editor, window, cx, move |window, cx| {
            this.update(cx, |this, cx| {
                this.composer = None;
                this.error = None;
                return_focus(this.editor_focus.as_ref(), window, cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn toggle_resolved(&mut self, cx: &mut Context<Self>) {
        let resolve = !self.thread.is_resolved;
        let allowed = if resolve {
            self.thread.viewer_can_resolve
        } else {
            self.thread.viewer_can_unresolve
        };
        if !allowed {
            self.error = Some("You can't change whether this thread is resolved".into());
            cx.notify();
            return;
        }
        self.run_mutation(
            |review, thread_id, cx| review.set_thread_resolved(thread_id, resolve, cx),
            cx,
        );
    }

    fn is_pending(&self, comment_id: &str) -> bool {
        self.thread
            .comments
            .iter()
            .any(|comment| comment.id == comment_id && comment.is_pending())
    }

    /// Edits the viewer's latest comment in the thread.
    fn edit_last_own(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        let comment = self
            .thread
            .comments
            .iter()
            .rev()
            .find(|comment| can_edit(comment))
            .context("You have no comment in this thread to edit")?;
        self.start_edit(comment.id.clone(), window, cx);
        Ok(())
    }

    /// Deletes the viewer's latest comment in the thread, after confirming.
    fn delete_last_own(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        let comment = self
            .thread
            .comments
            .iter()
            .rev()
            .find(|comment| can_delete(comment))
            .context("You have no comment in this thread to delete")?;
        self.delete_comment(comment.id.clone(), window, cx);
        Ok(())
    }

    fn delete_comment(&mut self, comment_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let (message, detail) = if self.is_pending(&comment_id) {
            ("Delete this pending comment?", None)
        } else {
            (
                "Delete this comment from GitHub?",
                Some("Everyone on the pull request will stop seeing it."),
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            message,
            detail,
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.run_mutation(
                    move |review, _, cx| review.delete_comment(comment_id, cx),
                    cx,
                );
            })
            .log_err();
        })
        .detach();
    }

    fn run_mutation(
        &mut self,
        mutation: impl FnOnce(
            &mut PullRequestReview,
            String,
            &mut Context<PullRequestReview>,
        ) -> Task<Result<()>>,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.upgrade() else {
            return;
        };
        let thread_id = self.thread.id.clone();
        let task = review.update(cx, |review, cx| mutation(review, thread_id, cx));
        self.pending_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.pending_task = None;
                this.error = result.err().map(|error| format!("{error:#}").into());
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn render_comment(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let comment = self.thread.comments.get(index)?;
        let (_, markdown) = self.bodies.get(index)?;
        let editing = matches!(
            &self.composer,
            Some(ThreadComposer { target: ComposerTarget::Edit { comment_id }, .. }) if *comment_id == comment.id
        );
        let can_edit = can_edit(comment);
        let can_delete = can_delete(comment);
        let comment_id = comment.id.clone();
        let delete_id = comment.id.clone();
        Some(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Label::new(format!(
                                "@{}",
                                comment.author.as_deref().unwrap_or("ghost")
                            ))
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(relative_time(comment.created_at))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(comment.is_pending(), |row| {
                            row.child(tag("Pending", Color::Warning))
                        })
                        .child(div().flex_1())
                        .when(can_edit && !editing, |row| {
                            row.child(
                                IconButton::new(("edit-comment", index), IconName::Pencil)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Edit"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_edit(comment_id.clone(), window, cx);
                                    })),
                            )
                        })
                        .when(can_delete && !editing, |row| {
                            row.child(
                                IconButton::new(("delete-comment", index), IconName::Trash)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Delete"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_comment(delete_id.clone(), window, cx);
                                    })),
                            )
                        }),
                )
                .child(if editing {
                    self.render_composer(cx)
                } else {
                    MarkdownElement::new(markdown.clone(), editor::hover_markdown_style(window, cx))
                        .into_any_element()
                })
                .into_any_element(),
        )
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(composer) = &self.composer else {
            return div().into_any_element();
        };
        let submit_label = match composer.target {
            ComposerTarget::Reply => "Add Reply",
            ComposerTarget::Edit { .. } => "Save",
        };
        render_composer_box(
            &composer.editor,
            submit_label,
            self.pending_task.is_some(),
            cx.listener(|this, _, window, cx| this.submit(&SubmitComment, window, cx)),
            cx.listener(|this, _, window, cx| this.cancel(&CancelComment, window, cx)),
            cx,
        )
        .key_context("PullRequestComposer")
        .on_action(cx.listener(Self::submit))
        .on_action(cx.listener(Self::cancel))
        .into_any_element()
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let thread = &self.thread;
        let comment_count = thread.comments.len();
        let collapsed = thread.is_resolved && !self.expanded;
        let first_author = thread
            .comments
            .first()
            .and_then(|comment| comment.author.clone())
            .unwrap_or_default();
        let replying = matches!(
            self.composer,
            Some(ThreadComposer {
                target: ComposerTarget::Reply,
                ..
            })
        );
        let can_resolve = if thread.is_resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        };
        v_flex()
            .w_full()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .when(self.outdated, |this| this.opacity(0.75))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Chat)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(if collapsed {
                            format!(
                                "@{first_author} · {comment_count} comment{}",
                                if comment_count == 1 { "" } else { "s" }
                            )
                        } else {
                            format!(
                                "{comment_count} comment{}",
                                if comment_count == 1 { "" } else { "s" }
                            )
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when(self.outdated, |row| {
                        row.child(tag("Outdated", Color::Muted))
                    })
                    .when(thread.is_resolved, |row| {
                        row.child(tag("Resolved", Color::Success))
                    })
                    .child(div().flex_1())
                    .when(can_resolve, |row| {
                        row.child(
                            Button::new(
                                "toggle-resolved",
                                if thread.is_resolved {
                                    "Unresolve"
                                } else {
                                    "Resolve"
                                },
                            )
                            .label_size(LabelSize::Small)
                            .disabled(self.pending_task.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_resolved(cx))),
                        )
                    })
                    .when(thread.is_resolved, |row| {
                        row.child(
                            IconButton::new(
                                "toggle-expanded",
                                if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                },
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if collapsed { "Show" } else { "Hide" }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.expanded = !this.expanded;
                                cx.notify();
                            })),
                        )
                    }),
            )
            .when(!collapsed, |this| {
                this.children(
                    (0..comment_count).filter_map(|index| self.render_comment(index, window, cx)),
                )
                .child(if replying {
                    self.render_composer(cx)
                } else if thread.viewer_can_reply {
                    h_flex()
                        .child(
                            Button::new("reply", "Reply")
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::ReplyArrowRight).size(IconSize::Small),
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.start_reply(window, cx)),
                                ),
                        )
                        .into_any_element()
                } else {
                    div().into_any_element()
                })
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }
}

fn render_composer_box(
    editor: &Entity<Editor>,
    submit_label: &'static str,
    busy: bool,
    on_submit: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::Div {
    let colors = cx.theme().colors();
    v_flex()
        .gap_1p5()
        .child(
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border_focused)
                .bg(colors.editor_background)
                .child(editor.clone()),
        )
        .child(
            h_flex()
                .gap_1()
                .justify_end()
                .child(
                    Button::new("cancel-comment", "Cancel")
                        .label_size(LabelSize::Small)
                        .on_click(on_cancel),
                )
                .child(
                    Button::new("submit-comment", submit_label)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Filled)
                        .disabled(busy)
                        .on_click(on_submit),
                ),
        )
}

/// The composer of a new thread, drawn under the commented lines.
pub(crate) struct NewCommentView {
    binding: WeakEntity<ReviewEditorBinding>,
    review: WeakEntity<PullRequestReview>,
    thread: NewThread,
    label: SharedString,
    editor: Entity<Editor>,
    editor_focus: WeakFocusHandle,
    pending_task: Option<Task<()>>,
    error: Option<SharedString>,
}

impl NewCommentView {
    fn new(
        binding: WeakEntity<ReviewEditorBinding>,
        review: WeakEntity<PullRequestReview>,
        thread: NewThread,
        label: SharedString,
        editor_focus: WeakFocusHandle,
        language_registry: &Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = composer_editor(
            "Leave a comment… (ctrl-enter adds it to your review)",
            "",
            language_registry,
            window,
            cx,
        );
        Self {
            binding,
            review,
            thread,
            label,
            editor,
            editor_focus,
            pending_task: None,
            error: None,
        }
    }

    fn submit(&mut self, _: &SubmitComment, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() || self.pending_task.is_some() {
            return;
        }
        let Some(review) = self.review.upgrade() else {
            return;
        };
        let thread = NewThread {
            body,
            ..self.thread.clone()
        };
        let task = review.update(cx, |review, cx| review.add_thread(thread, cx));
        self.pending_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.pending_task = None;
                match result {
                    Ok(()) => {
                        let had_focus = this.editor.focus_handle(cx).is_focused(window);
                        if had_focus && let Some(focus) = this.editor_focus.upgrade() {
                            window.focus(&focus, cx);
                        }
                        this.binding
                            .update(cx, |binding, cx| binding.close_draft(cx))
                            .log_err();
                    }
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

    fn cancel(&mut self, _: &CancelComment, window: &mut Window, cx: &mut Context<Self>) {
        let binding = self.binding.clone();
        let editor_focus = self.editor_focus.clone();
        confirm_discard(&self.editor, window, cx, move |window, cx| {
            return_focus(Some(&editor_focus), window, cx);
            binding
                .update(cx, |binding, cx| binding.close_draft(cx))
                .log_err();
        });
    }
}

impl Render for NewCommentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .key_context("PullRequestComposer")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .child(
                Label::new(self.label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(render_composer_box(
                &self.editor,
                "Add to Review",
                self.pending_task.is_some(),
                cx.listener(|this, _, window, cx| this.submit(&SubmitComment, window, cx)),
                cx.listener(|this, _, window, cx| this.cancel(&CancelComment, window, cx)),
                cx,
            ))
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }
}

impl gpui::Focusable for NewCommentView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
