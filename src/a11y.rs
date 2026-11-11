//! accessibility tree for the title bar and focused terminal pane

use accesskit::{Action, HasPopup, Live, Node, NodeId, Orientation, Rect, Role, TreeId, TreeInfo, TreeUpdate};

use crate::term::Terminal;

const ROOT: NodeId = NodeId(1);
const TERMINAL: NodeId = NodeId(2);
const TITLE_BAR: NodeId = NodeId(3);
const TAB_LIST: NodeId = NodeId(4);
const TAB_PANEL: NodeId = NodeId(5);
const TAB_BASE: u64 = 1 << 48;
const LAYER_BASE: u64 = 1 << 47;

/// overlay item ids above any row index, shared by every layer
pub const PANEL: u32 = u32::MAX;
pub const INPUT: u32 = u32::MAX - 1;
pub const LIST: u32 = u32::MAX - 2;
pub const STATUS: u32 = u32::MAX - 3;
pub const CLOSE: u32 = u32::MAX - 4;

/// an overlay drawn over the panes; item ids are only unique within one
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Find = 1,
    Market,
    Palette,
    Menu,
    Dialog,
    Notice,
}

impl Layer {
    fn from_u8(n: u8) -> Option<Self> {
        [Self::Find, Self::Market, Self::Palette, Self::Menu, Self::Dialog, Self::Notice]
            .into_iter()
            .find(|layer| *layer as u8 == n)
    }
}

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
    Item(Layer, u32),
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
            Self::Item(layer, id) => NodeId(LAYER_BASE | (layer as u64) << 32 | id as u64),
        }
    }
}

pub fn target_for_node(id: NodeId) -> Option<Target> {
    match id.0 {
        id if id & TAB_BASE != 0 => Some(Target::Tab(id & !TAB_BASE)),
        id if id & LAYER_BASE != 0 => Layer::from_u8((id >> 32) as u8).map(|layer| Target::Item(layer, id as u32)),
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

/// one node of an overlay; `active` names the item a composite input (the
/// palette query) treats as focused
#[derive(Default)]
pub struct Item {
    pub id: u32,
    pub role: Role,
    pub label: String,
    pub value: Option<String>,
    pub description: Option<String>,
    pub shortcut: Option<String>,
    pub bounds: Option<Rect>,
    pub selected: Option<bool>,
    pub toggled: Option<bool>,
    pub click: bool,
    pub modal: bool,
    pub live: bool,
    pub position: Option<(usize, usize)>,
    pub active: Option<u32>,
    pub children: Vec<Item>,
}

pub struct Panel {
    pub layer: Layer,
    pub root: Item,
    pub focus: Option<u32>,
}

pub fn rect((x, y, w, h): (f32, f32, f32, f32)) -> Rect {
    Rect::new(x as f64, y as f64, (x + w) as f64, (y + h) as f64)
}

fn push_item(layer: Layer, item: Item, nodes: &mut Vec<(NodeId, Node)>) -> NodeId {
    let id = Target::Item(layer, item.id).node_id();
    let mut node = Node::new(item.role);
    if !item.label.is_empty() {
        node.set_label(item.label);
    }
    if let Some(value) = item.value {
        node.set_value(value);
    }
    if let Some(description) = item.description {
        node.set_description(description);
    }
    if let Some(shortcut) = item.shortcut.filter(|s| !s.is_empty()) {
        node.set_keyboard_shortcut(shortcut);
    }
    if let Some(bounds) = item.bounds {
        node.set_bounds(bounds);
    }
    if let Some(selected) = item.selected {
        node.set_selected(selected);
    }
    if let Some(toggled) = item.toggled {
        node.set_toggled(toggled.into());
    }
    if item.click {
        node.add_action(Action::Click);
    }
    if item.modal {
        node.set_modal();
    }
    if item.live {
        node.set_live(Live::Polite);
    }
    if let Some((position, size)) = item.position {
        node.set_position_in_set(position);
        node.set_size_of_set(size);
    }
    if let Some(active) = item.active {
        node.set_active_descendant(Target::Item(layer, active).node_id());
    }
    let children: Vec<NodeId> = item.children.into_iter().map(|child| push_item(layer, child, nodes)).collect();
    node.set_children(children);
    nodes.push((id, node));
    id
}

/// `panels` run bottom to top; the topmost panel that asks for focus takes it
/// from the terminal
#[allow(clippy::too_many_arguments)]
pub fn build_tree(
    text: &str,
    label: &str,
    window_bounds: Option<Rect>,
    terminal_bounds: Option<Rect>,
    title_bar_bounds: Option<Rect>,
    tabs: Vec<TabInfo>,
    controls: Vec<ControlInfo>,
    panels: Vec<Panel>,
) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label(label.to_string());
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
    let mut focus = TERMINAL;
    let mut top = vec![TITLE_BAR, TAB_PANEL];
    for panel in panels {
        if let Some(id) = panel.focus {
            focus = Target::Item(panel.layer, id).node_id();
        }
        top.push(push_item(panel.layer, panel.root, &mut nodes));
    }
    nodes[0].1.set_children(top);

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus,
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
            Vec::new(),
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
        assert_eq!(update.nodes[0].1.children(), [TITLE_BAR, TAB_PANEL]);
    }

    fn dialog(focus: Option<u32>) -> Panel {
        Panel {
            layer: Layer::Dialog,
            root: Item {
                id: PANEL,
                role: Role::AlertDialog,
                label: "Close 2 tabs?".to_string(),
                modal: true,
                children: vec![
                    Item { id: 0, role: Role::Button, label: "cancel".to_string(), click: true, ..Default::default() },
                    Item { id: 1, role: Role::DefaultButton, label: "close".to_string(), click: true, ..Default::default() },
                ],
                ..Default::default()
            },
            focus,
        }
    }

    fn tree(panels: Vec<Panel>) -> TreeUpdate {
        build_tree("", "termie", None, None, None, Vec::new(), Vec::new(), panels)
    }

    fn node(update: &TreeUpdate, id: NodeId) -> &Node {
        &update.nodes.iter().find(|(n, _)| *n == id).expect("node in tree").1
    }

    #[test]
    fn overlay_panel_hangs_off_the_window_and_takes_focus() {
        let update = tree(vec![dialog(Some(1))]);
        let panel = Target::Item(Layer::Dialog, PANEL).node_id();
        let default = Target::Item(Layer::Dialog, 1).node_id();
        assert_eq!(update.nodes[0].1.children(), [TITLE_BAR, TAB_PANEL, panel]);
        assert_eq!(update.focus, default);
        let root = node(&update, panel);
        assert_eq!(root.role(), Role::AlertDialog);
        assert!(root.is_modal());
        assert_eq!(root.children().len(), 2);
        assert!(node(&update, default).supports_action(Action::Click));
        assert_eq!(target_for_node(default), Some(Target::Item(Layer::Dialog, 1)));
    }

    #[test]
    fn topmost_focusing_panel_wins_and_notices_leave_focus_alone() {
        let notice = Panel {
            layer: Layer::Notice,
            root: Item { id: PANEL, role: Role::Status, label: "copied".to_string(), live: true, ..Default::default() },
            focus: None,
        };
        let find = Panel {
            layer: Layer::Find,
            root: Item {
                id: PANEL,
                role: Role::Group,
                children: vec![Item { id: INPUT, role: Role::SearchInput, value: Some("foo".to_string()), ..Default::default() }],
                ..Default::default()
            },
            focus: Some(INPUT),
        };
        let update = tree(vec![find, dialog(Some(1)), notice]);
        assert_eq!(update.focus, Target::Item(Layer::Dialog, 1).node_id());
        let status = node(&update, Target::Item(Layer::Notice, PANEL).node_id());
        assert_eq!(status.live(), Some(Live::Polite));
        assert_eq!(update.nodes[0].1.children().len(), 5);
        assert_eq!(tree(vec![dialog(None)]).focus, TERMINAL);
    }

    #[test]
    fn listbox_options_carry_position_and_active_descendant() {
        let options = (3..5)
            .map(|i| Item {
                id: i,
                role: Role::ListBoxOption,
                label: format!("row {i}"),
                selected: Some(i == 4),
                position: Some((i as usize + 1, 20)),
                click: true,
                ..Default::default()
            })
            .collect();
        let palette = Panel {
            layer: Layer::Palette,
            root: Item {
                id: PANEL,
                role: Role::Dialog,
                children: vec![
                    Item { id: INPUT, role: Role::EditableComboBox, active: Some(4), ..Default::default() },
                    Item { id: LIST, role: Role::ListBox, children: options, ..Default::default() },
                ],
                ..Default::default()
            },
            focus: Some(INPUT),
        };
        let update = tree(vec![palette]);
        let input = node(&update, Target::Item(Layer::Palette, INPUT).node_id());
        assert_eq!(input.active_descendant(), Some(Target::Item(Layer::Palette, 4).node_id()));
        let option = node(&update, Target::Item(Layer::Palette, 4).node_id());
        assert_eq!((option.position_in_set(), option.size_of_set()), (Some(5), Some(20)));
        assert_eq!(option.is_selected(), Some(true));
        assert_eq!(target_for_node(Target::Item(Layer::Palette, 4).node_id()), Some(Target::Item(Layer::Palette, 4)));
        assert_eq!(target_for_node(Target::Item(Layer::Notice, CLOSE).node_id()), Some(Target::Item(Layer::Notice, CLOSE)));
        assert_eq!(target_for_node(NodeId(LAYER_BASE | 99 << 32)), None);
    }
}
