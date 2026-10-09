use std::collections::BTreeMap;

use collections::HashSet;
use git::repository::RepoPath;
use gpui::SharedString;

use super::comparison::ComparisonEntry;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CompareRow {
    Directory {
        path: RepoPath,
        /// A chain of directories without files of their own is shown as one row, e.g.
        /// `crates/git_ui/src`, keyed by the last directory's path.
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    File {
        entry_index: usize,
        depth: usize,
    },
}

impl CompareRow {
    pub(super) fn depth(&self) -> usize {
        match self {
            CompareRow::Directory { depth, .. } | CompareRow::File { depth, .. } => *depth,
        }
    }
}

#[derive(Default)]
struct TreeNode {
    name: SharedString,
    path: Option<RepoPath>,
    children: BTreeMap<SharedString, TreeNode>,
    entry_indices: Vec<usize>,
}

/// Lays out `entries` (sorted by path) as a tree: directories before files, and the contents of
/// `collapsed` directories left out.
pub(super) fn build_rows(
    entries: &[ComparisonEntry],
    collapsed: &HashSet<RepoPath>,
    visible_entry_indices: Option<&HashSet<usize>>,
) -> Vec<CompareRow> {
    let mut root = TreeNode::default();
    for (entry_index, entry) in entries.iter().enumerate() {
        if visible_entry_indices.is_some_and(|indices| !indices.contains(&entry_index)) {
            continue;
        }
        let components = entry.repo_path.components().collect::<Vec<_>>();
        let directories = components
            .split_last()
            .map_or(&[][..], |(_, directories)| directories);
        let mut node = &mut root;
        let mut directory_path = String::new();
        for directory in directories {
            if !directory_path.is_empty() {
                directory_path.push('/');
            }
            directory_path.push_str(directory);
            let Ok(path) = RepoPath::new(&directory_path) else {
                break;
            };
            let name = SharedString::from(directory.to_string());
            node = node
                .children
                .entry(name.clone())
                .or_insert_with(|| TreeNode {
                    name,
                    path: Some(path),
                    ..TreeNode::default()
                });
        }
        node.entry_indices.push(entry_index);
    }

    let mut rows = Vec::new();
    flatten(&root, 0, collapsed, &mut rows);
    rows
}

fn flatten(
    node: &TreeNode,
    depth: usize,
    collapsed: &HashSet<RepoPath>,
    rows: &mut Vec<CompareRow>,
) {
    for child in node.children.values() {
        let (last_directory, name) = compact_directory_chain(child);
        let Some(path) = last_directory.path.clone() else {
            continue;
        };
        let expanded = !collapsed.contains(&path);
        rows.push(CompareRow::Directory {
            path,
            name,
            depth,
            expanded,
        });
        if expanded {
            flatten(last_directory, depth + 1, collapsed, rows);
        }
    }
    rows.extend(
        node.entry_indices
            .iter()
            .map(|&entry_index| CompareRow::File { entry_index, depth }),
    );
}

fn compact_directory_chain(mut node: &TreeNode) -> (&TreeNode, SharedString) {
    let mut names = vec![node.name.clone()];
    while node.entry_indices.is_empty()
        && node.children.len() == 1
        && let Some(child) = node.children.values().next()
    {
        names.push(child.name.clone());
        node = child;
    }
    (node, names.join("/").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_compare::comparison::OldSide;
    use git::status::FileStatus;
    use pretty_assertions::assert_eq;

    fn entries(paths: &[&str]) -> Vec<ComparisonEntry> {
        paths
            .iter()
            .map(|path| ComparisonEntry {
                repo_path: RepoPath::new(path).unwrap(),
                status: FileStatus::Untracked,
                old_side: OldSide::Absent,
            })
            .collect()
    }

    /// Renders rows as indented lines: directories end with `/`, `-` marks a collapsed one.
    fn render(rows: &[CompareRow], entries: &[ComparisonEntry]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth());
                match row {
                    CompareRow::Directory { name, expanded, .. } => {
                        format!("{indent}{name}/{}", if *expanded { "" } else { " -" })
                    }
                    CompareRow::File { entry_index, .. } => format!(
                        "{indent}{}",
                        entries[*entry_index]
                            .repo_path
                            .file_name()
                            .unwrap_or_default()
                    ),
                }
            })
            .collect()
    }

    #[test]
    fn test_build_rows() {
        let entries = entries(&[
            "README.md",
            "assets/keymaps/default.json",
            "assets/keymaps/vim.json",
            "crates/git_ui/src/branch_compare/compare_list.rs",
            "crates/git_ui/src/branch_compare/comparison.rs",
            "crates/git_ui/src/git_panel.rs",
            "crates/picker/src/picker.rs",
        ]);
        assert_eq!(
            render(&build_rows(&entries, &HashSet::default(), None), &entries),
            [
                "assets/keymaps/",
                "  default.json",
                "  vim.json",
                "crates/",
                "  git_ui/src/",
                "    branch_compare/",
                "      compare_list.rs",
                "      comparison.rs",
                "    git_panel.rs",
                "  picker/src/",
                "    picker.rs",
                "README.md",
            ]
        );

        let collapsed = HashSet::from_iter([
            RepoPath::new("crates/git_ui/src").unwrap(),
            RepoPath::new("assets/keymaps").unwrap(),
        ]);
        assert_eq!(
            render(&build_rows(&entries, &collapsed, None), &entries),
            [
                "assets/keymaps/ -",
                "crates/",
                "  git_ui/src/ -",
                "  picker/src/",
                "    picker.rs",
                "README.md",
            ]
        );
    }

    #[test]
    fn test_filtered_rows_keep_original_entry_indices() {
        let entries = entries(&[
            "README.md",
            "assets/keymaps/vim.json",
            "crates/git_ui/src/branch_compare/comparison.rs",
            "crates/picker/src/picker.rs",
        ]);
        let visible = HashSet::from_iter([2, 3]);
        let rows = build_rows(&entries, &HashSet::default(), Some(&visible));
        assert_eq!(
            render(&rows, &entries),
            [
                "crates/",
                "  git_ui/src/branch_compare/",
                "    comparison.rs",
                "  picker/src/",
                "    picker.rs",
            ]
        );
        assert_eq!(
            rows.iter()
                .filter_map(|row| match row {
                    CompareRow::File { entry_index, .. } => Some(*entry_index),
                    CompareRow::Directory { .. } => None,
                })
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(build_rows(&entries, &HashSet::default(), Some(&HashSet::default())).is_empty());
    }
}
