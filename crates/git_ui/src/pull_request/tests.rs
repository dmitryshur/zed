use std::{path::Path, sync::Arc, time::Duration};

use collections::HashMap;
use editor::{DiffViewStyle, Editor, SelectionEffects, scroll::Autoscroll};
use fs::FakeFs;
use futures::{AsyncReadExt as _, future::BoxFuture};
use gpui::{
    Entity, Focusable as _, TestAppContext, VisualTestContext,
    http_client::{AsyncBody, FakeHttpClient, HttpClient, Response},
};
use language::Point;
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use util::path;
use workspace::{MultiWorkspace, Workspace};

use super::{
    AddPullRequestComment, ReviewPullRequest,
    github::{CommandOutput, CommandRunner, DiffSide, GlobalCommandRunner},
    picker::{NextFilter, PullRequestPicker},
    threads::SubmitComment,
};
use crate::{branch_compare::CompareDiffView, git_panel::GitPanel};

const BASE_SHA: &str = "1111111111111111111111111111111111111111";
const HEAD_SHA: &str = "2222222222222222222222222222222222222222";
const LIB_BASE: &str = "fn a() {}\nfn b() {}\nfn old() {}\nfn d() {}\nfn e() {}\nfn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\n";
const LIB_HEAD: &str = "fn a() {}\nfn b() {}\nfn changed() {}\nfn d() {}\nfn e() {}\nfn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\n";
const LIB_PATCH: &str = "@@ -1,6 +1,6 @@\n fn a() {}\n fn b() {}\n-fn old() {}\n+fn changed() {}\n fn d() {}\n fn e() {}\n fn f() {}";

type Requests = Arc<Mutex<Vec<(String, Value)>>>;

struct FakeRunner {
    calls: Mutex<Vec<String>>,
    fs: Arc<FakeFs>,
    /// What `git worktree list --porcelain` prints.
    worktrees: Mutex<String>,
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        _working_directory: &Path,
        _environment: &HashMap<String, String>,
    ) -> BoxFuture<'static, anyhow::Result<CommandOutput>> {
        let line = format!("{program} {}", args.join(" "));
        self.calls.lock().push(line.clone());
        let (success, stdout) = match line.as_str() {
            "gh auth token --hostname github.com" => (true, "test-token\n".to_string()),
            "gh pr checkout 7 -R owner/repo" => {
                self.fs
                    .set_branch_name(Path::new(path!("/project/.git")), Some("feature"));
                (true, String::new())
            }
            "git symbolic-ref -q HEAD" => (true, "refs/heads/feature\n".to_string()),
            "git worktree list --porcelain" => (true, self.worktrees.lock().clone()),
            _ if line.starts_with("git cat-file -e ") => (true, String::new()),
            _ => (false, String::new()),
        };
        Box::pin(async move {
            Ok(CommandOutput {
                success,
                stdout,
                stderr: if success {
                    String::new()
                } else {
                    format!("unexpected command: {line}")
                },
            })
        })
    }
}

fn comment_json(id: &str, body: &str, author: &str, pending: bool) -> Value {
    json!({
        "id": id,
        "body": body,
        "author": { "login": author },
        "createdAt": "2026-10-09T10:00:00Z",
        "state": if pending { "PENDING" } else { "SUBMITTED" },
        "viewerDidAuthor": author == "me",
        "viewerCanUpdate": author == "me",
        "viewerCanDelete": author == "me",
        "diffHunk": "@@ -1,3 +1,3 @@\n fn a() {}\n fn b() {}\n+fn changed() {}",
        "originalCommit": { "oid": HEAD_SHA },
    })
}

fn thread_json(id: &str, path: &str, side: &str, line: u32, comments: Vec<Value>) -> Value {
    json!({
        "id": id,
        "isResolved": false,
        "isOutdated": false,
        "path": path,
        "diffSide": side,
        "startDiffSide": null,
        "line": line,
        "startLine": line,
        "originalLine": line,
        "originalStartLine": null,
        "subjectType": "LINE",
        "viewerCanResolve": true,
        "viewerCanUnresolve": false,
        "viewerCanReply": true,
        "comments": { "nodes": comments },
    })
}

fn details_json(threads: Vec<Value>) -> Value {
    let pending_count = threads
        .iter()
        .flat_map(|thread| {
            thread["comments"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|comment| comment["state"] == "PENDING")
        .count();
    let pending = if pending_count == 0 {
        json!([])
    } else {
        json!([{ "id": "PRR_1", "comments": { "totalCount": pending_count } }])
    };
    json!({ "data": {
        "viewer": { "login": "me" },
        "repository": { "pullRequest": {
            "id": "PR_7",
            "number": 7,
            "title": "Add feature",
            "url": "https://github.com/owner/repo/pull/7",
            "isDraft": false,
            "viewerDidAuthor": false,
            "author": { "login": "alice" },
            "baseRefName": "main",
            "baseRefOid": BASE_SHA,
            "headRefName": "feature",
            "headRefOid": HEAD_SHA,
            "commits": { "nodes": [{ "commit": {
                "oid": HEAD_SHA,
                "abbreviatedOid": "2222222",
                "messageHeadline": "Change things",
                "parents": { "nodes": [{ "oid": BASE_SHA }] },
            }}]},
            "pending": { "nodes": pending },
            "submitted": { "nodes": [] },
            "reviewThreads": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": threads,
            },
        }},
    }})
}

/// Answers like GitHub does, recording each request's operation and variables.
fn fake_github(threads: Arc<Mutex<Vec<Value>>>, requests: Requests) -> Arc<dyn HttpClient> {
    FakeHttpClient::create(move |request| {
        let threads = threads.clone();
        let requests = requests.clone();
        async move {
            let uri = request.uri().to_string();
            let mut body = Vec::new();
            request.into_body().read_to_end(&mut body).await?;
            let response = if uri.ends_with("/graphql") {
                let request = serde_json::from_slice::<Value>(&body)?;
                let operation = request["operationName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let variables = request["variables"].clone();
                requests.lock().push((operation.clone(), variables.clone()));
                match operation.as_str() {
                    "PullRequestSearch"
                        if variables["searchQuery"]
                            .as_str()
                            .is_some_and(|query| query.contains("assignee:@me")) =>
                    {
                        json!({ "data": { "search": { "nodes": [] } } })
                    }
                    "PullRequestSearch" => json!({ "data": { "search": { "nodes": [{
                        "number": 7,
                        "title": "Add feature",
                        "updatedAt": "2026-10-09T10:00:00Z",
                        "isDraft": false,
                        "author": { "login": "alice" },
                        "headRefName": "feature",
                    }]}}}),
                    "PullRequestDetails" => details_json(threads.lock().clone()),
                    "AddPullRequestReviewThread" => {
                        let thread = thread_json(
                            &format!("T{}", threads.lock().len() + 1),
                            variables["path"].as_str().unwrap_or_default(),
                            variables["side"].as_str().unwrap_or_default(),
                            u32::try_from(variables["line"].as_u64().unwrap_or_default())
                                .unwrap_or_default(),
                            vec![comment_json(
                                "C-new",
                                variables["body"].as_str().unwrap_or_default(),
                                "me",
                                true,
                            )],
                        );
                        threads.lock().push(thread.clone());
                        json!({ "data": { "addPullRequestReviewThread": { "thread": thread } } })
                    }
                    "SubmitPullRequestReview" => {
                        for thread in threads.lock().iter_mut() {
                            if let Some(comments) = thread["comments"]["nodes"].as_array_mut() {
                                for comment in comments {
                                    comment["state"] = json!("SUBMITTED");
                                }
                            }
                        }
                        json!({ "data": { "submitPullRequestReview": { "pullRequestReview": { "id": "PRR_1" } } } })
                    }
                    "UpdatePullRequestReviewComment" => {
                        let comment_id = variables["commentId"].clone();
                        for thread in threads.lock().iter_mut() {
                            for comment in thread["comments"]["nodes"]
                                .as_array_mut()
                                .into_iter()
                                .flatten()
                            {
                                if comment["id"] == comment_id {
                                    comment["body"] = variables["body"].clone();
                                }
                            }
                        }
                        json!({ "data": { "updatePullRequestReviewComment": { "pullRequestReviewComment": { "id": comment_id } } } })
                    }
                    "DeletePullRequestReviewComment" => {
                        let comment_id = variables["commentId"].clone();
                        let mut threads = threads.lock();
                        for thread in threads.iter_mut() {
                            if let Some(comments) = thread["comments"]["nodes"].as_array_mut() {
                                comments.retain(|comment| comment["id"] != comment_id);
                            }
                        }
                        threads.retain(|thread| {
                            thread["comments"]["nodes"]
                                .as_array()
                                .is_some_and(|comments| !comments.is_empty())
                        });
                        json!({ "data": { "deletePullRequestReviewComment": { "pullRequestReview": { "id": "PRR_0" } } } })
                    }
                    "ResolveReviewThread" => {
                        let thread_id = variables["threadId"].clone();
                        for thread in threads.lock().iter_mut() {
                            if thread["id"] == thread_id {
                                thread["isResolved"] = json!(true);
                            }
                        }
                        json!({ "data": { "resolveReviewThread": { "thread": { "id": thread_id } } } })
                    }
                    _ => json!({ "errors": [{ "message": format!("unexpected {operation}") }] }),
                }
            } else if uri.contains("/repos/owner/repo/pulls/7/files") {
                requests.lock().push(("files".to_string(), Value::Null));
                json!([
                    { "filename": "src/lib.rs", "status": "modified", "patch": LIB_PATCH },
                    {
                        "filename": "src/renamed.rs",
                        "previous_filename": "src/original.rs",
                        "status": "renamed",
                        "patch": "@@ -1,2 +1,2 @@\n fn moved() {}\n-fn before() {}\n+fn edited() {}",
                    },
                ])
            } else {
                json!({ "message": "Not Found" })
            };
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(response.to_string()))?)
        }
    })
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        language_model::init(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

struct ReviewTest {
    workspace: Entity<Workspace>,
    panel: Entity<GitPanel>,
    fs: Arc<FakeFs>,
    runner: Arc<FakeRunner>,
    requests: Requests,
}

async fn setup(
    style: DiffViewStyle,
    cx: &mut TestAppContext,
) -> (ReviewTest, &mut VisualTestContext) {
    init_test(cx);
    cx.update_global(|store: &mut SettingsStore, cx| {
        store.update_user_settings(cx, |settings| {
            settings.editor.diff_view_style = Some(style);
        });
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            ".git": {},
            "src": {
                "lib.rs": LIB_HEAD,
                "renamed.rs": "fn moved() {}\nfn edited() {}\n",
            },
        }),
    )
    .await;
    let dot_git = Path::new(path!("/project/.git"));
    fs.set_merge_base_content_for_repo(
        dot_git,
        &[
            ("src/lib.rs", LIB_BASE.into()),
            ("src/original.rs", "fn moved() {}\nfn before() {}\n".into()),
        ],
    );
    fs.set_head_for_repo(
        dot_git,
        &[
            ("src/lib.rs", LIB_HEAD.into()),
            ("src/renamed.rs", "fn moved() {}\nfn edited() {}\n".into()),
        ],
        HEAD_SHA,
    );
    fs.set_index_for_repo(
        dot_git,
        &[
            ("src/lib.rs", LIB_HEAD.into()),
            ("src/renamed.rs", "fn moved() {}\nfn edited() {}\n".into()),
        ],
    );
    fs.with_git_state(dot_git, true, |state| {
        state.refs.insert("refs/heads/main".into(), BASE_SHA.into());
        state.branches.insert("main".into());
        state.branches.insert("feature".into());
        state
            .remotes
            .insert("origin".into(), "git@github.com:owner/repo.git".into());
    })
    .expect("the repository should exist");
    fs.set_branch_name(dot_git, Some("main"));

    let runner = Arc::new(FakeRunner {
        calls: Mutex::default(),
        fs: fs.clone(),
        worktrees: Mutex::new(format!(
            "worktree {}\nHEAD {HEAD_SHA}\nbranch refs/heads/main\n",
            path!("/project")
        )),
    });
    let requests = Requests::default();
    let threads = Arc::new(Mutex::new(vec![
        thread_json(
            "T1",
            "src/lib.rs",
            "RIGHT",
            3,
            vec![
                comment_json("C1", "Why rename it?", "bob", false),
                comment_json("C3", "Because it changed", "me", false),
            ],
        ),
        thread_json(
            "T2",
            "src/lib.rs",
            "LEFT",
            3,
            vec![comment_json("C2", "Was this used?", "bob", false)],
        ),
    ]));
    cx.update(|cx| {
        cx.set_global(GlobalCommandRunner(runner.clone()));
        cx.set_http_client(fake_github(threads.clone(), requests.clone()));
    });

    let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
    cx.run_until_parked();
    let window_handle =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window_handle
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window should exist");
    let cx = VisualTestContext::from_window(window_handle.into(), cx).into_mut();
    let panel = workspace.update_in(cx, GitPanel::new_test);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx);
    });
    cx.run_until_parked();
    (
        ReviewTest {
            workspace,
            panel,
            fs,
            runner,
            requests,
        },
        cx,
    )
}

fn operations(requests: &Requests) -> Vec<String> {
    requests
        .lock()
        .iter()
        .map(|(operation, _)| operation.clone())
        .collect()
}

fn diff_view(test: &ReviewTest, cx: &mut VisualTestContext) -> Entity<CompareDiffView> {
    test.workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<CompareDiffView>(cx)
            .next()
            .expect("the review should show a diff")
    })
}

/// Opens the picker, picks #7 and waits for the checkout to be noticed.
fn start_review(test: &ReviewTest, cx: &mut VisualTestContext) {
    cx.dispatch_action(ReviewPullRequest);
    cx.run_until_parked();
    let picker = test
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<PullRequestPicker>(cx)
        })
        .expect("the pull request picker should open");
    let picker = picker.read_with(cx, |picker, _| picker.picker().clone());
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.matched_numbers()),
        [7]
    );
    picker.update_in(cx, |picker, window, cx| {
        picker::PickerDelegate::confirm(&mut picker.delegate, false, window, cx)
    });
    for _ in 0..5 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
    }
    cx.run_until_parked();
}

fn rhs_editor(view: &Entity<CompareDiffView>, cx: &mut VisualTestContext) -> Entity<Editor> {
    view.read_with(cx, |view, cx| view.editor().read(cx).rhs_editor().clone())
}

fn move_cursor(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::fit()),
            window,
            cx,
            |selections| selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]),
        );
    });
}

#[gpui::test]
async fn test_picker_filters_load_lazily(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    cx.dispatch_action(ReviewPullRequest);
    cx.run_until_parked();
    let picker = test
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<PullRequestPicker>(cx)
        })
        .expect("the pull request picker should open");
    let searches = |test: &ReviewTest| {
        test.requests
            .lock()
            .iter()
            .filter(|(operation, _)| operation == "PullRequestSearch")
            .map(|(_, variables)| {
                variables["searchQuery"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        searches(&test),
        ["repo:owner/repo is:pr is:open sort:updated-desc review-requested:@me"]
    );
    assert_eq!(
        test.runner.calls.lock().as_slice(),
        ["gh auth token --hostname github.com"]
    );

    picker.update_in(cx, |picker, window, cx| {
        window.focus(&picker.focus_handle(cx), cx);
    });
    cx.dispatch_action(NextFilter);
    cx.run_until_parked();
    assert_eq!(
        searches(&test)[1..],
        ["repo:owner/repo is:pr is:open sort:updated-desc author:@me"]
    );
    let inner = picker.read_with(cx, |picker, _| picker.picker().clone());
    assert_eq!(
        inner.read_with(cx, |picker, _| picker.delegate.filter()),
        super::github::PullRequestFilter::CreatedByMe
    );
    assert_eq!(
        inner.read_with(cx, |picker, _| picker.delegate.matched_numbers()),
        [7]
    );

    let filter =
        |cx: &mut VisualTestContext| inner.read_with(cx, |picker, _| picker.delegate.filter());
    cx.dispatch_action(NextFilter);
    cx.run_until_parked();
    assert_eq!(filter(cx), super::github::PullRequestFilter::AssignedToMe);
    assert_eq!(
        inner.read_with(cx, |picker, _| picker.delegate.matched_numbers()),
        Vec::<u64>::new()
    );
    assert!(
        cx.debug_bounds("pull-request-filters").is_some(),
        "the filters stay visible when a filter has no pull requests"
    );
    for _ in 0..2 {
        cx.dispatch_action(NextFilter);
        cx.run_until_parked();
    }
    assert_eq!(
        filter(cx),
        super::github::PullRequestFilter::AllOpen,
        "moving right stops at the last filter"
    );
    assert_eq!(searches(&test).len(), 4);
    for _ in 0..4 {
        cx.dispatch_action(super::picker::PreviousFilter);
        cx.run_until_parked();
    }
    assert_eq!(
        filter(cx),
        super::github::PullRequestFilter::ReviewRequested,
        "moving left stops at the first filter"
    );
    assert_eq!(
        searches(&test).len(),
        4,
        "returning to an already loaded filter doesn't search again"
    );
}

#[gpui::test]
async fn test_review_refuses_uncommitted_changes(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    test.fs.set_status_for_repo(
        Path::new(path!("/project/.git")),
        &[("src/lib.rs", git::status::StatusCode::Modified.worktree())],
    );
    cx.run_until_parked();
    start_review(&test, cx);
    assert!(
        !test
            .runner
            .calls
            .lock()
            .iter()
            .any(|call| call.starts_with("gh pr checkout")),
        "nothing is checked out over uncommitted changes"
    );
    assert!(
        test.workspace
            .read_with(cx, |workspace, cx| workspace
                .items_of_type::<CompareDiffView>(cx)
                .next())
            .is_none()
    );
}

#[gpui::test]
async fn test_review_flow(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    start_review(&test, cx);

    let calls = test.runner.calls.lock().clone();
    assert!(calls.contains(&"gh pr checkout 7 -R owner/repo".to_string()));
    assert!(operations(&test.requests).contains(&"PullRequestDetails".to_string()));

    let list = test
        .panel
        .read_with(cx, |panel, _| panel.compare_list().clone());
    let review = list
        .read_with(cx, |list, cx| list.pull_request_review(cx))
        .expect("the Compare tab should show the pull request");
    assert!(review.read_with(cx, |review, cx| review.is_on_branch(cx)));
    assert_eq!(
        list.read_with(cx, |list, _| list.entry_paths()),
        [
            ("src/lib.rs".to_string(), None),
            (
                "src/renamed.rs".to_string(),
                Some("src/original.rs".to_string())
            ),
        ],
        "GitHub's rename replaces git's deletion and addition"
    );

    let view = diff_view(&test, cx);
    assert_eq!(
        view.read_with(cx, |view, _| view
            .shown_path()
            .map(|path| path.as_unix_str().to_string())),
        Some("src/lib.rs".to_string())
    );
    let binding = view
        .read_with(cx, |view, _| view.review_binding().cloned())
        .expect("the shown file should be bound to the review");
    assert_eq!(
        binding.read_with(cx, |binding, _| binding.placed_threads()),
        [("T1".to_string(), false, 2), ("T2".to_string(), false, 2)],
        "the unified view shows the removed line's thread under it"
    );
    assert_eq!(binding.read_with(cx, |binding, _| binding.block_count()), 2);
    binding.read_with(cx, |binding, _| {
        assert!(binding.is_commentable(DiffSide::Right, 0));
        assert!(binding.is_commentable(DiffSide::Right, 5));
        assert!(
            !binding.is_commentable(DiffSide::Right, 6),
            "past the hunk's context"
        );
        assert!(binding.is_commentable(DiffSide::Left, 2));
    });

    // Comment on a context line of the hunk.
    let editor = rhs_editor(&view, cx);
    move_cursor(&editor, 0, cx);
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    let draft = binding
        .read_with(cx, |binding, cx| binding.draft_editor(cx))
        .expect("a comment box should open");
    assert_eq!(
        binding.read_with(cx, |binding, cx| binding.draft_label(cx)),
        Some("Comment on line 1".into())
    );
    draft.update_in(cx, |editor, window, cx| {
        editor.set_text("Looks good", window, cx)
    });
    let draft_view = binding
        .read_with(cx, |binding, _| binding.draft_view())
        .expect("a comment box should be open");
    draft_view.update_in(cx, |view, window, cx| {
        window.focus(&view.focus_handle(cx), cx);
    });
    cx.dispatch_action(SubmitComment);
    cx.run_until_parked();
    let added = test
        .requests
        .lock()
        .iter()
        .filter(|(operation, _)| operation == "AddPullRequestReviewThread")
        .map(|(_, variables)| variables.clone())
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0]["path"], "src/lib.rs");
    assert_eq!(added[0]["line"], 1);
    assert_eq!(added[0]["side"], "RIGHT");
    assert_eq!(added[0]["startLine"], Value::Null);
    assert_eq!(added[0]["body"], "Looks good");
    assert!(binding.read_with(cx, |binding, cx| binding.draft_editor(cx).is_none()));
    assert_eq!(
        binding
            .read_with(cx, |binding, _| binding.placed_threads())
            .len(),
        3,
        "the new thread is drawn after GitHub returns it"
    );
    assert_eq!(
        review.read_with(cx, |review, _| review.pending_comment_count()),
        1
    );

    // Lines are translated through local edits.
    let buffer = editor.read_with(cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .all_buffers()
            .into_iter()
            .find(|buffer| buffer.read(cx).file().is_some())
            .expect("the working-tree buffer should be shown")
    });
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// local\n")], None, cx)
    });
    cx.executor().advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    binding.read_with(cx, |binding, _| {
        assert!(!binding.is_commentable(DiffSide::Right, 0), "a local line");
        assert!(binding.is_commentable(DiffSide::Right, 3));
    });
    move_cursor(&editor, 0, cx);
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    assert!(
        binding.read_with(cx, |binding, cx| binding.draft_editor(cx).is_none()),
        "local lines can't be commented on"
    );
    // The unified view's rows: the local line, a, b, the removed line, changed, d, ….
    move_cursor(&editor, 5, cx);
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    assert_eq!(
        binding.read_with(cx, |binding, cx| binding.draft_label(cx)),
        Some("Comment on line 4".into()),
        "the edited buffer's fifth line is the pull request's fourth"
    );
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([Point::new(3, 0)..Point::new(4, 3)])
        });
    });
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    assert_eq!(
        binding.read_with(cx, |binding, cx| binding.draft_label(cx)),
        Some("Comment on lines 3 (old)–3".into()),
        "a selection from a removed line to its replacement spans both sides"
    );
    move_cursor(&editor, 3, cx);
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    assert!(binding.read_with(cx, |binding, cx| binding.is_replying("T2", cx)));
    move_cursor(&editor, 4, cx);
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    assert!(
        binding.read_with(cx, |binding, cx| binding.is_replying("T1", cx)),
        "commenting on a line with a thread replies to it"
    );
}

#[gpui::test]
async fn test_threads_follow_split_toggle(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Split, cx).await;
    cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(900.)));
    start_review(&test, cx);
    let view = diff_view(&test, cx);
    let binding = view
        .read_with(cx, |view, _| view.review_binding().cloned())
        .expect("the shown file should be bound to the review");
    let splittable = view.read_with(cx, |view, _| view.editor().clone());
    assert!(splittable.read_with(cx, |editor, _| editor.is_split()));
    assert_eq!(
        binding.read_with(cx, |binding, _| binding.placed_threads()),
        [("T1".to_string(), false, 2), ("T2".to_string(), true, 2)],
        "the old side's thread goes to the left editor"
    );

    splittable.update_in(cx, |editor, window, cx| {
        editor.toggle_split(&editor::ToggleSplitDiff, window, cx)
    });
    cx.run_until_parked();
    assert!(!splittable.read_with(cx, |editor, _| editor.is_split()));
    assert_eq!(
        binding.read_with(cx, |binding, _| binding.placed_threads()),
        [("T1".to_string(), false, 2), ("T2".to_string(), false, 2)]
    );
    assert_eq!(binding.read_with(cx, |binding, _| binding.block_count()), 2);

    splittable.update_in(cx, |editor, window, cx| {
        editor.toggle_split(&editor::ToggleSplitDiff, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        binding.read_with(cx, |binding, _| binding.placed_threads()),
        [("T1".to_string(), false, 2), ("T2".to_string(), true, 2)]
    );
    let lhs = splittable
        .read_with(cx, |editor, _| editor.lhs_editor().cloned())
        .expect("the view should be split again");
    move_cursor(&lhs, 2, cx);
    lhs.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(super::TogglePullRequestThreadResolved);
    cx.run_until_parked();
    assert!(
        operations(&test.requests).contains(&"ResolveReviewThread".to_string()),
        "the left editor's thread can be resolved from its line"
    );
}

#[gpui::test]
async fn test_submit_review(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    start_review(&test, cx);
    let view = diff_view(&test, cx);
    let binding = view
        .read_with(cx, |view, _| view.review_binding().cloned())
        .expect("the shown file should be bound to the review");
    let editor = rhs_editor(&view, cx);
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    move_cursor(&editor, 0, cx);
    cx.dispatch_action(AddPullRequestComment);
    cx.run_until_parked();
    let draft = binding
        .read_with(cx, |binding, cx| binding.draft_editor(cx))
        .expect("a comment box should open");
    draft.update_in(cx, |editor, window, cx| {
        editor.set_text("Nit", window, cx);
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(SubmitComment);
    cx.run_until_parked();

    cx.dispatch_action(super::SubmitPullRequestReview);
    cx.run_until_parked();
    let modal = test
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<super::submit_modal::SubmitReviewModal>(cx)
        })
        .expect("the submit modal should open");
    modal.update_in(cx, |modal, window, cx| {
        window.focus(&modal.focus_handle(cx), cx);
    });
    cx.simulate_input("Thanks!");
    cx.dispatch_action(SubmitComment);
    cx.run_until_parked();
    let submitted = test
        .requests
        .lock()
        .iter()
        .find(|(operation, _)| operation == "SubmitPullRequestReview")
        .map(|(_, variables)| variables.clone())
        .expect("the pending review should be submitted");
    assert_eq!(submitted["reviewId"], "PRR_1");
    assert_eq!(submitted["event"], "COMMENT");
    assert_eq!(submitted["body"], "Thanks!");
    assert!(
        test.workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<super::submit_modal::SubmitReviewModal>(cx)
            })
            .is_none(),
        "the modal closes once GitHub accepts the review"
    );
}

#[gpui::test]
fn test_keymaps_name_registered_actions(cx: &mut TestAppContext) {
    init_test(cx);
    let mut names = vec![
        "git::ReviewPullRequest".to_string(),
        "git::SubmitPullRequestReview".to_string(),
        "git::AddPullRequestComment".to_string(),
        "git::TogglePullRequestThreadResolved".to_string(),
        "git::EditPullRequestComment".to_string(),
        "git::DeletePullRequestComment".to_string(),
        "git::NextPullRequestThread".to_string(),
        "git::PreviousPullRequestThread".to_string(),
        "git::RefreshPullRequest".to_string(),
    ];
    for source in [
        include_str!("../../../../assets/keymaps/default-linux.json"),
        include_str!("../../../../assets/keymaps/default-macos.json"),
        include_str!("../../../../assets/keymaps/default-windows.json"),
        include_str!("../../../../assets/keymaps/vim.json"),
        include_str!("../../../../assets/keymaps/specific-overrides.json"),
        include_str!("../../../../assets/keymaps/specific-overrides-macos.json"),
    ] {
        let keymap = settings::KeymapFile::parse(source).expect("keymap should parse");
        for section in keymap
            .sections()
            .filter(|section| section.context.contains("PullRequest"))
        {
            for (_, action) in section.bindings() {
                let (name, _) = settings::KeymapFile::parse_action(action)
                    .expect("action should parse")
                    .expect("binding should have an action");
                names.push(name.to_string());
            }
        }
    }
    assert!(
        names.len() > 7,
        "the bundled keymaps bind pull request actions"
    );
    for name in names {
        assert!(
            cx.update(|cx| cx.build_action(&name, None)).is_ok(),
            "{name} should be a registered action"
        );
    }
}

#[gpui::test]
async fn test_edit_and_delete_own_published_comment(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    start_review(&test, cx);
    let view = diff_view(&test, cx);
    let binding = view
        .read_with(cx, |view, _| view.review_binding().cloned())
        .expect("the shown file should be bound to the review");
    assert_eq!(
        binding.read_with(cx, |binding, cx| binding.editable_comment_ids("T1", cx)),
        ["C3"],
        "only the viewer's own comments can be edited"
    );
    let requests_for = |operation: &str, test: &ReviewTest| {
        test.requests
            .lock()
            .iter()
            .filter(|(name, _)| name == operation)
            .map(|(_, variables)| variables.clone())
            .collect::<Vec<_>>()
    };

    // The unified view's rows: a, b, the removed line (T2's), changed (T1's), ….
    let editor = rhs_editor(&view, cx);
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    move_cursor(&editor, 2, cx);
    cx.dispatch_action(super::EditPullRequestComment);
    cx.run_until_parked();
    assert!(
        binding
            .read_with(cx, |binding, cx| binding.editing_comment("T2", cx))
            .is_none(),
        "someone else's thread has nothing of the viewer's to edit"
    );

    move_cursor(&editor, 3, cx);
    cx.dispatch_action(super::EditPullRequestComment);
    cx.run_until_parked();
    let (comment_id, composer) = binding
        .read_with(cx, |binding, cx| binding.editing_comment("T1", cx))
        .expect("the viewer's comment should open for editing");
    assert_eq!(comment_id, "C3");
    assert_eq!(
        composer.read_with(cx, |editor, cx| editor.text(cx)),
        "Because it changed"
    );
    composer.update_in(cx, |editor, window, cx| {
        editor.set_text("Because it changed upstream", window, cx);
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(SubmitComment);
    cx.run_until_parked();
    let updates = requests_for("UpdatePullRequestReviewComment", &test);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["commentId"], "C3");
    assert_eq!(updates[0]["body"], "Because it changed upstream");
    assert!(
        binding
            .read_with(cx, |binding, cx| binding.editing_comment("T1", cx))
            .is_none()
    );

    move_cursor(&editor, 0, cx);
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(super::DeletePullRequestComment);
    cx.run_until_parked();
    assert!(
        !cx.has_pending_prompt(),
        "a line without a thread deletes nothing"
    );

    move_cursor(&editor, 3, cx);
    cx.dispatch_action(super::DeletePullRequestComment);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt(), "deleting asks first");
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    let deletes = requests_for("DeletePullRequestReviewComment", &test);
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0]["commentId"], "C3");
    assert!(
        binding
            .read_with(cx, |binding, cx| binding.editable_comment_ids("T1", cx))
            .is_empty(),
        "the thread keeps only the other comments"
    );
}

#[gpui::test]
async fn test_review_refuses_branch_checked_out_in_another_worktree(cx: &mut TestAppContext) {
    let (test, cx) = setup(DiffViewStyle::Unified, cx).await;
    *test.runner.worktrees.lock() = format!(
        "worktree {}\nHEAD {HEAD_SHA}\nbranch refs/heads/main\n\nworktree {}\nHEAD {HEAD_SHA}\nbranch refs/heads/feature\n",
        path!("/project"),
        path!("/other-worktree"),
    );
    start_review(&test, cx);
    assert!(
        !test
            .runner
            .calls
            .lock()
            .iter()
            .any(|call| call.starts_with("gh pr checkout")),
        "git would refuse to check the branch out a second time"
    );
    assert!(
        test.workspace
            .read_with(cx, |workspace, cx| workspace
                .items_of_type::<CompareDiffView>(cx)
                .next())
            .is_none()
    );
}

#[test]
fn test_worktree_with_branch() {
    let porcelain = "worktree /work/samw\nHEAD 1111\nbranch refs/heads/feature/sp-12820\n\nworktree /work/samw-worktree\nHEAD 2222\ndetached\n\nworktree /work/samw-worktree-2\nHEAD 3333\nbranch refs/heads/feature/sp-12819\n";
    assert_eq!(
        super::worktree_with_branch(porcelain, "feature/sp-12819", Path::new("/work/samw")),
        Some(Path::new("/work/samw-worktree-2"))
    );
    assert_eq!(
        super::worktree_with_branch(
            porcelain,
            "feature/sp-12819",
            Path::new("/work/samw-worktree-2")
        ),
        None,
        "the current worktree having the branch is fine"
    );
    assert_eq!(
        super::worktree_with_branch(porcelain, "feature/sp-1281", Path::new("/work/samw")),
        None,
        "branch names must match exactly"
    );
}

#[test]
fn test_command_errors_show_git_reason() {
    let output = CommandOutput {
        success: false,
        stdout: String::new(),
        stderr: "fatal: 'feature/sp-12819' is already used by worktree at '/work/samw-worktree-2'\nfailed to run git: exit status 128\n".to_string(),
    };
    assert_eq!(
        output
            .into_stdout("`gh pr checkout 28511`")
            .map_err(|error| error.to_string()),
        Err("`gh pr checkout 28511` failed: 'feature/sp-12819' is already used by worktree at '/work/samw-worktree-2'".to_string())
    );
    let output = CommandOutput {
        success: false,
        stdout: String::new(),
        stderr: "could not resolve host\n".to_string(),
    };
    assert_eq!(
        output
            .into_stdout("`gh pr checkout 1`")
            .map_err(|error| error.to_string()),
        Err("`gh pr checkout 1` failed: could not resolve host".to_string())
    );
}
