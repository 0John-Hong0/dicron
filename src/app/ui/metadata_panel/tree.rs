//! Visible rows of the metadata hierarchy; search expands matches without changing user state.

use std::collections::HashSet;

use crate::dicom::MetadataItem;

pub(super) struct VisibleRow<'a> {
    pub(super) item: &'a MetadataItem,
    pub(super) depth: usize,
    pub(super) expanded: bool,
}

pub(super) fn visible_rows<'a>(
    items: &'a [MetadataItem],
    expanded_paths: &HashSet<String>,
    query: &str,
) -> Vec<VisibleRow<'a>> {
    let mut rows = Vec::new();
    if query.is_empty() {
        append_expanded(items, expanded_paths, 0, &mut rows);
    } else {
        for item in items {
            append_matches(item, query, false, 0, &mut rows);
        }
    }
    rows
}

fn append_expanded<'a>(
    items: &'a [MetadataItem],
    expanded_paths: &HashSet<String>,
    depth: usize,
    rows: &mut Vec<VisibleRow<'a>>,
) {
    for item in items {
        let expanded = !item.children.is_empty() && expanded_paths.contains(&item.path);
        rows.push(VisibleRow {
            item,
            depth,
            expanded,
        });
        if expanded {
            append_expanded(&item.children, expanded_paths, depth + 1, rows);
        }
    }
}

fn append_matches<'a>(
    item: &'a MetadataItem,
    query: &str,
    parent_matches: bool,
    depth: usize,
    rows: &mut Vec<VisibleRow<'a>>,
) -> bool {
    let start = rows.len();
    let matches = parent_matches || item.matches_search(query);
    rows.push(VisibleRow {
        item,
        depth,
        expanded: !item.children.is_empty(),
    });
    let mut child_matches = false;
    for child in &item.children {
        child_matches |= append_matches(child, query, matches, depth + 1, rows);
    }
    if !matches && !child_matches {
        rows.truncate(start);
        return false;
    }
    true
}

pub(super) fn expand_all(items: &[MetadataItem], expanded_paths: &mut HashSet<String>) {
    for item in items {
        if !item.children.is_empty() {
            expanded_paths.insert(item.path.clone());
            expand_all(&item.children, expanded_paths);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::dicom::MetadataItemKind;

    pub(crate) fn fixture() -> Vec<MetadataItem> {
        let path = "(0008,1032)";
        let items = ["alpha", "deepneedle"]
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let item_path = format!("{path} / Item {}", index + 1);
                let leaf = MetadataItem::with_structure(
                    "(0008,0100)".to_owned(),
                    "CodeValue".to_owned(),
                    value.to_owned(),
                    format!("{item_path} / (0008,0100)"),
                    MetadataItemKind::Element,
                    Vec::new(),
                );
                MetadataItem::with_structure(
                    String::new(),
                    format!("Item {}", index + 1),
                    "1 element".to_owned(),
                    item_path,
                    MetadataItemKind::Item,
                    vec![leaf],
                )
            })
            .collect();
        vec![
            MetadataItem::with_structure(
                path.to_owned(),
                "SequenceRoot".to_owned(),
                "2 items".to_owned(),
                path.to_owned(),
                MetadataItemKind::Sequence,
                items,
            ),
            MetadataItem::new(
                "(0010,0010)".to_owned(),
                "PatientName".to_owned(),
                "Unrelated".to_owned(),
            ),
        ]
    }

    #[test]
    fn expansion_uses_the_full_item_path_and_collapse_restores_roots() {
        let items = fixture();
        let mut expanded = HashSet::new();
        assert_eq!(visible_rows(&items, &expanded, "").len(), 2);
        expanded.insert(items[0].path.clone());
        assert_eq!(visible_rows(&items, &expanded, "").len(), 4);
        expanded.insert(items[0].children[0].path.clone());
        let rows = visible_rows(&items, &expanded, "");
        assert!(rows.iter().any(|row| row.item.value == "alpha"));
        assert!(!rows.iter().any(|row| row.item.value == "deepneedle"));
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2, 1, 0]
        );
        expanded.clear();
        assert_eq!(visible_rows(&items, &expanded, "").len(), 2);
    }

    #[test]
    fn nested_search_shows_ancestors_and_keeps_manual_expansion_unchanged() {
        let items = fixture();
        let expanded = HashSet::new();
        let rows = visible_rows(&items, &expanded, "deepneedle");
        assert_eq!(
            rows.iter()
                .map(|row| row.item.description.as_str())
                .collect::<Vec<_>>(),
            ["SequenceRoot", "Item 2", "CodeValue"]
        );
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(rows[0].expanded && rows[1].expanded);
        assert!(expanded.is_empty());
        assert_eq!(visible_rows(&items, &expanded, "").len(), 2);
        assert!(visible_rows(&items, &expanded, "no such value").is_empty());
    }

    #[test]
    fn sequence_and_item_path_search_include_their_contents() {
        let items = fixture();
        let expanded = HashSet::new();
        assert_eq!(visible_rows(&items, &expanded, "sequenceroot").len(), 5);
        let rows = visible_rows(&items, &expanded, "(0008,1032) / item 1");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.last().unwrap().item.value, "alpha");
    }

    #[test]
    fn expand_all_reveals_every_item_in_order_and_keeps_empty_sequences_empty() {
        let mut items = fixture();
        items.push(MetadataItem::with_structure(
            "(0008,1110)".to_owned(),
            "EmptySequence".to_owned(),
            "0 items".to_owned(),
            "(0008,1110)".to_owned(),
            MetadataItemKind::Sequence,
            Vec::new(),
        ));
        let mut expanded = HashSet::new();
        expand_all(&items, &mut expanded);
        let rows = visible_rows(&items, &expanded, "");
        assert_eq!(rows.len(), 7);
        assert_eq!(expanded.len(), 3);
        assert!(!rows.last().unwrap().expanded);
        assert_eq!(
            rows.iter()
                .filter(|row| row.item.kind == MetadataItemKind::Item)
                .map(|row| row.item.description.as_str())
                .collect::<Vec<_>>(),
            ["Item 1", "Item 2"]
        );
    }
}
