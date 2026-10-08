use anyhow::Result;
use async_recursion::async_recursion;
use futures::future::join_all;
use gpui::{AppContext as _, AsyncWindowContext, Axis, Entity, Task, WeakEntity};
use project::Project;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ui::{App, Window};
use util::ResultExt as _;

use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{
    ItemId, Member, Pane, PaneAxis, PaneGroup, SerializableItem as _, Workspace, WorkspaceDb,
    WorkspaceId,
};

use crate::{
    TerminalView, default_working_directory,
    terminal_panel::{TerminalPanel, new_terminal_pane},
};

pub(crate) fn serialize_tabs<'a>(
    tabs: impl Iterator<Item = (&'a PaneGroup, &'a Entity<Pane>)>,
    active_tab: usize,
    cx: &App,
) -> SerializedTabs {
    let mut serialized_tabs = Vec::new();
    let mut serialized_active_tab = 0;
    for (index, (pane_group, active_pane)) in tabs.enumerate() {
        let serialized_tab = build_serialized_pane_group(&pane_group.root, active_pane, cx);
        // Task terminals are not serialized, so a tab of only tasks has nothing to restore.
        if !serialized_tab.has_terminals() {
            continue;
        }
        serialized_tabs.push(serialized_tab);
        if index <= active_tab {
            serialized_active_tab = serialized_tabs.len() - 1;
        }
    }
    SerializedTabs {
        tabs: serialized_tabs,
        active_tab: serialized_active_tab,
    }
}

fn build_serialized_pane_group(
    pane_group: &Member,
    active_pane: &Entity<Pane>,
    cx: &App,
) -> SerializedPaneGroup {
    match pane_group {
        Member::Axis(PaneAxis {
            axis,
            members,
            flexes,
            bounding_boxes: _,
        }) => SerializedPaneGroup::Group {
            axis: SerializedAxis(*axis),
            children: members
                .iter()
                .map(|member| build_serialized_pane_group(member, active_pane, cx))
                .collect::<Vec<_>>(),
            flexes: Some(flexes.lock().clone()),
        },
        Member::Pane(pane_handle) => {
            SerializedPaneGroup::Pane(serialize_pane(pane_handle, pane_handle == active_pane, cx))
        }
    }
}

fn serialize_pane(pane: &Entity<Pane>, active: bool, cx: &App) -> SerializedPane {
    let pane = pane.read(cx);
    let children = pane
        .items()
        .filter_map(|item| {
            let terminal_view = item.act_as::<TerminalView>(cx)?;
            if terminal_view.read(cx).terminal().read(cx).task().is_some() {
                None
            } else {
                Some(item.item_id().as_u64())
            }
        })
        .collect::<Vec<_>>();
    let active_item = children.first().copied();
    SerializedPane {
        active,
        children,
        active_item,
        pinned_count: 0,
    }
}

/// Restores the serialized terminal panel and returns the number of restored terminals.
pub(crate) fn deserialize_terminal_panel(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    database_id: WorkspaceId,
    serialized_panel: SerializedTerminalPanel,
    terminal_panel: WeakEntity<TerminalPanel>,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<usize>> {
    window.spawn(cx, async move |cx| {
        let (restored_tabs, active_tab) = match serialized_panel.items {
            SerializedItems::NoSplits(item_ids) => {
                let active_index = serialized_panel
                    .active_item_id
                    .and_then(|active_id| item_ids.iter().position(|id| *id == active_id));
                deserialize_legacy_tabs(
                    workspace,
                    project,
                    terminal_panel.clone(),
                    database_id,
                    &item_ids,
                    active_index,
                    cx,
                )
                .await
            }
            SerializedItems::WithSplits(serialized_pane_group) => {
                let mut item_ids = Vec::new();
                let mut active_index = None;
                flatten_legacy_pane_group(&serialized_pane_group, &mut item_ids, &mut active_index);
                deserialize_legacy_tabs(
                    workspace,
                    project,
                    terminal_panel.clone(),
                    database_id,
                    &item_ids,
                    active_index,
                    cx,
                )
                .await
            }
            SerializedItems::WithTabs(serialized_tabs) => {
                let mut restored_tabs = Vec::new();
                let mut active_tab = 0;
                for (index, serialized_tab) in serialized_tabs.tabs.iter().enumerate() {
                    let Some((root, active_pane)) = deserialize_pane_group(
                        workspace.clone(),
                        project.clone(),
                        terminal_panel.clone(),
                        database_id,
                        serialized_tab,
                        false,
                        cx,
                    )
                    .await
                    else {
                        continue;
                    };
                    if index == serialized_tabs.active_tab {
                        active_tab = restored_tabs.len();
                    }
                    let pane_group = PaneGroup::with_root(root);
                    let active_pane = active_pane.unwrap_or_else(|| pane_group.first_pane());
                    restored_tabs.push((pane_group, active_pane));
                }
                (restored_tabs, active_tab)
            }
        };

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            terminal_panel.restore_tabs(restored_tabs, active_tab, window, cx)
        })
    })
}

/// Pre-tabs layouts are restored with every terminal in its own tab.
fn flatten_legacy_pane_group(
    serialized: &SerializedPaneGroup,
    item_ids: &mut Vec<u64>,
    active_index: &mut Option<usize>,
) {
    match serialized {
        SerializedPaneGroup::Pane(serialized_pane) => {
            if serialized_pane.active && !serialized_pane.children.is_empty() {
                let active_position = serialized_pane
                    .active_item
                    .and_then(|active_item| {
                        serialized_pane
                            .children
                            .iter()
                            .position(|item_id| *item_id == active_item)
                    })
                    .unwrap_or(0);
                *active_index = Some(item_ids.len() + active_position);
            }
            item_ids.extend(serialized_pane.children.iter().copied());
        }
        SerializedPaneGroup::Group { children, .. } => {
            for child in children {
                flatten_legacy_pane_group(child, item_ids, active_index);
            }
        }
    }
}

async fn deserialize_legacy_tabs(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    terminal_panel: WeakEntity<TerminalPanel>,
    workspace_id: WorkspaceId,
    item_ids: &[u64],
    active_index: Option<usize>,
    cx: &mut AsyncWindowContext,
) -> (Vec<(PaneGroup, Entity<Pane>)>, usize) {
    let terminal_views = deserialize_terminal_views(
        workspace_id,
        project.clone(),
        workspace.clone(),
        item_ids,
        cx,
    )
    .await;
    let mut restored_tabs = Vec::new();
    let mut active_tab = 0;
    for (index, terminal_view) in terminal_views.into_iter().enumerate() {
        let Some(terminal_view) = terminal_view else {
            continue;
        };
        let Some(pane) = new_pane_with_terminal(
            workspace.clone(),
            project.clone(),
            terminal_panel.clone(),
            terminal_view,
            cx,
        ) else {
            continue;
        };
        if Some(index) == active_index {
            active_tab = restored_tabs.len();
        }
        restored_tabs.push((PaneGroup::new(pane.clone()), pane));
    }
    (restored_tabs, active_tab)
}

fn new_pane_with_terminal(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    terminal_panel: WeakEntity<TerminalPanel>,
    terminal_view: Entity<TerminalView>,
    cx: &mut AsyncWindowContext,
) -> Option<Entity<Pane>> {
    terminal_panel
        .update_in(cx, |terminal_panel, window, cx| {
            let zoomed = terminal_panel.active_pane().read(cx).is_zoomed();
            let pane = new_terminal_pane(workspace, project, zoomed, window, cx);
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(terminal_view), true, false, None, window, cx);
            });
            pane
        })
        .log_err()
}

/// Restores one tab's split layout. `in_split` is set for panes that share the tab with
/// other panes: those get a fresh shell if their terminal can't be restored, so the
/// layout keeps its shape.
#[async_recursion(?Send)]
async fn deserialize_pane_group(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    terminal_panel: WeakEntity<TerminalPanel>,
    workspace_id: WorkspaceId,
    serialized: &SerializedPaneGroup,
    in_split: bool,
    cx: &mut AsyncWindowContext,
) -> Option<(Member, Option<Entity<Pane>>)> {
    match serialized {
        SerializedPaneGroup::Group {
            axis,
            flexes,
            children,
        } => {
            let children_in_split = in_split || children.len() > 1;
            let mut current_active_pane = None;
            let mut members = Vec::new();
            for child in children {
                if let Some((new_member, active_pane)) = deserialize_pane_group(
                    workspace.clone(),
                    project.clone(),
                    terminal_panel.clone(),
                    workspace_id,
                    child,
                    children_in_split,
                    cx,
                )
                .await
                {
                    members.push(new_member);
                    current_active_pane = current_active_pane.or(active_pane);
                }
            }

            if members.is_empty() {
                return None;
            }

            if members.len() == 1 {
                return Some((members.remove(0), current_active_pane));
            }

            Some((
                Member::Axis(PaneAxis::load(axis.0, members, flexes.clone())),
                current_active_pane,
            ))
        }
        SerializedPaneGroup::Pane(serialized_pane) => {
            let restored_view = match serialized_pane.children.first() {
                Some(item_id) => deserialize_terminal_views(
                    workspace_id,
                    project.clone(),
                    workspace.clone(),
                    &[*item_id],
                    cx,
                )
                .await
                .into_iter()
                .flatten()
                .next(),
                None => None,
            };
            let terminal_view = match restored_view {
                Some(terminal_view) => terminal_view,
                None if in_split => {
                    new_shell_terminal_view(workspace.clone(), project.clone(), workspace_id, cx)
                        .await?
                }
                None => return None,
            };
            let pane =
                new_pane_with_terminal(workspace, project, terminal_panel, terminal_view, cx)?;
            Some((
                Member::Pane(pane.clone()),
                serialized_pane.active.then_some(pane),
            ))
        }
    }
}

async fn new_shell_terminal_view(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    workspace_id: WorkspaceId,
    cx: &mut AsyncWindowContext,
) -> Option<Entity<TerminalView>> {
    let working_directory = workspace
        .update(cx, |workspace, cx| default_working_directory(workspace, cx))
        .ok()
        .flatten();
    let terminal = project
        .update(cx, |project, cx| {
            project.create_terminal_shell(working_directory, cx)
        })
        .await
        .log_err()?;
    cx.update(|window, cx| {
        cx.new(|cx| {
            TerminalView::new(
                terminal,
                workspace,
                Some(workspace_id),
                project.downgrade(),
                window,
                cx,
            )
        })
    })
    .log_err()
}

/// Returns one entry per item id, `None` where the terminal could not be restored.
fn deserialize_terminal_views(
    workspace_id: WorkspaceId,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    item_ids: &[u64],
    cx: &mut AsyncWindowContext,
) -> impl Future<Output = Vec<Option<Entity<TerminalView>>>> + use<> {
    let deserialized_items = item_ids
        .iter()
        .map(|item_id| {
            cx.update(|window, cx| {
                TerminalView::deserialize(
                    project.clone(),
                    workspace.clone(),
                    workspace_id,
                    *item_id,
                    window,
                    cx,
                )
            })
            .ok()
        })
        .collect::<Vec<_>>();
    join_all(
        deserialized_items
            .into_iter()
            .map(|item| async move { item?.await.log_err() }),
    )
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedTerminalPanel {
    pub items: SerializedItems,
    // A deprecated field, kept for backwards compatibility for the code before terminal splits were introduced.
    pub active_item_id: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SerializedItems {
    // The data stored before terminal splits were introduced.
    NoSplits(Vec<u64>),
    // The data stored before each tab got its own split layout.
    WithSplits(SerializedPaneGroup),
    WithTabs(SerializedTabs),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedTabs {
    pub tabs: Vec<SerializedPaneGroup>,
    pub active_tab: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SerializedPaneGroup {
    Pane(SerializedPane),
    Group {
        axis: SerializedAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<SerializedPaneGroup>,
    },
}

impl SerializedPaneGroup {
    fn has_terminals(&self) -> bool {
        match self {
            SerializedPaneGroup::Pane(serialized_pane) => !serialized_pane.children.is_empty(),
            SerializedPaneGroup::Group { children, .. } => {
                children.iter().any(SerializedPaneGroup::has_terminals)
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedPane {
    pub active: bool,
    pub children: Vec<u64>,
    pub active_item: Option<u64>,
    #[serde(default)]
    pub pinned_count: usize,
}

#[derive(Debug)]
pub(crate) struct SerializedAxis(pub Axis);

impl Serialize for SerializedAxis {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Axis::Horizontal => serializer.serialize_str("horizontal"),
            Axis::Vertical => serializer.serialize_str("vertical"),
        }
    }
}

impl<'de> Deserialize<'de> for SerializedAxis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "horizontal" => Ok(SerializedAxis(Axis::Horizontal)),
            "vertical" => Ok(SerializedAxis(Axis::Vertical)),
            invalid => Err(serde::de::Error::custom(format!(
                "Invalid axis value: '{invalid}'"
            ))),
        }
    }
}

pub struct TerminalDb(ThreadSafeConnection);

impl Domain for TerminalDb {
    const NAME: &str = stringify!(TerminalDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE terminals (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        // Remove the unique constraint on the item_id table
        // SQLite doesn't have a way of doing this automatically, so
        // we have to do this silly copying.
        sql!(
            CREATE TABLE terminals2 (
                workspace_id INTEGER,
                item_id INTEGER,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;

            INSERT INTO terminals2 (workspace_id, item_id, working_directory)
            SELECT workspace_id, item_id, working_directory FROM terminals;

            DROP TABLE terminals;

            ALTER TABLE terminals2 RENAME TO terminals;
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN working_directory_path TEXT;
            UPDATE terminals SET working_directory_path = CAST(working_directory AS TEXT);
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN custom_title TEXT;
        ),
    ];
}

db::static_connection!(TerminalDb, [WorkspaceDb]);

impl TerminalDb {
    query! {
       pub async fn update_workspace_id(
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            item_id: ItemId
        ) -> Result<()> {
            UPDATE terminals
            SET workspace_id = ?
            WHERE workspace_id = ? AND item_id = ?
        }
    }

    pub async fn save_working_directory(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: PathBuf,
    ) -> Result<()> {
        log::debug!(
            "Saving working directory {working_directory:?} for item {item_id} in workspace {workspace_id:?}"
        );
        let query =
            "INSERT INTO terminals(item_id, workspace_id, working_directory, working_directory_path)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT DO UPDATE SET
                item_id = ?1,
                workspace_id = ?2,
                working_directory = ?3,
                working_directory_path = ?4"
        ;
        self.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            next_index = statement.bind(&working_directory, next_index)?;
            statement.bind(
                &working_directory.to_string_lossy().into_owned(),
                next_index,
            )?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_working_directory(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
            SELECT working_directory
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_custom_title(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        custom_title: Option<String>,
    ) -> Result<()> {
        log::debug!(
            "Saving custom title {:?} for item {} in workspace {:?}",
            custom_title,
            item_id,
            workspace_id
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, custom_title)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    custom_title = excluded.custom_title";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&custom_title, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_custom_title(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT custom_title
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}
