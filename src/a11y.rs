//! accessibility tree for the title bar and focused terminal pane

use std::time::{Duration, Instant};

use accesskit::{
    Action, HasPopup, Live, Node, NodeId, Orientation, Rect, Role, TextDirection, TextPosition, TextSelection, TreeId,
    TreeInfo, TreeUpdate,
};

use crate::grid::{Grid, Line};
use crate::term::Terminal;

const ROOT: NodeId = NodeId(1);
const TERMINAL: NodeId = NodeId(2);
const TITLE_BAR: NodeId = NodeId(3);
const TAB_LIST: NodeId = NodeId(4);
const TAB_PANEL: NodeId = NodeId(5);
const OUTPUT: NodeId = NodeId(6);
const TAB_BASE: u64 = 1 << 48;
const LAYER_BASE: u64 = 1 << 47;
const RUN_BASE: u64 = 1 << 46;
/// accesskit indexes a run's words with a u8, so a wide row splits into runs
/// of this many characters plus its line break
const RUN_CHARS: usize = 254;

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

/// `screen` None leaves the terminal's text runs as the last update sent
/// them; `output` is the live region's text. `panels` run bottom to top; the
/// topmost panel that asks for focus takes it from the terminal
#[allow(clippy::too_many_arguments)]
pub fn build_tree(
    screen: Option<&Screen>,
    output: &str,
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

    let mut live = Node::new(Role::Status);
    live.set_live(Live::Polite);
    if !output.is_empty() {
        live.set_label(output.to_string());
    }

    let mut nodes = vec![(ROOT, root), (TITLE_BAR, title_bar), (TAB_LIST, tab_list), (TAB_PANEL, tab_panel), (OUTPUT, live)];
    if let Some(screen) = screen {
        let mut terminal = Node::new(Role::Terminal);
        terminal.set_label("Terminal".to_string());
        terminal.set_text_direction(TextDirection::LeftToRight);
        if let Some(bounds) = terminal_bounds {
            terminal.set_bounds(bounds);
        }
        let runs = screen.runs();
        let at = |(row, index): (usize, usize)| {
            let chunk = (index / RUN_CHARS).min(screen.rows[row].chars.len().saturating_sub(1) / RUN_CHARS);
            TextPosition { node: run_id(row, chunk), character_index: index - chunk * RUN_CHARS }
        };
        terminal.set_text_selection(TextSelection { anchor: at(screen.anchor.unwrap_or(screen.caret)), focus: at(screen.caret) });
        terminal.set_children(runs.iter().map(|(id, _)| *id).collect::<Vec<_>>());
        nodes.push((TERMINAL, terminal));
        nodes.extend(runs);
    }
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
    let mut top = vec![TITLE_BAR, TAB_PANEL, OUTPUT];
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

fn run_id(row: usize, chunk: usize) -> NodeId {
    NodeId(RUN_BASE | (row as u64) << 8 | chunk as u64)
}

/// one character of a row: its utf-8 length, first column and cell width
struct Char {
    len: u8,
    col: usize,
    cells: usize,
}

/// a grid line as text: each grapheme cluster is one character and a wide
/// glyph spans its spacer cell
#[derive(Default)]
struct Row {
    text: String,
    chars: Vec<Char>,
}

impl Row {
    /// trailing blanks are dropped except before column `keep`, so a caret
    /// past the prompt's last glyph still lands on a real position
    fn new(g: &Grid, line: &Line, keep: usize) -> Self {
        let mut row = Row::default();
        for (col, cell) in line.iter().enumerate() {
            let start = row.text.len();
            if cell.cluster != 0 {
                let cluster = g.cluster_str(cell.cluster);
                let mut end = cluster.len().min(u8::MAX as usize);
                while !cluster.is_char_boundary(end) {
                    end -= 1;
                }
                row.text.push_str(&cluster[..end]);
            } else if cell.c != '\0' {
                row.text.push(cell.c);
            } else {
                if let Some(wide) = row.chars.last_mut() {
                    wide.cells += 1;
                }
                continue;
            }
            row.chars.push(Char { len: (row.text.len() - start) as u8, col, cells: 1 });
        }
        while let Some(last) = row.chars.last()
            && last.col >= keep
            && row.text.ends_with(' ')
            && last.len == 1
        {
            row.text.pop();
            row.chars.pop();
        }
        row
    }

    /// the character index a caret at `col` sits before
    fn index(&self, col: usize) -> usize {
        self.chars.partition_point(|c| c.col < col)
    }
}

/// the focused pane's visible rows, the caret and the selection anchor, each
/// as (row, character index)
pub struct Screen {
    rows: Vec<Row>,
    caret: (usize, usize),
    anchor: Option<(usize, usize)>,
    origin: (f64, f64),
    cell: (f64, f64),
}

impl Screen {
    /// `origin` is the grid's top-left and `cell` one cell's size, in window
    /// pixels; `selection` runs from its anchor to its focus as viewport
    /// (row, col) cells
    pub fn new(term: &Terminal, origin: (f64, f64), cell: (f64, f64), selection: Option<((usize, usize), (usize, usize))>) -> Self {
        let g = &term.grid;
        let last = g.rows.saturating_sub(1);
        let caret = ((g.cursor.row + g.view_offset).min(last), g.cursor.col);
        let rows: Vec<Row> = (0..g.rows)
            .map(|r| Row::new(g, g.line_at(r), if r == caret.0 { caret.1 } else { 0 }))
            .collect();
        let at = |(row, col): (usize, usize)| {
            let row = row.min(last);
            (row, rows[row].index(col))
        };
        let (caret, anchor) = match selection {
            Some((anchor, focus)) => (at(focus), Some(at(anchor))),
            None => (at(caret), None),
        };
        Screen { rows, caret, anchor, origin, cell }
    }

    /// every row as text runs: a line break ends all but the last row
    fn runs(&self) -> Vec<(NodeId, Node)> {
        let (cw, ch) = self.cell;
        let mut out = Vec::new();
        for (r, row) in self.rows.iter().enumerate() {
            let y0 = self.origin.1 + r as f64 * ch;
            let newline = r + 1 < self.rows.len();
            let chunks: Vec<&[Char]> = if row.chars.is_empty() { vec![&[]] } else { row.chars.chunks(RUN_CHARS).collect() };
            let mut byte = 0;
            let count = chunks.len();
            for (i, chars) in chunks.into_iter().enumerate() {
                let first = chars.first().map_or(0, |c| c.col);
                let end = chars.last().map_or(first, |c| c.col + c.cells);
                let len: usize = chars.iter().map(|c| c.len as usize).sum();
                let mut value = row.text[byte..byte + len].to_string();
                byte += len;
                let mut lengths: Vec<u8> = chars.iter().map(|c| c.len).collect();
                let mut positions: Vec<f32> = chars.iter().map(|c| ((c.col - first) as f64 * cw) as f32).collect();
                let mut widths: Vec<f32> = chars.iter().map(|c| (c.cells as f64 * cw) as f32).collect();
                let tail = i + 1 == count;
                if tail && newline {
                    value.push('\n');
                    lengths.push(1);
                    positions.push(((end - first) as f64 * cw) as f32);
                    widths.push(0.0);
                }
                let mut words = Vec::new();
                let (mut gap, mut off) = (i == 0, 0);
                for (k, c) in chars.iter().enumerate() {
                    let blank = value.as_bytes()[off] == b' ';
                    if gap && !blank {
                        words.push(k as u8);
                    }
                    gap = blank;
                    off += c.len as usize;
                }
                let mut node = Node::new(Role::TextRun);
                node.set_bounds(Rect::new(
                    self.origin.0 + first as f64 * cw,
                    y0,
                    self.origin.0 + end as f64 * cw,
                    y0 + ch,
                ));
                node.set_value(value);
                node.set_character_lengths(lengths);
                node.set_character_positions(positions);
                node.set_character_widths(widths);
                node.set_word_starts(words);
                node.set_text_direction(TextDirection::LeftToRight);
                if i > 0 {
                    node.set_previous_on_line(run_id(r, i - 1));
                }
                if !tail {
                    node.set_next_on_line(run_id(r, i + 1));
                }
                out.push((run_id(r, i), node));
            }
        }
        out
    }
}

/// shortest gap between two announcements of new output, so a build log
/// reads as a few batches instead of a flood
const ANNOUNCE_GAP: Duration = Duration::from_millis(500);
/// the most text one announcement carries; a flood keeps its tail
const ANNOUNCE_MAX: usize = 1000;
/// the most lines one look reads back, however far the output ran
const ANNOUNCE_LINES: u64 = 50;

/// turns lines a pane finished into polite live-region text: the line being
/// typed on, a full-screen app and a cleared screen stay quiet
#[derive(Default)]
pub struct Announcer {
    /// (pane id, reflow generation, alt screen) the line ids below belong to
    pane: Option<(usize, u32, bool)>,
    /// absolute id of the first line not yet read
    next: u64,
    /// the line the user typed on, which the shell echoes back
    typed: Option<u64>,
    pending: String,
    last: Option<Instant>,
    /// the live region's current text
    spoken: String,
}

impl Announcer {
    /// read the lines `term` completed since the last look; `typed` says the
    /// user typed into it since then
    pub fn observe(&mut self, pane: usize, term: &Terminal, typed: bool) {
        let g = &term.grid;
        let cursor = g.abs_base() + (g.total_lines().saturating_sub(g.rows) + g.cursor.row) as u64;
        if typed {
            self.typed = Some(cursor);
        }
        let key = Some((pane, g.reflow_gen, term.using_alt));
        if self.pane != key || term.using_alt || cursor < self.next {
            self.pane = key;
            self.next = cursor;
            return;
        }
        for abs in self.next.max(g.abs_base()).max(cursor.saturating_sub(ANNOUNCE_LINES))..cursor {
            if self.typed == Some(abs) {
                continue;
            }
            let Some(line) = g.line_at_abs(abs) else {
                continue;
            };
            let row = Row::new(g, line, 0);
            if !row.text.is_empty() {
                self.pending.push_str(&row.text);
                self.pending.push('\n');
            }
        }
        self.next = cursor;
        if self.pending.len() > ANNOUNCE_MAX {
            let mut cut = self.pending.len() - ANNOUNCE_MAX;
            while !self.pending.is_char_boundary(cut) {
                cut += 1;
            }
            self.pending.drain(..cut);
        }
    }

    /// the live region's text, moved on to the pending lines once the gap
    /// since the last announcement has passed
    pub fn spoken(&mut self, now: Instant) -> &str {
        if !self.pending.is_empty() && self.wait(now).is_none() {
            let text = std::mem::take(&mut self.pending);
            let text = text.trim_end();
            // a repeat of the same text needs a changed name to be read again
            self.spoken = if text == self.spoken.trim_end() && !self.spoken.ends_with(' ') {
                format!("{text} ")
            } else {
                text.to_string()
            };
            self.last = Some(now);
        }
        &self.spoken
    }

    /// how long pending text still waits for its gap; None when nothing waits
    /// or it's due now
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if self.pending.is_empty() {
            return None;
        }
        let gap = ANNOUNCE_GAP.saturating_sub(now.saturating_duration_since(self.last?));
        (!gap.is_zero()).then_some(gap)
    }
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
        let screen = Screen::new(&t, (0.0, 10.0), (10.0, 20.0), None);
        let update = build_tree(
            Some(&screen),
            "",
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
        // root, title bar, tab list, tab panel, live output, terminal, three
        // row runs, the tab and the pane mode button
        assert_eq!(update.nodes.len(), 11);
        assert_eq!(update.nodes[0].1.role(), Role::Window);
        assert_eq!(node(&update, TAB_PANEL).role(), Role::TabPanel);
        assert_eq!(node(&update, OUTPUT).live(), Some(Live::Polite));
        let terminal = node(&update, TERMINAL);
        assert_eq!(terminal.role(), Role::Terminal);
        assert_eq!(update.focus, TERMINAL);
        let runs: Vec<&str> = terminal.children().iter().map(|id| node(&update, *id).value().unwrap_or_default()).collect();
        assert_eq!(runs, ["hello\n", "\n", ""]);
        let caret = TextPosition { node: run_id(0, 0), character_index: 5 };
        assert_eq!(terminal.text_selection(), Some(&TextSelection { anchor: caret, focus: caret }));
        let first = node(&update, run_id(0, 0));
        assert_eq!(first.character_lengths(), [1, 1, 1, 1, 1, 1]);
        assert_eq!(first.character_positions(), Some(&[0.0, 10.0, 20.0, 30.0, 40.0, 50.0][..]));
        assert_eq!(first.bounds(), Some(Rect::new(0.0, 10.0, 50.0, 30.0)));
        assert_eq!(target_for_node(Target::Tab(42).node_id()), Some(Target::Tab(42)));
        assert_eq!(target_for_node(Target::PaneMode.node_id()), Some(Target::PaneMode));
        let tab = node(&update, Target::Tab(42).node_id());
        assert!(tab.supports_action(Action::Click));
        assert_eq!(tab.is_selected(), Some(true));
        assert_eq!(node(&update, Target::PaneMode.node_id()).toggled(), Some(accesskit::Toggled::True));
        assert_eq!(update.nodes[0].1.children(), [TITLE_BAR, TAB_PANEL, OUTPUT]);
    }

    fn screen_of(bytes: &[u8], rows: usize, cols: usize) -> Terminal {
        let mut t = Terminal::new(rows, cols);
        Parser::new().advance(&mut t, bytes);
        t
    }

    #[test]
    fn an_unchanged_screen_keeps_its_runs_out_of_the_update() {
        let update = tree(Vec::new());
        assert!(update.nodes.iter().all(|(id, _)| *id != TERMINAL && id.0 & RUN_BASE == 0));
        assert_eq!(update.focus, TERMINAL);
    }

    #[test]
    fn wide_glyphs_span_two_cells_and_the_caret_keeps_its_blank_column() {
        let t = screen_of("\u{4f60}\u{597d} a ".as_bytes(), 2, 10);
        let screen = Screen::new(&t, (0.0, 0.0), (8.0, 16.0), None);
        let runs = screen.runs();
        let row = &runs[0].1;
        assert_eq!(row.value(), Some("\u{4f60}\u{597d} a \n"));
        assert_eq!(row.character_lengths(), [3, 3, 1, 1, 1, 1]);
        assert_eq!(row.character_positions(), Some(&[0.0, 16.0, 32.0, 40.0, 48.0, 56.0][..]));
        assert_eq!(row.character_widths(), Some(&[16.0, 16.0, 8.0, 8.0, 8.0, 0.0][..]));
        assert_eq!(row.word_starts(), [0, 3]);
        assert_eq!(screen.caret, (0, 5));
    }

    #[test]
    fn a_row_wider_than_one_run_chains_runs_on_its_line() {
        let t = screen_of(&[b'x'; 300], 2, 300);
        let screen = Screen::new(&t, (0.0, 0.0), (1.0, 1.0), Some(((0, 0), (0, 300))));
        let runs = screen.runs();
        assert_eq!(runs.len(), 3);
        let (first, second) = (&runs[0].1, &runs[1].1);
        assert_eq!(first.character_lengths().len(), RUN_CHARS);
        assert_eq!(first.next_on_line(), Some(run_id(0, 1)));
        assert_eq!(second.previous_on_line(), Some(run_id(0, 0)));
        assert_eq!(second.value().map(str::len), Some(300 - RUN_CHARS + 1));
        assert_eq!(second.word_starts(), [] as [u8; 0]);
        let update = build_tree(Some(&screen), "", "termie", None, None, None, Vec::new(), Vec::new(), Vec::new());
        let selection = node(&update, TERMINAL).text_selection().expect("selection");
        assert_eq!(selection.anchor, TextPosition { node: run_id(0, 0), character_index: 0 });
        assert_eq!(selection.focus, TextPosition { node: run_id(0, 1), character_index: 300 - RUN_CHARS });
    }

    #[test]
    fn announcer_reads_finished_lines_once_per_gap_and_skips_the_typed_line() {
        let mut t = Terminal::new(4, 20);
        let mut p = Parser::new();
        let mut a = Announcer::default();
        let start = Instant::now();
        p.advance(&mut t, b"$ ");
        a.observe(1, &t, false);
        p.advance(&mut t, b"ls");
        a.observe(1, &t, true);
        p.advance(&mut t, b"\r\nfile.txt\r\n$ ");
        a.observe(1, &t, false);
        assert_eq!(a.spoken(start), "file.txt");
        p.advance(&mut t, b"\r\nlate\r\n");
        a.observe(1, &t, false);
        assert_eq!(a.wait(start + Duration::from_millis(100)), Some(Duration::from_millis(400)));
        assert_eq!(a.spoken(start + Duration::from_millis(100)), "file.txt");
        assert_eq!(a.spoken(start + ANNOUNCE_GAP), "$\nlate");

        p.advance(&mut t, b"\x1b[?1049hfull screen\r\n");
        a.observe(1, &t, false);
        p.advance(&mut t, b"\x1b[?1049l");
        a.observe(1, &t, false);
        assert_eq!(a.wait(start + ANNOUNCE_GAP * 3), None, "the alt screen stays quiet");
        a.observe(2, &t, false);
        assert_eq!(a.spoken(start + ANNOUNCE_GAP * 3), "$\nlate", "a new pane starts at its cursor");
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
        build_tree(None, "", "termie", None, None, None, Vec::new(), Vec::new(), panels)
    }

    fn node(update: &TreeUpdate, id: NodeId) -> &Node {
        &update.nodes.iter().find(|(n, _)| *n == id).expect("node in tree").1
    }

    #[test]
    fn overlay_panel_hangs_off_the_window_and_takes_focus() {
        let update = tree(vec![dialog(Some(1))]);
        let panel = Target::Item(Layer::Dialog, PANEL).node_id();
        let default = Target::Item(Layer::Dialog, 1).node_id();
        assert_eq!(update.nodes[0].1.children(), [TITLE_BAR, TAB_PANEL, OUTPUT, panel]);
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
        assert_eq!(update.nodes[0].1.children().len(), 6);
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
