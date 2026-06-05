//! minimal accessibility: expose the focused pane's visible text + a terminal
//! role to screen readers via accesskit. one window root with a single terminal
//! child; precise per-character caret is a documented v2 boundary

use accesskit::{Node, NodeId, Rect, Role, Tree, TreeId, TreeUpdate};

use crate::term::Terminal;

const ROOT: NodeId = NodeId(1);
const TERMINAL: NodeId = NodeId(2);

/// build the tree: a Window root containing one Terminal node whose value is the
/// focused pane's visible text
pub fn build_tree(text: &str, label: &str, bounds: Option<Rect>) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label(label.to_string());
    root.set_children(vec![TERMINAL]);

    let mut terminal = Node::new(Role::Terminal);
    terminal.set_value(text.to_string());
    if let Some(b) = bounds {
        terminal.set_bounds(b);
    }

    TreeUpdate {
        nodes: vec![(ROOT, root), (TERMINAL, terminal)],
        tree: Some(Tree::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: TERMINAL,
    }
}

/// the focused grid's visible rows as plain text, one row per line, trailing
/// blanks trimmed; grapheme clusters are emitted whole
pub fn flatten(term: &Terminal) -> String {
    let g = &term.grid;
    let mut out = String::new();
    for r in 0..g.rows {
        let line = g.line_at(r);
        let mut row = String::new();
        for cell in line.iter() {
            if cell.cluster != 0 {
                row.push_str(g.cluster_str(cell.cluster));
            } else if cell.c != '\0' {
                row.push(cell.c);
            }
        }
        while row.ends_with(' ') {
            row.pop();
        }
        out.push_str(&row);
        if r + 1 < g.rows {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    #[test]
    fn build_tree_exposes_window_terminal_and_text() {
        let mut t = Terminal::new(3, 10);
        let mut p = Parser::new();
        p.advance(&mut t, b"hello");
        let update = build_tree(&flatten(&t), "termie", Some(Rect::new(0.0, 0.0, 100.0, 60.0)));
        // root window + terminal child, focus on the terminal
        assert_eq!(update.nodes.len(), 2);
        assert_eq!(update.nodes[0].0, ROOT);
        assert_eq!(update.nodes[0].1.role(), Role::Window);
        assert_eq!(update.nodes[1].0, TERMINAL);
        assert_eq!(update.nodes[1].1.role(), Role::Terminal);
        assert_eq!(update.focus, TERMINAL);
        // the terminal value carries the visible text (followed by blank rows)
        let value = update.nodes[1].1.value().unwrap_or_default();
        assert!(value.starts_with("hello"), "value was {value:?}");
    }
}
