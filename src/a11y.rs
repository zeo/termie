//! accessibility tree for the title bar and focused terminal pane

use accesskit::{Action, HasPopup, Node, NodeId, Orientation, Rect, Role, TreeId, TreeInfo, TreeUpdate};

use crate::term::Terminal;

const ROOT: NodeId = NodeId(1);
const TERMINAL: NodeId = NodeId(2);
const TITLE_BAR: NodeId = NodeId(3);
const TAB_LIST: NodeId = NodeId(4);
const TAB_PANEL: NodeId = NodeId(5);
const TAB_BASE: u64 = 1 << 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Tab(u64),
    NewTab,
    NewTabMenu,
    SplitVertical,
    SplitHorizontal,
    PaneMode,
    Settings,
    Minimize,
    Maximize,
    Close,
}

impl Target {
    fn node_id(self) -> NodeId {
        match self {
            Self::Tab(id) => NodeId(TAB_BASE | id),
            Self::NewTab => NodeId(10),
            Self::NewTabMenu => NodeId(11),
            Self::SplitVertical => NodeId(12),
            Self::SplitHorizontal => NodeId(13),
            Self::PaneMode => NodeId(14),
            Self::Settings => NodeId(15),
            Self::Minimize => NodeId(16),
            Self::Maximize => NodeId(17),
            Self::Close => NodeId(18),
        }
    }
}

pub fn target_for_node(id: NodeId) -> Option<Target> {
    match id.0 {
        id if id & TAB_BASE != 0 => Some(Target::Tab(id & !TAB_BASE)),
        10 => Some(Target::NewTab),
        11 => Some(Target::NewTabMenu),
        12 => Some(Target::SplitVertical),
        13 => Some(Target::SplitHorizontal),
        14 => Some(Target::PaneMode),
        15 => Some(Target::Settings),
        16 => Some(Target::Minimize),
        17 => Some(Target::Maximize),
        18 => Some(Target::Close),
        _ => None,
    }
}

pub struct TabInfo {
    pub id: u64,
    pub label: String,
    pub selected: bool,
    pub bounds: Rect,
}

pub struct ControlInfo {
    pub target: Target,
    pub label: String,
    pub bounds: Rect,
    pub toggled: Option<bool>,
    pub has_popup: bool,
}

pub fn build_tree(
    text: &str,
    label: &str,
    window_bounds: Option<Rect>,
    terminal_bounds: Option<Rect>,
    title_bar_bounds: Option<Rect>,
    tabs: Vec<TabInfo>,
    controls: Vec<ControlInfo>,
) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label(label.to_string());
    root.set_children(vec![TITLE_BAR, TAB_PANEL]);
    if let Some(bounds) = window_bounds {
        root.set_bounds(bounds);
    }

    let mut title_bar = Node::new(Role::TitleBar);
    title_bar.set_label("Window controls".to_string());
    title_bar.set_children(
        std::iter::once(TAB_LIST)
            .chain(controls.iter().map(|control| control.target.node_id()))
            .collect::<Vec<_>>(),
    );
    if let Some(bounds) = title_bar_bounds {
        title_bar.set_bounds(bounds);
    }

    let mut tab_list = Node::new(Role::TabList);
    tab_list.set_label("Tabs".to_string());
    tab_list.set_orientation(Orientation::Horizontal);
    tab_list.set_size_of_set(tabs.len());
    tab_list.set_children(tabs.iter().map(|tab| Target::Tab(tab.id).node_id()).collect::<Vec<_>>());

    let mut tab_panel = Node::new(Role::TabPanel);
    tab_panel.set_children([TERMINAL]);
    if let Some(tab) = tabs.iter().find(|tab| tab.selected) {
        tab_panel.set_labelled_by([Target::Tab(tab.id).node_id()]);
    }
    if let Some(bounds) = terminal_bounds {
        tab_panel.set_bounds(bounds);
    }

    let mut terminal = Node::new(Role::Terminal);
    terminal.set_label("Terminal".to_string());
    terminal.set_value(text.to_string());
    if let Some(bounds) = terminal_bounds {
        terminal.set_bounds(bounds);
    }

    let mut nodes = vec![
        (ROOT, root),
        (TITLE_BAR, title_bar),
        (TAB_LIST, tab_list),
        (TAB_PANEL, tab_panel),
        (TERMINAL, terminal),
    ];
    for (index, tab) in tabs.into_iter().enumerate() {
        let mut node = Node::new(Role::Tab);
        node.set_label(tab.label);
        node.set_selected(tab.selected);
        node.set_position_in_set(index + 1);
        node.set_bounds(tab.bounds);
        node.set_controls([TAB_PANEL]);
        node.add_action(Action::Click);
        nodes.push((Target::Tab(tab.id).node_id(), node));
    }
    for control in controls {
        let mut node = Node::new(Role::Button);
        node.set_label(control.label);
        node.set_bounds(control.bounds);
        node.add_action(Action::Click);
        if let Some(toggled) = control.toggled {
            node.set_toggled(toggled.into());
        }
        if control.has_popup {
            node.set_has_popup(HasPopup::Menu);
        }
        nodes.push((control.target.node_id(), node));
    }

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ROOT)),
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
        let update = build_tree(
            &flatten(&t),
            "termie",
            Some(Rect::new(0.0, 0.0, 100.0, 60.0)),
            Some(Rect::new(0.0, 10.0, 100.0, 60.0)),
            Some(Rect::new(0.0, 0.0, 100.0, 10.0)),
            vec![TabInfo {
                id: 42,
                label: "shell".to_string(),
                selected: true,
                bounds: Rect::new(10.0, 0.0, 50.0, 10.0),
            }],
            vec![ControlInfo {
                target: Target::PaneMode,
                label: "Pane mode".to_string(),
                bounds: Rect::new(50.0, 0.0, 60.0, 10.0),
                toggled: Some(true),
                has_popup: false,
            }],
        );
        // root, title bar, tab list, tab panel, terminal, tab, and pane mode button
        assert_eq!(update.nodes.len(), 7);
        assert_eq!(update.nodes[0].0, ROOT);
        assert_eq!(update.nodes[0].1.role(), Role::Window);
        assert_eq!(update.nodes[3].0, TAB_PANEL);
        assert_eq!(update.nodes[3].1.role(), Role::TabPanel);
        assert_eq!(update.nodes[4].0, TERMINAL);
        assert_eq!(update.nodes[4].1.role(), Role::Terminal);
        assert_eq!(update.focus, TERMINAL);
        // the terminal value carries the visible text (followed by blank rows)
        let value = update.nodes[4].1.value().unwrap_or_default();
        assert!(value.starts_with("hello"), "value was {value:?}");
        assert_eq!(target_for_node(Target::Tab(42).node_id()), Some(Target::Tab(42)));
        assert_eq!(target_for_node(Target::PaneMode.node_id()), Some(Target::PaneMode));
        assert!(update.nodes[5].1.supports_action(Action::Click));
        assert_eq!(update.nodes[5].1.is_selected(), Some(true));
        assert_eq!(update.nodes[6].1.toggled(), Some(accesskit::Toggled::True));
    }
}
