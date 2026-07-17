// no extra console window in release; keep one in debug for logs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod a11y;
mod apc;
mod color;
#[cfg(windows)]
mod defterm;
mod fxhash;
mod image;
mod grid;
mod input;
mod instance;
mod plugin;
mod pty;
mod regex;
mod render;
mod session;
mod sixel;
mod term;
mod update;
mod win;
#[cfg(debug_assertions)]
mod termview;
#[cfg(debug_assertions)]
mod uiview;
#[cfg(feature = "microbench")]
mod microbench;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId, WindowLevel};

use pty::{Pty, PtyMsg, ShellKind};
use render::{Hit, Hot, PaneDropSide, PaneView, Renderer};
use term::Terminal;

const CONTENT_PT: f32 = 14.0;
const CHROME_PT: f32 = 12.5;
/// pre-warmed shells kept ready so splits/tabs open instantly
const POOL_TARGET: usize = 3;
/// stop respawning after this many consecutive shell-spawn failures so a broken
/// shell can't peg a CPU core; the window then stays up (logged) instead
const MAX_WARM_FAILS: usize = 10;

type Rect = (f32, f32, f32, f32);

fn platform_window_attrs(attrs: WindowAttributes) -> WindowAttributes {
    #[cfg(all(unix, not(target_os = "macos")))]
    let attrs = {
        use winit::platform::wayland::WindowAttributesExtWayland;
        attrs.with_name("termie", "termie")
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let attrs = {
        use winit::platform::x11::WindowAttributesExtX11;
        attrs.with_name("termie", "termie")
    };
    attrs
}

fn satellite_window_attrs(icon: Option<winit::window::Icon>) -> WindowAttributes {
    platform_window_attrs(
        Window::default_attributes()
            .with_title("termie")
            .with_window_icon(icon)
            .with_decorations(false)
            .with_visible(false)
            .with_inner_size(LogicalSize::new(760.0, 480.0))
            .with_min_inner_size(LogicalSize::new(560.0, 380.0)),
    )
}

enum UserEvent {
    Pty { id: usize, bytes: Vec<u8> },
    Exited { id: usize },
    /// a pool shell finished spawning on a worker thread (None = spawn failed)
    PaneReady(Option<Box<Pane>>),
    /// a plugin process emitted a protocol message (id = plugin index)
    Plugin { id: usize, msg: plugin::PluginMsg },
    /// the marketplace catalog finished fetching on a worker thread (Ok with
    /// entries, or Err with a reason the fetch failed)
    Market(Result<Vec<plugin::market::Entry>, String>),
    /// the global quake hotkey fired (from the hotkey thread)
    #[cfg(any(windows, target_os = "linux"))]
    ToggleQuake,
    /// colors.conf or keybindings.conf changed on disk (watcher thread):
    /// re-read both so hand edits apply live, without a restart
    UserConfChanged,
    #[cfg(not(windows))]
    SystemThemeChanged(Option<bool>),
    /// a release check finished (None = up to date / unreachable); bool =
    /// the user asked from the palette, so silence is worth a status notice
    UpdateCheckDone(Option<update::Update>, bool),
    /// the native update finished downloading and installing (or failed)
    UpdateDownloaded(Result<std::path::PathBuf, String>),
    /// an accesskit adapter event (screen-reader tree request / action)
    Accessibility(accesskit_winit::Event),
    /// another ordinary launch joined this process and needs its own window
    Launch(instance::LaunchRequest),
    #[cfg(target_os = "linux")]
    KwinDragGeometry(win::KwinDragSnapshot),
    /// a default-terminal console session handed to this running instance
    #[cfg(windows)]
    Handoff(defterm::Handoff),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> Self {
        UserEvent::Accessibility(e)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Vertical,   // divider is vertical → panes side by side (left | right)
    Horizontal, // divider is horizontal → panes stacked (top / bottom)
}

/// one shell pane: its own pty, parser, and screen
struct Pane {
    id: usize,
    term: Terminal,
    parser: Parser,
    pty: Pty,
    /// the shell this pane was spawned with, kept so session restore can respawn
    /// the same shell in the same directory
    shell: ShellKind,
    /// true once the shell has produced output (prompt up) — safe to resize
    ready: bool,
    /// set when the shell rang the bell (BEL); drives a brief border flash
    flash: Option<Instant>,
    /// kitty-graphics APC scanner state, buffering image sequences across reads
    apc: apc::ApcScanner,
    /// OSC 133-derived command status for the pane badge: running while a
    /// command executes, done (with exit) until the pane is next viewed
    status: PaneStatus,
}

/// what a pane's shell is doing, for the corner badge + tab strip rollup —
/// the cockpit answer to "which agents are still working, which finished"
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneStatus {
    Idle,
    Running,
    Done(Option<i32>),
}

impl PaneStatus {
    /// severity for tab rollup + paint: 0 none, 1 running, 2 done-ok, 4 failed
    /// (3 is the bell-attention slot, which stays tab-level)
    fn rank(self) -> u8 {
        match self {
            PaneStatus::Idle => 0,
            PaneStatus::Running => 1,
            PaneStatus::Done(code) => {
                if code.unwrap_or(0) == 0 {
                    2
                } else {
                    4
                }
            }
        }
    }
}

impl Pane {
    // resize the screen and the pty together so the two can never diverge or be
    // transposed; both take (rows, cols). cell pixel size goes to ConPTY so
    // apps that ask the console for a window size in pixels get a real answer
    fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(rows, cols);
        let (cw, ch) = self.term.cell_px();
        self.pty.resize(rows as u16, cols as u16, cw, ch);
    }
}

// Leaf is the common, hot variant (walked every frame to paint panes); boxing
// it to shrink the enum would add an indirection to that path for no real gain
#[allow(clippy::large_enum_variant)]
enum Node {
    Leaf(Pane),
    Split {
        dir: Dir,
        /// fraction of the space given to `a` (the rest goes to `b`); 0.5 = even
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

struct Tab {
    focused: usize, // pane id
    root: Option<Node>,
    /// when set, that leaf pane fills the whole content area (tmux-style zoom),
    /// hiding its siblings until toggled off. transient — not persisted
    zoom: Option<usize>,
    /// user-given name overriding the cwd-derived label (persisted)
    title: Option<String>,
    /// a bell rang here while the tab was in the background (or the window
    /// unfocused); draws a dot on the tab until it is viewed. transient
    attention: bool,
    /// user-picked tint, an ansi palette index 1-6 so it follows the theme
    /// (wt's per-tab color; persisted with the session)
    color: Option<u8>,
}

fn tab_from_pane(pane: Pane) -> Tab {
    Tab {
        focused: pane.id,
        root: Some(Node::Leaf(pane)),
        zoom: None,
        title: None,
        attention: false,
        color: None,
    }
}

#[derive(Clone)]
struct TabDrag {
    source: WindowId,
    index: usize,
    start: PhysicalPosition<f64>,
    screen: Option<PhysicalPosition<i32>>,
    target: Option<(WindowId, usize)>,
    left_strip: bool,
    left_window: bool,
    label: String,
}

impl TabDrag {
    fn window_left(&mut self, window: WindowId) -> bool {
        let left_source = self.source == window;
        let left_target = self.target.is_some_and(|(target, _)| target == window);
        if left_source {
            self.left_strip = true;
            self.left_window = true;
        }
        if left_source || left_target {
            self.target = None;
        }
        left_source || left_target
    }
}

#[derive(Clone, Copy)]
struct PaneDropTarget {
    window: WindowId,
    tab: usize,
    pane: usize,
    side: PaneDropSide,
    rect: Rect,
}

#[derive(Clone, Copy)]
enum PaneDropDestination {
    Dock(PaneDropTarget),
    Tab(WindowId, usize),
}

#[derive(Clone)]
struct PaneDrag {
    source_window: WindowId,
    source_tab: usize,
    pane: usize,
    start: PhysicalPosition<f64>,
    screen: Option<PhysicalPosition<i32>>,
    target: Option<PaneDropDestination>,
    moved: bool,
    left_window: bool,
    label: String,
}

#[cfg(target_os = "linux")]
struct KwinDragProbe {
    generation: u64,
    tagged: Vec<(WindowId, String)>,
    started: Instant,
    script: Option<String>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct KwinWindowGeometry {
    window: WindowId,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl PaneDrag {
    fn window_left(&mut self, window: WindowId) -> bool {
        let left_source = self.source_window == window;
        let left_target = self.target.is_some_and(|target| match target {
            PaneDropDestination::Dock(target) => target.window == window,
            PaneDropDestination::Tab(target, _) => target == window,
        });
        if left_source {
            self.moved = true;
            self.left_window = true;
        }
        if left_source || left_target {
            self.target = None;
        }
        left_source || left_target
    }
}

/// an active text selection in one pane, anchored to (absolute line id, col)
/// in the grid's prompt-mark space — the highlight and the copied text stay on
/// the content the user swept even as output scrolls or the view moves
#[derive(Clone, Copy)]
struct Sel {
    pane: usize,
    start: (u64, usize),
    end: (u64, usize),
    /// alt+drag: rectangular column selection (rows and cols span independently)
    block: bool,
    /// grid.reflow_gen at anchor time; a rewrap re-bases line ids, so a stale
    /// generation means the selection no longer points at real content
    reflow_gen: u32,
}

/// keyboard mark mode: the selection cursor cell (viewport-relative — it moves
/// with the keys and rides the view at the edges) and, once shift-extending,
/// the content-anchored point it grows from
#[derive(Clone, Copy)]
struct MarkState {
    cur: (usize, usize),
    anchor: Option<(u64, usize)>,
}

/// map an absolute-anchored selection onto the pane's current viewport for
/// painting: an endpoint scrolled off-screen clamps to the edge, so a
/// selection taller than the screen highlights the visible slice
fn sel_view_span(g: &grid::Grid, s: &Sel) -> Option<render::SelSpan> {
    if s.reflow_gen != g.reflow_gen {
        return None;
    }
    let (mut a, mut b) = (s.start, s.end);
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let top = g.viewport_to_abs(0);
    let bot = g.viewport_to_abs(g.rows.saturating_sub(1));
    if b.0 < top || a.0 > bot {
        return None;
    }
    let start = if a.0 < top {
        (0, if s.block { a.1 } else { 0 })
    } else {
        (g.abs_to_viewport(a.0)?, a.1)
    };
    let end = if b.0 > bot {
        (g.rows.saturating_sub(1), if s.block { b.1 } else { g.cols.saturating_sub(1) })
    } else {
        (g.abs_to_viewport(b.0)?, b.1)
    };
    Some((start, end, s.block))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaletteAction {
    NewTab,
    NewTabHere,
    /// new tab with the focused pane's shell and cwd (Windows Terminal's Ctrl+Shift+D)
    DuplicateTab,
    NewShell(ShellKind),
    /// open one fresh window carrying the focused pane's shell and cwd
    NewWindow,
    /// open a new window with an elevated shell in the focused directory
    AdminWindow,
    SplitV,
    SplitH,
    NextTab,
    PrevTab,
    /// searchable switcher over every tab in the current window
    TabSearch,
    /// reorder: nudge the active tab one slot along the strip
    MoveTabLeft,
    MoveTabRight,
    CloseTab,
    /// re-spawn the most recently closed tab from its launch spec (Ctrl+Shift+T)
    ReopenTab,
    Settings,
    PaneMode,
    /// keyboard selection: move a cursor through screen + scrollback and copy
    MarkMode,
    /// select the focused pane's retained history and live screen
    SelectAll,
    /// toggle termie as the platform's default terminal
    DefaultTerminal,
    #[cfg(any(windows, target_os = "linux"))]
    Quake,
    Theme,
    Plugins,
    Quit,
    // bindable built-ins (also reachable from keybindings.conf); some are not
    // shown in the palette, only the keybinding table
    ToggleSettings,
    FontInc,
    FontDec,
    FontReset,
    ToggleBroadcast,
    OpenFind,
    OpenPalette,
    Copy,
    Paste,
    CloseFocusedPane,
    /// type a configured string into the focused pane (keybindings.conf
    /// `send <text>`); the index points into App::send_inputs
    SendInput(usize),
    ToggleZoom,
    ToggleFullscreen,
    RenameTab,
    /// prompt-jump passes through to the program when there are no OSC-133 marks
    JumpPromptPrev,
    JumpPromptNext,
    /// focus the next pane whose command failed / rang / finished / is running
    JumpAttention,
    /// keep this window above every other (toggles; quake uses its own level)
    ToggleOnTop,
    /// open the searchable font picker over installed monospace families
    FontPicker,
    /// keyboard scrollback: a page (or straight to an end) of history
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    ClearScrollback,
    /// write the focused pane's history + screen as plain text to Downloads
    ExportScrollback,
    /// check for a newer release / confirm installing a pending one
    InstallUpdate,
    /// 0-based tab index (Ctrl+1..9)
    SelectTab(usize),
    /// recolor the active tab from the keyboard — the same swatch list the tab
    /// menu offers, indexed into TAB_COLOR_ITEMS (0 clears)
    SetTabColor(usize),
}

const PALETTE_ACTIONS: &[(&str, PaletteAction)] = &[
    ("new tab", PaletteAction::NewTab),
    ("new tab here", PaletteAction::NewTabHere),
    ("duplicate tab", PaletteAction::DuplicateTab),
    ("new tab: pwsh", PaletteAction::NewShell(ShellKind::Pwsh)),
    #[cfg(windows)]
    ("new tab: cmd", PaletteAction::NewShell(ShellKind::Cmd)),
    #[cfg(windows)]
    ("new tab: wsl", PaletteAction::NewShell(ShellKind::Wsl)),
    #[cfg(not(windows))]
    ("new tab: bash", PaletteAction::NewShell(ShellKind::Bash)),
    #[cfg(not(windows))]
    ("new tab: zsh", PaletteAction::NewShell(ShellKind::Zsh)),
    #[cfg(not(windows))]
    ("new tab: fish", PaletteAction::NewShell(ShellKind::Fish)),
    ("new window", PaletteAction::NewWindow),
    ("new admin window", PaletteAction::AdminWindow),
    ("split vertical", PaletteAction::SplitV),
    ("split horizontal", PaletteAction::SplitH),
    ("next tab", PaletteAction::NextTab),
    ("previous tab", PaletteAction::PrevTab),
    ("tab search", PaletteAction::TabSearch),
    ("move tab left", PaletteAction::MoveTabLeft),
    ("move tab right", PaletteAction::MoveTabRight),
    ("close tab", PaletteAction::CloseTab),
    ("reopen closed tab", PaletteAction::ReopenTab),
    ("settings", PaletteAction::Settings),
    ("pane mode", PaletteAction::PaneMode),
    ("mark mode", PaletteAction::MarkMode),
    ("select all", PaletteAction::SelectAll),
    ("jump to attention", PaletteAction::JumpAttention),
    ("always on top", PaletteAction::ToggleOnTop),
    ("choose font", PaletteAction::FontPicker),
    ("zoom pane", PaletteAction::ToggleZoom),
    ("toggle fullscreen", PaletteAction::ToggleFullscreen),
    ("rename tab", PaletteAction::RenameTab),
    // one entry per TAB_COLOR_ITEMS row (a test keeps them in lockstep)
    ("tab color: none", PaletteAction::SetTabColor(0)),
    ("tab color: red", PaletteAction::SetTabColor(1)),
    ("tab color: green", PaletteAction::SetTabColor(2)),
    ("tab color: yellow", PaletteAction::SetTabColor(3)),
    ("tab color: blue", PaletteAction::SetTabColor(4)),
    ("tab color: magenta", PaletteAction::SetTabColor(5)),
    ("tab color: cyan", PaletteAction::SetTabColor(6)),
    #[cfg(any(windows, target_os = "linux"))]
    ("quake drop-down", PaletteAction::Quake),
    ("cycle theme", PaletteAction::Theme),
    ("plugins", PaletteAction::Plugins),
    ("find", PaletteAction::OpenFind),
    ("copy", PaletteAction::Copy),
    ("paste", PaletteAction::Paste),
    ("scroll page up", PaletteAction::ScrollPageUp),
    ("scroll page down", PaletteAction::ScrollPageDown),
    ("scroll to top", PaletteAction::ScrollTop),
    ("scroll to bottom", PaletteAction::ScrollBottom),
    ("clear scrollback", PaletteAction::ClearScrollback),
    ("export scrollback", PaletteAction::ExportScrollback),
    ("update termie", PaletteAction::InstallUpdate),
    ("default terminal", PaletteAction::DefaultTerminal),
    ("broadcast input", PaletteAction::ToggleBroadcast),
    ("close pane", PaletteAction::CloseFocusedPane),
    ("font increase", PaletteAction::FontInc),
    ("font decrease", PaletteAction::FontDec),
    ("font reset", PaletteAction::FontReset),
    ("quit", PaletteAction::Quit),
];

/// fuzzy subsequence score of `query` against `label`: every query char must
/// appear in order; contiguous runs, word-boundary hits, and earlier matches
/// score higher. None when the query is not a subsequence. case-insensitive
fn fuzzy_score(query: &str, label: &str) -> Option<i32> {
    let q: Vec<char> = query.trim().chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let lab: Vec<char> = label.chars().collect();
    let mut score = 0i32;
    let mut qi = 0usize;
    let mut prev: Option<usize> = None;
    for (li, &lc) in lab.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if lc.to_ascii_lowercase() == q[qi] {
            score += 1;
            if prev == Some(li.wrapping_sub(1)) {
                score += 5; // contiguous with the previous match
            }
            if li == 0 || matches!(lab.get(li - 1), Some(' ' | '-' | '_' | ':')) {
                score += 8; // at a word boundary
            }
            score -= (li as i32) / 4; // earlier matches slightly preferred
            prev = Some(li);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

/// palette entries matching `query`, best match first (stable by label on ties)
/// the static actions plus one "new tab: <name>" per custom profile. built on
/// first use (profiles are installed before the ui exists) and process-wide,
/// so the leaked labels are a bounded one-time cost
fn all_palette_actions() -> &'static [(&'static str, PaletteAction)] {
    static ALL: std::sync::OnceLock<Vec<(&'static str, PaletteAction)>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut actions = PALETTE_ACTIONS.to_vec();
        for prof in pty::profiles() {
            let label: &'static str = Box::leak(format!("new tab: {}", prof.name).into_boxed_str());
            actions.push((label, PaletteAction::NewShell(ShellKind::Custom(prof.name.as_str()))));
        }
        actions
    })
}

/// fold the installed wsl distros into the profile list as synthetic
/// "wsl: <name>" entries (wsl.exe -d <name>), one per distro. every distro gets
/// its named entry (like windows terminal) so it's findable even when it's the
/// default; a name a user profile already defines is left untouched
fn with_wsl_profiles(mut profiles: Vec<pty::Profile>, distros: Vec<String>) -> Vec<pty::Profile> {
    for distro in distros {
        let name = format!("wsl: {distro}");
        if !profiles.iter().any(|p| p.name == name) {
            profiles.push(pty::Profile {
                name,
                argv: vec!["wsl.exe".to_string(), "-d".to_string(), distro],
                cwd: None,
                env: Vec::new(),
            });
        }
    }
    profiles
}

/// the profile named `name` in `profiles`, created empty if new, so config lines
/// (argv, .cwd, .env.<VAR>) can arrive in any order and accumulate onto it
fn profile_mut<'a>(profiles: &'a mut Vec<pty::Profile>, name: &str) -> &'a mut pty::Profile {
    if let Some(i) = profiles.iter().position(|p| p.name == name) {
        &mut profiles[i]
    } else {
        profiles.push(pty::Profile {
            name: name.to_string(),
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
        });
        profiles.last_mut().unwrap()
    }
}

/// the new-tab '+' dropdown rows: the built-in shells for this OS then one per
/// custom profile, as (menu label, shell to spawn). the ordering is the click index
fn new_tab_menu_entries() -> Vec<(String, ShellKind)> {
    #[cfg(windows)]
    let mut rows = vec![
        ("pwsh".to_string(), ShellKind::Pwsh),
        ("cmd".to_string(), ShellKind::Cmd),
        ("wsl".to_string(), ShellKind::Wsl),
    ];
    #[cfg(not(windows))]
    let mut rows = vec![
        ("bash".to_string(), ShellKind::Bash),
        ("zsh".to_string(), ShellKind::Zsh),
        ("fish".to_string(), ShellKind::Fish),
    ];
    rows.extend(pty::profiles().iter().map(|p| (p.name.clone(), ShellKind::Custom(p.name.as_str()))));
    rows
}

fn palette_filter(query: &str) -> Vec<(&'static str, PaletteAction)> {
    if query.trim().is_empty() {
        return all_palette_actions().to_vec();
    }
    let mut scored: Vec<(i32, &'static str, PaletteAction)> = all_palette_actions()
        .iter()
        .filter_map(|(label, a)| fuzzy_score(query, label).map(|s| (s, *label, *a)))
        .collect();
    scored.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(y.1)));
    scored.into_iter().map(|(_, l, a)| (l, a)).collect()
}

/// searchable tab rows, numbered for duplicate titles and direct numeric lookup
fn tab_filter(query: &str, labels: &[String]) -> Vec<(String, usize)> {
    let query = query.trim();
    let mut rows: Vec<(i32, usize, String)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, label)| {
            let row = format!("{}  {label}", i + 1);
            fuzzy_score(query, &row).map(|score| (score, i, row))
        })
        .collect();
    if !query.is_empty() {
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    rows.into_iter().map(|(_, i, row)| (row, i)).collect()
}

#[derive(Clone, Copy)]
enum PaletteMode {
    Commands,
    Tabs,
}

struct PaletteState {
    query: String,
    selected: usize,
    mode: PaletteMode,
}

/// right-click context menu state: anchor point, hovered item, and what it acts on
struct PaneMenu {
    x: f32,
    y: f32,
    hovered: Option<usize>,
    target: MenuTarget,
}

/// which surface a context menu acts on: the focused pane, the tab at this
/// index, or the new-tab '+' (whose items are shell profiles)
#[derive(Clone, Copy)]
enum MenuTarget {
    /// the per-tab color swatch list, reached from the tab menu's color row
    TabColor(usize),
    Pane,
    Tab(usize),
    NewTab,
}

/// find-in-scrollback overlay state for the focused pane; matches are
/// (global_line_index, col) into that pane's grid
struct FindState {
    query: String,
    /// (global line, col, match char length) — regex matches vary in length
    matches: Vec<(usize, usize, usize)>,
    current: usize,
    /// the query failed to compile as a regex (regex mode only)
    bad: bool,
}

/// a pending modal confirmation: the action runs on enter, esc cancels
enum ConfirmAction {
    /// send these bytes to a pane — a risky multiline paste held for confirm
    PasteBytes { pane: usize, bytes: Vec<u8> },
    /// close a tab that holds more than one pane
    CloseTab { tab: usize },
    /// close every tab except `keep` when at least one of them holds >1 pane
    CloseOthers { keep: usize },
    /// quit the app (main window X / Alt+F4 / the quit action) with live panes
    Quit,
    /// close the current torn-off window and its panes
    CloseWindow,
    /// download the pending update and restart into it
    InstallUpdate,
}

/// tab-rename text field overlay: which tab is being renamed + the current input
struct RenameState {
    tab: usize,
    buf: String,
}

/// the launch spec of a closed tab, kept so Ctrl+Shift+T can re-spawn it. only
/// the focused pane's shell + cwd and a custom title are restored, not splits,
/// scrollback, or running processes
struct ClosedTab {
    shell: ShellKind,
    cwd: Option<String>,
    title: Option<String>,
}

/// how many closed tabs the reopen stack remembers; older ones drop off
const CLOSED_TAB_CAP: usize = 10;

/// push a closed-tab spec onto the bounded reopen stack, dropping the oldest
/// once it passes the cap
fn push_closed_tab(stack: &mut Vec<ClosedTab>, closed: ClosedTab) {
    stack.push(closed);
    if stack.len() > CLOSED_TAB_CAP {
        stack.remove(0);
    }
}

/// modal yes/no overlay state; captures keys until resolved
struct ConfirmState {
    prompt: String,
    hint: String,
    action: ConfirmAction,
}

/// one row in the plugins marketplace overlay: either an installed plugin or a
/// remote catalog entry not yet installed
#[derive(Clone)]
struct MarketRow {
    id: String,
    name: String,
    version: String,
    /// one-line catalog description (empty when unknown)
    description: String,
    permissions: Vec<String>,
    /// installed + currently enabled
    installed: bool,
    enabled: bool,
    /// the catalog download url, if present in the remote catalog
    url: Option<String>,
}

/// the plugins marketplace overlay state
struct MarketState {
    rows: Vec<MarketRow>,
    selected: usize,
    /// transient status line (last action result / hint)
    status: String,
    /// the remote catalog is still being fetched on a worker thread
    loading: bool,
    /// the catalog fetch failed (vs. simply returning no entries)
    fetch_failed: bool,
}

// ---- tree helpers (free functions, by-value where ownership moves) ----

fn layout(node: &Node, rect: Rect, out: &mut Vec<(usize, Rect)>) {
    match node {
        Node::Leaf(p) => out.push((p.id, rect)),
        Node::Split { dir, ratio, a, b } => {
            let (x, y, w, h) = rect;
            let r = ratio.clamp(0.1, 0.9);
            match dir {
                Dir::Vertical => {
                    let split = (w * r).floor().max(1.0).min(w - 1.0);
                    layout(a, (x, y, split, h), out);
                    layout(b, (x + split, y, w - split, h), out);
                }
                Dir::Horizontal => {
                    let split = (h * r).floor().max(1.0).min(h - 1.0);
                    layout(a, (x, y, w, split), out);
                    layout(b, (x, y + split, w, h - split), out);
                }
            }
        }
    }
}

fn find_pane_mut(node: &mut Node, id: usize) -> Option<&mut Pane> {
    match node {
        Node::Leaf(p) => {
            if p.id == id {
                Some(p)
            } else {
                None
            }
        }
        Node::Split { a, b, .. } => find_pane_mut(a, id).or_else(|| find_pane_mut(b, id)),
    }
}

fn find_pane(node: &Node, id: usize) -> Option<&Pane> {
    match node {
        Node::Leaf(p) => (p.id == id).then_some(p),
        Node::Split { a, b, .. } => find_pane(a, id).or_else(|| find_pane(b, id)),
    }
}

fn first_leaf(node: &Node) -> usize {
    match node {
        Node::Leaf(p) => p.id,
        Node::Split { a, .. } => first_leaf(a),
    }
}

/// the tab label when a pane has no cwd or title yet: the OS's default shell
const FALLBACK_LABEL: &str = if cfg!(windows) { "pwsh" } else { "shell" };

/// derive a short tab/title label from an OSC-7 cwd uri (e.g. file:///C:/Users/dev -> dev)
fn cwd_label(cwd: Option<&str>) -> String {
    let Some(u) = cwd else {
        return FALLBACK_LABEL.to_string();
    };
    let path = u
        .strip_prefix("file://")
        .map(|r| match r.find('/') {
            Some(i) => &r[i..],
            None => r,
        })
        .unwrap_or(u);
    let path = path.trim_end_matches(['/', '\\']);
    // strip control chars: this feeds both the tab label and the OS window title,
    // and the cwd comes from an untrusted OSC-7 string
    let seg: String = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .replace("%20", " ")
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if seg.is_empty() {
        FALLBACK_LABEL.to_string()
    } else {
        seg
    }
}

/// conpty and the shells announce themselves through OSC 0/2 the moment a pane
/// spawns (exe paths, bare shell names); letting those through would displace
/// the more useful cwd label on every tab
fn boring_title(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    if lower.ends_with(".exe")
        || lower.starts_with("\\\\")
        || lower.as_bytes().get(1..3) == Some(b":\\")
        || lower.as_bytes().get(1..3) == Some(b":/")
    {
        return true;
    }
    matches!(
        lower.as_str(),
        "pwsh" | "powershell" | "windows powershell" | "powershell 7" | "cmd"
            | "command prompt" | "nu" | "nushell" | "bash" | "wsl" | "zsh" | "fish" | "sh"
    )
}

/// the label a tab shows: a user rename wins, then a meaningful OSC 0/2 title
/// from the focused pane (agents report status this way), then its cwd
fn tab_label(tab: &Tab) -> String {
    if let Some(title) = tab.title.as_deref().filter(|s| !s.is_empty()) {
        return title.to_string();
    }
    let pane = tab.root.as_ref().and_then(|r| find_pane(r, tab.focused));
    pane.map(pane_label).unwrap_or_else(|| FALLBACK_LABEL.to_string())
}

fn pane_label(pane: &Pane) -> String {
    let title = pane.term.title.trim();
    if !title.is_empty() && !boring_title(title) {
        title.chars().take(64).collect()
    } else {
        cwd_label(pane.term.cwd.as_deref())
    }
}

/// serialize a pane's badge state for the plugin bus (read_output-gated)
fn pane_state_event(pane: usize, status: PaneStatus, title: &str) -> plugin::HostEvent {
    let (state, exit) = match status {
        PaneStatus::Idle => ("idle", None),
        PaneStatus::Running => ("running", None),
        PaneStatus::Done(code) => ("done", code),
    };
    plugin::HostEvent::PaneState {
        pane: pane as u64,
        state: state.to_string(),
        exit,
        title: title.to_string(),
    }
}

/// turn an OSC-7 file:// uri into a filesystem path (forward slashes are fine
/// for std::fs). the leading slash is only an artifact on windows drive paths
/// (`file:///C:/x` → `C:/x`); a unix path keeps it (`file:///home/x` → `/home/x`)
fn cwd_path(cwd: Option<&str>) -> Option<String> {
    let u = cwd?;
    let path = u
        .strip_prefix("file://")
        .map(|r| match r.find('/') {
            Some(i) => &r[i..],
            None => r,
        })
        .unwrap_or(u);
    let drive = path.len() > 2 && path.starts_with('/') && path.as_bytes()[2] == b':';
    let path = if drive { &path[1..] } else { path };
    Some(path.replace("%20", " "))
}

/// parse a color as #rrggbb, #rgb, or r,g,b
fn parse_color(s: &str) -> Option<color::Rgb> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some(color::Rgb::new((v >> 16) as u8, (v >> 8) as u8, v as u8)),
            3 => Some(color::Rgb::new(
                (((v >> 8) & 0xf) * 17) as u8,
                (((v >> 4) & 0xf) * 17) as u8,
                ((v & 0xf) * 17) as u8,
            )),
            _ => None,
        };
    }
    let p: Vec<&str> = s.split(',').collect();
    if p.len() == 3 {
        Some(color::Rgb::new(
            p[0].trim().parse().ok()?,
            p[1].trim().parse().ok()?,
            p[2].trim().parse().ok()?,
        ))
    } else {
        None
    }
}

/// load user color overrides from %APPDATA%\termie\colors.conf (key=color lines,
/// e.g. `bg=#101216`, `ansi1=#bf6360`); missing file yields no overrides
fn load_color_overrides() -> Vec<(String, color::Rgb)> {
    let mut out = Vec::new();
    let Some(dir) = app_dir() else {
        return out;
    };
    let path = dir.join("colors.conf");
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            log::warn!("colors.conf: no '=' in line: {line}");
            continue;
        };
        match parse_color(v) {
            Some(c) => out.push((k.trim().to_string(), c)),
            None => log::warn!("colors.conf: unparseable color '{}' for key '{}'", v.trim(), k.trim()),
        }
    }
    out
}

/// match a key event's logical key against a parsed binding key
fn key_matches(ev: &Key, bind: &Key) -> bool {
    match (ev, bind) {
        (Key::Named(a), Key::Named(b)) => a == b,
        (Key::Character(a), Key::Character(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// parse a single key token: a name (enter/tab/esc/up/f5/...) or a one-char key
fn parse_key(s: &str) -> Option<Key> {
    Some(match s {
        "enter" | "return" => Key::Named(NamedKey::Enter),
        "tab" => Key::Named(NamedKey::Tab),
        "space" => Key::Named(NamedKey::Space),
        "esc" | "escape" => Key::Named(NamedKey::Escape),
        "up" => Key::Named(NamedKey::ArrowUp),
        "down" => Key::Named(NamedKey::ArrowDown),
        "left" => Key::Named(NamedKey::ArrowLeft),
        "right" => Key::Named(NamedKey::ArrowRight),
        "f1" => Key::Named(NamedKey::F1),
        "f2" => Key::Named(NamedKey::F2),
        "f3" => Key::Named(NamedKey::F3),
        "f4" => Key::Named(NamedKey::F4),
        "f5" => Key::Named(NamedKey::F5),
        "f6" => Key::Named(NamedKey::F6),
        "f7" => Key::Named(NamedKey::F7),
        "f8" => Key::Named(NamedKey::F8),
        "f9" => Key::Named(NamedKey::F9),
        "f10" => Key::Named(NamedKey::F10),
        "f11" => Key::Named(NamedKey::F11),
        "f12" => Key::Named(NamedKey::F12),
        "f23" | "mouse4" | "xbutton1" => Key::Named(NamedKey::F23),
        "f24" | "mouse5" | "xbutton2" => Key::Named(NamedKey::F24),
        "insert" | "ins" => Key::Named(NamedKey::Insert),
        "delete" | "del" => Key::Named(NamedKey::Delete),
        "home" => Key::Named(NamedKey::Home),
        "end" => Key::Named(NamedKey::End),
        "pageup" | "pgup" => Key::Named(NamedKey::PageUp),
        "pagedown" | "pgdn" => Key::Named(NamedKey::PageDown),
        s if s.chars().count() == 1 => Key::Character(s.into()),
        _ => return None,
    })
}

/// parse a combo like "ctrl+shift+t" into modifiers + a key
fn parse_combo(s: &str) -> Option<(ModifiersState, Key)> {
    let mut mods = ModifiersState::empty();
    let mut key = None;
    for part in s.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            "shift" => mods |= ModifiersState::SHIFT,
            "alt" => mods |= ModifiersState::ALT,
            "super" | "win" | "cmd" | "meta" => mods |= ModifiersState::SUPER,
            "" => {}
            other => key = parse_key(other),
        }
    }
    key.map(|k| (mods, k))
}

/// format a key back to a keybindings.conf token, the inverse of parse_key. None
/// for a key the syntax can't express (the '+' separator)
fn key_label(key: &Key) -> Option<String> {
    let named = |s: &str| Some(s.to_string());
    match key {
        Key::Character(c) => {
            if c.as_str() == "+" {
                return None; // '+' is the combo separator, unrepresentable in a chord
            }
            Some(c.to_string())
        }
        Key::Named(n) => match n {
            NamedKey::Enter => named("enter"),
            NamedKey::Tab => named("tab"),
            NamedKey::Space => named("space"),
            NamedKey::Escape => named("esc"),
            NamedKey::ArrowUp => named("up"),
            NamedKey::ArrowDown => named("down"),
            NamedKey::ArrowLeft => named("left"),
            NamedKey::ArrowRight => named("right"),
            NamedKey::Insert => named("insert"),
            NamedKey::Delete => named("delete"),
            NamedKey::Home => named("home"),
            NamedKey::End => named("end"),
            NamedKey::PageUp => named("pageup"),
            NamedKey::PageDown => named("pagedown"),
            NamedKey::F1 => named("f1"),
            NamedKey::F2 => named("f2"),
            NamedKey::F3 => named("f3"),
            NamedKey::F4 => named("f4"),
            NamedKey::F5 => named("f5"),
            NamedKey::F6 => named("f6"),
            NamedKey::F7 => named("f7"),
            NamedKey::F8 => named("f8"),
            NamedKey::F9 => named("f9"),
            NamedKey::F10 => named("f10"),
            NamedKey::F11 => named("f11"),
            NamedKey::F12 => named("f12"),
            NamedKey::F23 => named("mouse4"),
            NamedKey::F24 => named("mouse5"),
            _ => None,
        },
        _ => None,
    }
}

/// format a chord back to a keybindings.conf combo like "ctrl+shift+t"; None when
/// the key can't be written (see key_label)
fn combo_label(mods: &ModifiersState, key: &Key) -> Option<String> {
    let mut s = String::new();
    if mods.control_key() {
        s.push_str("ctrl+");
    }
    if mods.shift_key() {
        s.push_str("shift+");
    }
    if mods.alt_key() {
        s.push_str("alt+");
    }
    if mods.super_key() {
        s.push_str("super+");
    }
    s.push_str(&key_label(key)?);
    Some(s)
}

/// where the active-tab index lands after the tab at `from` is moved to `to`:
/// the active tab itself follows the move, tabs between the two slots shift one
fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// true when the focused *view* (the pane's grid) changed. identity is the
/// globally unique pane id; tab index is unstable under insert/remove/reorder
/// and must not count as a view change — same pane at a left-shifted tab index
/// is still the same scrollback for find
fn focus_view_changed(
    before: Option<(usize, usize)>,
    after: Option<(usize, usize)>,
) -> bool {
    match (before, after) {
        (None, None) => false,
        (None, Some(_)) | (Some(_), None) => true,
        // compare pane ids only; discard tab indices
        (Some((_, bp)), Some((_, ap))) => bp != ap,
    }
}

/// find matches are bound to one pane grid. recompute only when find is open,
/// not held (mid-flight temporary owner switch), and the focused pane id
/// actually changed — tab reindex alone is not a view change
fn find_must_follow_focus(
    find_open: bool,
    before: Option<(usize, usize)>,
    after: Option<(usize, usize)>,
    hold: bool,
) -> bool {
    find_open && !hold && focus_view_changed(before, after)
}

/// after a focus context change, replace the match list and snap the cursor to
/// the first hit in the new grid (or stay at 0 when empty)
fn find_after_grid_change<T>(matches: Vec<T>) -> (Vec<T>, usize) {
    (matches, 0)
}

/// after closing a pane that may have lived in a non-viewer tab, the active_tab
/// index to restore. `None` means leave whatever close already set (viewer was
/// the owner). when the owner tab was removed, indices after it shift left
fn restore_viewer_tab(
    prev_active: usize,
    owner: usize,
    tabs_after: usize,
    tab_removed: bool,
) -> Option<usize> {
    if tabs_after == 0 {
        return None;
    }
    if prev_active == owner {
        // viewer was the owner: close_tab / pane-retarget already left active
        // on the right tab
        return None;
    }
    let idx = if tab_removed && prev_active > owner {
        prev_active - 1
    } else {
        prev_active
    };
    Some(idx.min(tabs_after - 1))
}

/// focus identity (active_tab, pane_id) after tab `closed` is removed.
/// `pane_ids` is the focused pane id of each tab *before* the close, indexed
/// by tab slot. mirrors do_close_tab's active_tab arithmetic so unit tests can
/// drive the real find-follow decision without a window
fn focus_identity_after_tab_close(
    pane_ids: &[usize],
    active: usize,
    closed: usize,
) -> Option<(usize, usize)> {
    if pane_ids.is_empty() || closed >= pane_ids.len() || active >= pane_ids.len() {
        return None;
    }
    let remaining: Vec<usize> = pane_ids
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != closed)
        .map(|(_, p)| *p)
        .collect();
    if remaining.is_empty() {
        return None;
    }
    let mut new_active = active;
    if new_active > closed {
        new_active -= 1;
    }
    new_active = new_active.min(remaining.len() - 1);
    Some((new_active, remaining[new_active]))
}

/// the built-in keybindings, seeded before any user overrides. matching at the
/// gate is exact-modifier (Ctrl+Alt+X is a different chord than Ctrl+X), so the
/// shift-produced symbols '+' and '_' are seeded with Ctrl+Shift, the modifiers
/// that physically accompany them. Ctrl+Shift+P (pane mode toggle) and
/// Esc/Ctrl+, settings-close are intentionally absent — they stay as dedicated
/// state-aware handlers in handle_shortcut (and are not rebindable)
fn default_keybindings() -> Vec<(ModifiersState, Key, PaletteAction)> {
    use PaletteAction as A;
    let ctrl = ModifiersState::CONTROL;
    let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
    let chr = |s: &str| Key::Character(s.into());
    let mut v = vec![
        (ctrl, chr(","), A::ToggleSettings),
        (ctrl, chr("="), A::FontInc),
        (cs, chr("+"), A::FontInc),
        (ctrl, chr("-"), A::FontDec),
        (cs, chr("_"), A::FontDec),
        (ctrl, chr("0"), A::FontReset),
        (ctrl, Key::Named(NamedKey::Tab), A::NextTab),
        (cs, Key::Named(NamedKey::Tab), A::PrevTab),
        (cs, chr("b"), A::ToggleBroadcast),
        (cs, chr("f"), A::OpenFind),
        (ctrl, Key::Named(NamedKey::ArrowUp), A::JumpPromptPrev),
        (ctrl, Key::Named(NamedKey::ArrowDown), A::JumpPromptNext),
        (ctrl, chr("p"), A::OpenPalette),
        (ctrl, chr("t"), A::NewTab),
        (cs, chr("t"), A::ReopenTab),
        (cs, chr("n"), A::NewWindow),
        (cs, chr("d"), A::DuplicateTab),
        (cs, chr("c"), A::Copy),
        (cs, chr("v"), A::Paste),
        // the classic conhost chords, kept by windows terminal too
        (ctrl, Key::Named(NamedKey::Insert), A::Copy),
        (ModifiersState::SHIFT, Key::Named(NamedKey::Insert), A::Paste),
        // keyboard scrollback nav (conhost/WT convention)
        (ModifiersState::SHIFT, Key::Named(NamedKey::PageUp), A::ScrollPageUp),
        (ModifiersState::SHIFT, Key::Named(NamedKey::PageDown), A::ScrollPageDown),
        (cs, Key::Named(NamedKey::Home), A::ScrollTop),
        (cs, Key::Named(NamedKey::End), A::ScrollBottom),
        // tab reorder (the VS Code / Konsole chords)
        (cs, Key::Named(NamedKey::PageUp), A::MoveTabLeft),
        (cs, Key::Named(NamedKey::PageDown), A::MoveTabRight),
        (ModifiersState::empty(), Key::Named(NamedKey::F11), A::ToggleFullscreen),
        (cs, chr("w"), A::CloseFocusedPane),
        (cs, chr("e"), A::SplitV),
        (cs, chr("o"), A::SplitH),
        // keyboard selection, the windows terminal chord
        (cs, chr("m"), A::MarkMode),
        (cs, chr("a"), A::SelectAll),
        // cockpit: hop to the pane that needs eyes
        (ctrl | ModifiersState::ALT, chr("a"), A::JumpAttention),
    ];
    for n in 1u8..=9 {
        v.push((ctrl, chr(&n.to_string()), A::SelectTab((n - 1) as usize)));
    }
    v
}

/// jump-list tasks: a plain new window plus one per shell and custom profile.
/// (title, exe arguments) pairs; a profile name with a quote is skipped rather
/// than risk a mangled argv
fn jumplist_entries() -> Vec<(String, String)> {
    let mut v = vec![("new window".to_string(), String::new())];
    #[cfg(windows)]
    for label in ["pwsh", "cmd", "wsl"] {
        v.push((format!("new window: {label}"), format!("--shell {label}")));
    }
    #[cfg(not(windows))]
    for label in ["bash", "zsh", "fish"] {
        v.push((format!("new window: {label}"), format!("--shell {label}")));
    }
    for prof in pty::profiles() {
        if !prof.name.contains('"') {
            v.push((format!("new window: {}", prof.name), format!("--shell \"{}\"", prof.name)));
        }
    }
    v
}

/// keybinding-only action labels — the ones not in the command palette. several
/// labels can alias one action; the first label of each action is the canonical
/// name the generated keybindings.conf template uses
const KEYBIND_ALIASES: &[(&str, PaletteAction)] = &[
    ("copy", PaletteAction::Copy),
    ("paste", PaletteAction::Paste),
    ("find", PaletteAction::OpenFind),
    ("toggle broadcast", PaletteAction::ToggleBroadcast),
    ("broadcast", PaletteAction::ToggleBroadcast),
    ("command palette", PaletteAction::OpenPalette),
    ("palette", PaletteAction::OpenPalette),
    ("toggle settings", PaletteAction::ToggleSettings),
    ("font increase", PaletteAction::FontInc),
    ("font bigger", PaletteAction::FontInc),
    ("font decrease", PaletteAction::FontDec),
    ("font smaller", PaletteAction::FontDec),
    ("font reset", PaletteAction::FontReset),
    ("close pane", PaletteAction::CloseFocusedPane),
    ("prompt prev", PaletteAction::JumpPromptPrev),
    ("previous prompt", PaletteAction::JumpPromptPrev),
    ("prompt next", PaletteAction::JumpPromptNext),
    ("next prompt", PaletteAction::JumpPromptNext),
];

/// resolve a keybindings.conf action label to an action — the palette entries
/// plus the keybinding-only aliases (copy/paste/find/font/select-tab/etc.)
fn action_from_label(name: &str) -> Option<PaletteAction> {
    let n = name.trim();
    if let Some((_, a)) = all_palette_actions().iter().find(|(l, _)| l.eq_ignore_ascii_case(n)) {
        return Some(*a);
    }
    let lower = n.to_ascii_lowercase();
    if let Some(d) = lower.strip_prefix("select tab ")
        && let Ok(num) = d.trim().parse::<u8>()
        && (1..=9).contains(&num)
    {
        return Some(PaletteAction::SelectTab((num - 1) as usize));
    }
    KEYBIND_ALIASES.iter().find(|(l, _)| l.eq_ignore_ascii_case(n)).map(|(_, a)| *a)
}

/// the canonical label for an action, for the keybindings.conf template: the
/// palette label when it has one, else the keybinding-only alias
fn action_label(action: PaletteAction) -> Option<String> {
    if let PaletteAction::SelectTab(n) = action {
        return Some(format!("select tab {}", n + 1));
    }
    PALETTE_ACTIONS
        .iter()
        .chain(KEYBIND_ALIASES)
        .find(|(_, a)| *a == action)
        .map(|(l, _)| l.to_string())
}

/// unescape a keybindings `send` payload: \r and \n both type enter (typed
/// input is CR), \t tab, \e escape, \\ a literal backslash; any other pair
/// stays as written
fn unescape_send_input(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') | Some('n') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// apply a keybindings.conf body onto `out`, returning how many lines were
/// ignored — no '=', an unparseable combo, or an unknown action; each also warns.
/// `combo=none` (or `unbind`) frees a chord and doesn't count as ignored.
/// `combo=send <text>` interns its payload in `sends`
fn apply_keybindings_conf(
    text: &str,
    out: &mut Vec<(ModifiersState, Key, PaletteAction)>,
    sends: &mut Vec<String>,
) -> usize {
    let mut ignored = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((combo, action)) = line.split_once('=') else {
            log::warn!("keybindings.conf: no '=' in line: {line}");
            ignored += 1;
            continue;
        };
        let Some((mods, key)) = parse_combo(combo.trim()) else {
            log::warn!("keybindings.conf: unparseable combo: {}", combo.trim());
            ignored += 1;
            continue;
        };
        let action = action.trim();
        // a user line replaces any default (or earlier line) for the same combo
        out.retain(|(m, k, _)| !(*m == mods && key_matches(&key, k)));
        if action.eq_ignore_ascii_case("none") || action.eq_ignore_ascii_case("unbind") {
            continue;
        }
        // `send <text>` types the text into the focused pane (WT's sendInput)
        if action.len() > 5 && action[..5].eq_ignore_ascii_case("send ") {
            let payload = unescape_send_input(action[5..].trim_start());
            if payload.is_empty() {
                log::warn!("keybindings.conf: empty send payload (combo {})", combo.trim());
                ignored += 1;
                continue;
            }
            out.push((mods, key, PaletteAction::SendInput(sends.len())));
            sends.push(payload);
            continue;
        }
        match action_from_label(action) {
            Some(a) => out.push((mods, key, a)),
            None => {
                log::warn!("keybindings.conf: unknown action '{action}' (combo {})", combo.trim());
                ignored += 1;
            }
        }
    }
    ignored
}

/// load keybindings (built-in defaults + user overrides from
/// %APPDATA%\termie\keybindings.conf) with the interned `send` payloads and
/// the count of ignored config lines
fn load_keybindings() -> (Vec<(ModifiersState, Key, PaletteAction)>, Vec<String>, usize) {
    let mut out = default_keybindings();
    let mut sends = Vec::new();
    let Some(dir) = app_dir() else {
        return (out, sends, 0);
    };
    let path = dir.join("keybindings.conf");
    let Ok(text) = std::fs::read_to_string(path) else {
        return (out, sends, 0);
    };
    let ignored = apply_keybindings_conf(&text, &mut out, &mut sends);
    (out, sends, ignored)
}

/// a fully commented keybindings.conf listing every action with its default
/// chord, generated from default_keybindings + PALETTE_ACTIONS so it never drifts
fn keybindings_template() -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str(
        "# termie keybindings\n\
         #\n\
         # each line is `combo = action`. a combo joins modifiers (ctrl, shift, alt,\n\
         # super) and a key with + — a letter/digit, or a name like enter tab esc up\n\
         # down left right home end pageup pagedown insert delete f1..f12, e.g.\n\
         # ctrl+shift+t. set the action to `none` (or `unbind`) to free a chord.\n\
         #\n\
         # `send <text>` types text into the focused pane, like typing it:\n\
         #   ctrl+alt+g = send git status\\r\n\
         # escapes: \\r or \\n = enter, \\t = tab, \\e = escape, \\\\ = backslash.\n\
         #\n\
         # every line below is commented out and shows the built-in default;\n\
         # uncomment and edit to change it.\n\n",
    );
    let mut shown: Vec<PaletteAction> = Vec::new();
    for (m, k, a) in default_keybindings() {
        if let (Some(combo), Some(label)) = (combo_label(&m, &k), action_label(a)) {
            let _ = writeln!(out, "# {combo} = {label}");
            shown.push(a);
        }
    }
    out.push_str("\n# no default chord (add a combo before the =):\n");
    for (label, action) in PALETTE_ACTIONS {
        if !shown.contains(action) {
            let _ = writeln!(out, "# <combo> = {label}");
        }
    }
    out
}

/// write the commented template on first run so keybindings.conf is discoverable;
/// never touches an existing config
fn write_keybindings_template_if_absent() {
    let Some(dir) = app_dir() else {
        return;
    };
    let path = dir.join("keybindings.conf");
    if path.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, keybindings_template());
}

/// the git branch (or short detached hash) for a cwd, walking up to the repo root.
/// reads at most a few hundred bytes of .git/HEAD and caps the walk depth so a
/// hostile cwd / oversized HEAD can't hang or OOM the UI thread
fn git_branch(cwd: Option<&str>) -> Option<String> {
    use std::io::Read;
    let mut dir = std::path::PathBuf::from(cwd_path(cwd)?);
    for _ in 0..64 {
        let head = dir.join(".git").join("HEAD");
        if let Ok(f) = std::fs::File::open(&head) {
            let mut s = String::new();
            if f.take(256).read_to_string(&mut s).is_ok() {
                let s = s.trim();
                if let Some(b) = s.strip_prefix("ref: refs/heads/") {
                    return Some(b.to_string());
                }
                return Some(s.chars().take(7).collect());
            }
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// split the leaf with `id` into [old | new] using `dir`; `new` is taken on success
fn split_pane(node: Node, id: usize, dir: Dir, new: &mut Option<Pane>) -> Node {
    match node {
        Node::Leaf(p) => {
            if p.id == id
                && let Some(np) = new.take() {
                    return Node::Split {
                        dir,
                        ratio: 0.5,
                        a: Box::new(Node::Leaf(p)),
                        b: Box::new(Node::Leaf(np)),
                    };
                }
            Node::Leaf(p)
        }
        Node::Split { dir: d, ratio, a, b } => {
            let a2 = split_pane(*a, id, dir, new);
            let b2 = if new.is_some() {
                split_pane(*b, id, dir, new)
            } else {
                *b
            };
            Node::Split {
                dir: d,
                ratio,
                a: Box::new(a2),
                b: Box::new(b2),
            }
        }
    }
}

fn insert_pane(node: Node, id: usize, pane: &mut Option<Pane>, side: PaneDropSide) -> Node {
    match node {
        Node::Leaf(target) => {
            if target.id != id {
                return Node::Leaf(target);
            }
            let Some(moved) = pane.take() else {
                return Node::Leaf(target);
            };
            let (dir, first, second) = match side {
                PaneDropSide::Left => (Dir::Vertical, Node::Leaf(moved), Node::Leaf(target)),
                PaneDropSide::Right => (Dir::Vertical, Node::Leaf(target), Node::Leaf(moved)),
                PaneDropSide::Top => (Dir::Horizontal, Node::Leaf(moved), Node::Leaf(target)),
                PaneDropSide::Bottom => (Dir::Horizontal, Node::Leaf(target), Node::Leaf(moved)),
            };
            Node::Split { dir, ratio: 0.5, a: Box::new(first), b: Box::new(second) }
        }
        Node::Split { dir, ratio, a, b } => {
            let a = insert_pane(*a, id, pane, side);
            let b = if pane.is_some() { insert_pane(*b, id, pane, side) } else { *b };
            Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }
        }
    }
}

fn pane_drop_side(rect: Rect, x: f32, y: f32) -> PaneDropSide {
    let (rx, ry, rw, rh) = rect;
    let dx = (x - (rx + rw / 2.0)) / rw.max(1.0);
    let dy = (y - (ry + rh / 2.0)) / rh.max(1.0);
    if dx.abs() >= dy.abs() {
        if dx < 0.0 { PaneDropSide::Left } else { PaneDropSide::Right }
    } else if dy < 0.0 {
        PaneDropSide::Top
    } else {
        PaneDropSide::Bottom
    }
}

fn pane_tab_drop_index(hit: Hit, tab_count: usize) -> Option<usize> {
    match hit {
        Hit::Button(Hot::Tab(index) | Hot::TabClose(index)) => Some(index),
        Hit::TitleBar | Hit::Button(Hot::NewTab | Hot::NewTabMenu) => Some(tab_count),
        _ => None,
    }
}

/// remove the leaf with `id`, collapsing its parent, and hand the pane back
/// (alive — for tearing it off into its own window) via `out`
fn extract_pane(node: Node, id: usize, out: &mut Option<Pane>) -> Option<Node> {
    match node {
        Node::Leaf(p) => {
            if p.id == id {
                *out = Some(p);
                None
            } else {
                Some(Node::Leaf(p))
            }
        }
        Node::Split { dir, ratio, a, b } => {
            let a2 = extract_pane(*a, id, out);
            let b2 = extract_pane(*b, id, out);
            match (a2, b2) {
                (Some(a), Some(b)) => Some(Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            }
        }
    }
}

/// remove the leaf with `id`, collapsing its parent; returns the surviving tree
fn close_pane(node: Node, id: usize) -> Option<Node> {
    match node {
        Node::Leaf(mut p) => {
            if p.id == id {
                p.pty.kill();
                None
            } else {
                Some(Node::Leaf(p))
            }
        }
        Node::Split { dir, ratio, a, b } => {
            let a2 = close_pane(*a, id);
            let b2 = close_pane(*b, id);
            match (a2, b2) {
                (Some(a), Some(b)) => Some(Node::Split {
                    dir,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            }
        }
    }
}

/// the two child rects of a split given its rect + ratio (matches `layout`)
fn split_rects(dir: Dir, rect: Rect, ratio: f32) -> (Rect, Rect) {
    let (x, y, w, h) = rect;
    let r = ratio.clamp(0.1, 0.9);
    match dir {
        Dir::Vertical => {
            let s = (w * r).floor().max(1.0).min(w - 1.0);
            ((x, y, s, h), (x + s, y, w - s, h))
        }
        Dir::Horizontal => {
            let s = (h * r).floor().max(1.0).min(h - 1.0);
            ((x, y, w, s), (x, y + s, w, h - s))
        }
    }
}

/// path (a=0/b=1 turns) to the split whose divider is within `margin` of (x,y)
fn find_divider(node: &Node, rect: Rect, x: f32, y: f32, margin: f32, path: &mut Vec<usize>) -> Option<Vec<usize>> {
    let Node::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let (rx, ry, rw, rh) = rect;
    let (ar, br) = split_rects(*dir, rect, *ratio);
    let on_divider = match dir {
        Dir::Vertical => (x - (rx + ar.2)).abs() <= margin && y >= ry && y < ry + rh,
        Dir::Horizontal => (y - (ry + ar.3)).abs() <= margin && x >= rx && x < rx + rw,
    };
    if on_divider {
        return Some(path.clone());
    }
    let into_a = match dir {
        Dir::Vertical => x < rx + ar.2,
        Dir::Horizontal => y < ry + ar.3,
    };
    let (child, crect, idx) = if into_a { (a.as_ref(), ar, 0) } else { (b.as_ref(), br, 1) };
    path.push(idx);
    if let Some(p) = find_divider(child, crect, x, y, margin, path) {
        return Some(p);
    }
    path.pop();
    None
}

/// direction of the split whose divider sits under (x, y), if any
fn divider_dir(node: &Node, rect: Rect, x: f32, y: f32, margin: f32) -> Option<Dir> {
    let Node::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let (rx, ry, rw, rh) = rect;
    let (ar, br) = split_rects(*dir, rect, *ratio);
    let on_divider = match dir {
        Dir::Vertical => (x - (rx + ar.2)).abs() <= margin && y >= ry && y < ry + rh,
        Dir::Horizontal => (y - (ry + ar.3)).abs() <= margin && x >= rx && x < rx + rw,
    };
    if on_divider {
        return Some(*dir);
    }
    let into_a = match dir {
        Dir::Vertical => x < rx + ar.2,
        Dir::Horizontal => y < ry + ar.3,
    };
    let (child, crect) = if into_a { (a.as_ref(), ar) } else { (b.as_ref(), br) };
    divider_dir(child, crect, x, y, margin)
}

/// set the ratio of the split at `path` from the cursor position within its rect
fn set_divider_ratio(node: &mut Node, rect: Rect, path: &[usize], x: f32, y: f32) {
    let Node::Split { dir, ratio, a, b } = node else {
        return;
    };
    let (rx, ry, rw, rh) = rect;
    if path.is_empty() {
        *ratio = match dir {
            Dir::Vertical => ((x - rx) / rw).clamp(0.1, 0.9),
            Dir::Horizontal => ((y - ry) / rh).clamp(0.1, 0.9),
        };
        return;
    }
    let (ar, br) = split_rects(*dir, rect, *ratio);
    if path[0] == 0 {
        set_divider_ratio(a, ar, &path[1..], x, y);
    } else {
        set_divider_ratio(b, br, &path[1..], x, y);
    }
}

/// grow or shrink the pane with `id` by nudging the ratio of its nearest
/// ancestor split of orientation `dir`. returns true if a split was adjusted
fn grow_focused(node: &mut Node, id: usize, dir: Dir, grow: bool, step: f32, done: &mut bool) -> bool {
    match node {
        Node::Leaf(p) => p.id == id,
        Node::Split { dir: d, ratio, a, b } => {
            let in_a = grow_focused(a, id, dir, grow, step, done);
            let in_b = !in_a && grow_focused(b, id, dir, grow, step, done);
            if (in_a || in_b) && !*done && *d == dir {
                *done = true;
                // ratio is the A child's fraction: growing a pane on the A side
                // raises it, on the B side lowers it (and vice-versa for shrink)
                let delta = if in_a == grow { step } else { -step };
                *ratio = (*ratio + delta).clamp(0.1, 0.9);
            }
            in_a || in_b
        }
    }
}

fn kill_all(node: &mut Node) {
    match node {
        Node::Leaf(p) => p.pty.kill(),
        Node::Split { a, b, .. } => {
            kill_all(a);
            kill_all(b);
        }
    }
}

fn each_pane_mut(node: &mut Node, f: &mut impl FnMut(&mut Pane)) {
    match node {
        Node::Leaf(p) => f(p),
        Node::Split { a, b, .. } => {
            each_pane_mut(a, f);
            each_pane_mut(b, f);
        }
    }
}

/// fold two panes' OSC 9;4 progress values into the one the taskbar shows:
/// error beats paused beats normal beats indeterminate; ties take the larger
/// percentage. (0, 0) means no progress anywhere
fn merge_progress(a: (u8, u8), b: (u8, u8)) -> (u8, u8) {
    fn rank(state: u8) -> u8 {
        match state {
            2 => 4,
            4 => 3,
            1 => 2,
            3 => 1,
            _ => 0,
        }
    }
    match rank(a.0).cmp(&rank(b.0)) {
        std::cmp::Ordering::Greater => a,
        std::cmp::Ordering::Less => b,
        std::cmp::Ordering::Equal => {
            if a.1 >= b.1 {
                a
            } else {
                b
            }
        }
    }
}

/// number of leaf panes in a tree
fn pane_count(node: &Node) -> usize {
    let mut n = 0;
    each_pane(node, &mut |_| n += 1);
    n
}

fn each_pane(node: &Node, f: &mut impl FnMut(&Pane)) {
    match node {
        Node::Leaf(p) => f(p),
        Node::Split { a, b, .. } => {
            each_pane(a, f);
            each_pane(b, f);
        }
    }
}

/// discover enabled plugins under %APPDATA%\termie\plugins\<id>\, reading each
/// plugin.json for its entry command. returns (id, program, args) per plugin.
/// missing dir or malformed manifests are skipped silently — a bad plugin must
/// never block startup. (the marketplace + enable/disable arrive in later
/// phases; for now every installed plugin with a valid manifest is launched)
/// scan the plugins dir, returning every plugin with a valid manifest (enabled
/// or not — the marketplace UI needs to see disabled ones too). the directory
/// name is the trusted id; the manifest is validated against it so a plugin can
/// never claim another identity or escape its directory
fn discover_plugins() -> Vec<Discovered> {
    let Some(base) = plugins_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let states = load_plugin_states();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(dir.join("plugin.json")) else {
            continue;
        };
        let Some(manifest) = plugin::Manifest::parse(&text, dir_name) else {
            // parse() already logged the reason
            continue;
        };
        // resolve the entry program: relative paths stay inside the plugin dir;
        // an absolute cmd is allowed only for locally hand-installed plugins
        let prog = {
            let p = std::path::Path::new(&manifest.cmd);
            if p.is_absolute() {
                manifest.cmd.clone()
            } else {
                dir.join(&manifest.cmd).to_string_lossy().into_owned()
            }
        };
        // unknown-to-cfg plugins default to enabled, no granted perms
        let st = states.get(&manifest.id).cloned().unwrap_or(PluginState {
            enabled: true,
            granted: Vec::new(),
        });
        out.push(Discovered {
            manifest,
            program: prog,
            enabled: st.enabled,
            granted: st.granted,
        });
    }
    out
}

/// spawn one plugin, confined to the OS sandbox (windows appcontainer / linux
/// bwrap jail) when `sandbox` is set, otherwise as a normal child process. the
/// sandboxed path needs the plugin's install dir (working dir + the one mount
/// it may read) and maps the `network` permission to outbound network access
fn spawn_plugin(
    sandbox: bool,
    id: &str,
    d: &Discovered,
    on_msg: impl Fn(plugin::PluginMsg) + Send + 'static,
) -> std::io::Result<plugin::Plugin> {
    if sandbox
        && let Some(dir) = plugins_dir().map(|b| b.join(&d.manifest.id))
    {
        let net = d.granted.iter().any(|g| g == "network");
        let moniker = plugin::sandbox::moniker_for(&d.manifest.id);
        return plugin::Plugin::spawn_sandboxed(
            id.to_string(),
            &moniker,
            std::path::Path::new(&d.program),
            &d.manifest.args,
            &dir,
            net,
            on_msg,
        );
    }
    plugin::Plugin::spawn(id.to_string(), &d.program, &d.manifest.args, on_msg)
}

/// build a pane (pty + child + screen) without starting its reader thread.
/// the slow part (process spawn) — safe to run off the main thread
/// parsed command line. a bare initial launch can restore the saved session;
/// a bare launch forwarded into the running app opens one fresh window
#[derive(Clone, Default)]
struct CliArgs {
    cwd: Option<String>,
    /// shell label or custom profile name for the first tab (jump list, scripts)
    shell: Option<String>,
    command: Option<Vec<String>>,
    /// wt-style layout built from `new-tab` / `split-pane` verbs; non-empty
    /// makes the launch ephemeral and installs these tabs instead of tab one
    tabs: Vec<session::TabSnap>,
    /// `--drive <file>`: a timed key script injected through the normal input
    /// path once the first shell produces output — scripting, demos, and
    /// automation without synthesized OS input. the window opens non-activating
    /// so a drive run never touches whatever the user is doing
    drive: Option<String>,
    /// inject a kitty-graphics gradient into the first pane once it's up:
    /// ConPTY strips APC, so no shell can deliver one — this is the way to
    /// see the decoder + image pipeline work on a live window
    kitty_demo: bool,
    /// the Linux GUI stays user-owned while its first child shell is elevated
    admin_shell: bool,
}

impl CliArgs {
    fn is_bare(&self) -> bool {
        self.cwd.is_none()
            && self.shell.is_none()
            && self.command.is_none()
            && self.tabs.is_empty()
            && !self.admin_shell
    }
}

/// parse `termie [--cwd DIR | -d DIR | --cwd=DIR] [--shell NAME] [-- COMMAND...]`,
/// or wt-style layout verbs when the first argument is one:
/// `termie new-tab [-d DIR] [--shell NAME] ; split-pane [-H|-V] [-d DIR] ...`.
/// lenient and silent: release is a windowed subsystem with no console to print
/// help to, so unknown flags are ignored rather than erroring. `--` ends option
/// parsing and the remainder is a command argv to run instead of the default shell
fn parse_args<I: Iterator<Item = String>>(args: I) -> CliArgs {
    let mut out = CliArgs::default();
    let mut it = args.peekable();
    if matches!(it.peek().map(String::as_str), Some("new-tab" | "nt" | "split-pane" | "sp")) {
        out.tabs = parse_layout_verbs(&it.collect::<Vec<_>>());
        return out;
    }
    while let Some(a) = it.next() {
        if a == "--" {
            let rest: Vec<String> = it.by_ref().collect();
            if !rest.is_empty() {
                out.command = Some(rest);
            }
            break;
        } else if a == "--cwd" || a == "-d" {
            if let Some(dir) = it.next() {
                out.cwd = Some(dir);
            }
        } else if let Some(dir) = a.strip_prefix("--cwd=").or_else(|| a.strip_prefix("-d=")) {
            out.cwd = Some(dir.to_string());
        } else if a == "--shell" {
            if let Some(name) = it.next() {
                out.shell = Some(name);
            }
        } else if let Some(name) = a.strip_prefix("--shell=") {
            out.shell = Some(name.to_string());
        } else if a == "--kitty-demo" {
            out.kitty_demo = true;
        } else if a == "--admin-shell" {
            out.admin_shell = true;
        } else if a == "--drive" {
            if let Some(path) = it.next() {
                out.drive = Some(path);
            }
        } else if let Some(path) = a.strip_prefix("--drive=") {
            out.drive = Some(path.to_string());
        }
    }
    out
}

fn launch_can_join(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        arg == "--drive"
            || arg.starts_with("--drive=")
            || arg == "--admin-shell"
            || arg.eq_ignore_ascii_case("-embedding")
            || arg.eq_ignore_ascii_case("/embedding")
    })
}

fn resolve_launch_path(path: &str, base: Option<&str>) -> String {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    base.map(|base| std::path::Path::new(base).join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn resolve_layout_dirs(node: &mut session::NodeSnap, base: Option<&str>) {
    match node {
        session::NodeSnap::Leaf { cwd, .. } => {
            *cwd = cwd
                .as_deref()
                .map(|cwd| resolve_launch_path(cwd, base))
                .or_else(|| base.map(str::to_string));
        }
        session::NodeSnap::Split { a, b, .. } => {
            resolve_layout_dirs(a, base);
            resolve_layout_dirs(b, base);
        }
    }
}

fn drag_window_origin(
    point: PhysicalPosition<i32>,
    grab: PhysicalPosition<f64>,
) -> PhysicalPosition<i32> {
    PhysicalPosition::new(
        point.x - grab.x.round() as i32,
        point.y - grab.y.round() as i32,
    )
}

#[cfg(target_os = "linux")]
fn kwin_drag_title(title: &str, index: usize) -> String {
    format!("{title}\u{2063}\u{2063}\u{2063}{}", "\u{200b}".repeat(index + 1))
}

/// one scheduled `--drive` step
#[derive(Clone)]
enum DriveStep {
    Key(ModifiersState, Key),
    Type(String),
    Pointer(PhysicalPosition<f64>),
    Mouse(ElementState),
}

/// a parsed `--drive` script: steps at cumulative offsets from the moment the
/// first pane produces output (so scripts never race a cold shell)
struct Drive {
    steps: Vec<(Duration, DriveStep)>,
    next: usize,
    started: Option<Instant>,
}

/// parse a drive script: key, type, pointer, or left-mouse steps with delays
/// relative to the previous line; '#' lines are comments
fn parse_drive_script(text: &str) -> Vec<(Duration, DriveStep)> {
    let mut out = Vec::new();
    let mut at = Duration::ZERO;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (Some(ms), Some(verb)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(ms) = ms.parse::<u64>() else {
            continue;
        };
        let arg = parts.next().unwrap_or("");
        at += Duration::from_millis(ms);
        match verb {
            "key" => {
                if let Some((mods, key)) = parse_combo(arg) {
                    out.push((at, DriveStep::Key(mods, key)));
                }
            }
            "type" => out.push((at, DriveStep::Type(arg.to_string()))),
            "pointer" => {
                let mut coords = arg.split_whitespace();
                if let (Some(x), Some(y), None) = (coords.next(), coords.next(), coords.next())
                    && let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>())
                    && x.is_finite() && y.is_finite()
                {
                    out.push((at, DriveStep::Pointer(PhysicalPosition::new(x, y))));
                }
            }
            "mouse" => match arg {
                "down" => out.push((at, DriveStep::Mouse(ElementState::Pressed))),
                "up" => out.push((at, DriveStep::Mouse(ElementState::Released))),
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// wt-style layout verbs, segments separated by standalone `;` tokens. each
/// `split-pane` splits the current tab's newest pane; `-V` (the default) puts
/// the new pane beside it, `-H` below it. a leading split-pane implies a first
/// tab, matching wt
fn parse_layout_verbs(args: &[String]) -> Vec<session::TabSnap> {
    // the newest leaf is always on the rightmost spine: every split replaces
    // it with Split { a: old, b: new }, so descending `b` finds it
    fn split_newest(node: &mut session::NodeSnap, vertical: bool, leaf: session::NodeSnap) {
        match node {
            session::NodeSnap::Split { b, .. } => split_newest(b, vertical, leaf),
            session::NodeSnap::Leaf { .. } => {
                let old = std::mem::replace(node, session::NodeSnap::Leaf { cwd: None, shell: String::new() });
                *node = session::NodeSnap::Split { vertical, ratio: 0.5, a: Box::new(old), b: Box::new(leaf) };
            }
        }
    }

    let mut tabs: Vec<session::TabSnap> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let end = args[i..].iter().position(|a| a == ";").map(|p| i + p).unwrap_or(args.len());
        let seg = &args[i..end];
        i = end + 1;
        let Some(verb) = seg.first().map(String::as_str) else {
            continue;
        };
        let (mut cwd, mut shell, mut vertical) = (None, None, true);
        let mut j = 1;
        while j < seg.len() {
            let a = seg[j].as_str();
            match a {
                "-H" | "--horizontal" => vertical = false,
                "-V" | "--vertical" => vertical = true,
                "-d" | "--cwd" => {
                    j += 1;
                    cwd = seg.get(j).cloned();
                }
                "--shell" | "-p" | "--profile" => {
                    j += 1;
                    shell = seg.get(j).cloned();
                }
                _ => {
                    if let Some(v) = a.strip_prefix("--cwd=").or_else(|| a.strip_prefix("-d=")) {
                        cwd = Some(v.to_string());
                    } else if let Some(v) = a.strip_prefix("--shell=").or_else(|| a.strip_prefix("--profile=")) {
                        shell = Some(v.to_string());
                    }
                }
            }
            j += 1;
        }
        // an empty shell label rebuilds as ShellKind::Auto, the default
        let leaf = session::NodeSnap::Leaf { cwd, shell: shell.unwrap_or_default() };
        match verb {
            "new-tab" | "nt" => {
                tabs.push(session::TabSnap { focused_leaf: 0, root: leaf, title: None, color: None });
            }
            "split-pane" | "sp" => match tabs.last_mut() {
                Some(tab) => {
                    split_newest(&mut tab.root, vertical, leaf);
                    tab.focused_leaf += 1;
                }
                None => tabs.push(session::TabSnap { focused_leaf: 0, root: leaf, title: None, color: None }),
            },
            _ => {}
        }
    }
    tabs
}

/// a 96x96 gradient as real kitty-graphics bytes (chunked transmit+display),
/// pushed through the same pump as pty output so the whole APC → store → GPU
/// path is exercised; a trailing caption prints below the image
fn kitty_demo_bytes() -> Vec<u8> {
    const W: usize = 96;
    const H: usize = 96;
    let mut px = Vec::with_capacity(W * H * 3);
    for y in 0..H {
        for x in 0..W {
            px.push((255 * x / (W - 1)) as u8);
            px.push((255 * y / (H - 1)) as u8);
            px.push(96);
        }
    }
    let b64 = base64_encode(&px);
    // caption first: the placement lands below it whatever ConPTY repaints do,
    // so the words stay true on screen
    let mut out = Vec::from(&b"kitty demo: the gradient below went through the real APC path\r\n"[..]);
    let mut pos = 0;
    while pos < b64.len() {
        let end = (pos + 4096).min(b64.len());
        let more = if end < b64.len() { 1 } else { 0 };
        if pos == 0 {
            out.extend_from_slice(format!("\x1b_Ga=T,f=24,s={W},v={H},m={more};").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(&b64[pos..end]);
        out.extend_from_slice(b"\x1b\\");
        pos = end;
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn base64_encode(data: &[u8]) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63]);
        out.push(T[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { T[n as usize & 63] } else { b'=' });
    }
    out
}

/// the working directory a bare launch should open its first tab in, or None to
/// fall through to session restore. running `termie` from a shell sitting in a
/// repo launches with that folder as the process cwd, so open there, the way cmd
/// does. the explorer address bar instead launches a windows-subsystem app with
/// no working dir, so we inherit explorer's home dir and the process cwd is
/// useless — when that happens, recover the folder from the explorer window we
/// were launched from (`fg`). the start menu / desktop / taskbar / run-box
/// launches all land in the home dir with a non-explorer foreground window, so
/// they return None and restore the saved session instead
fn launch_cwd(fg: isize) -> Option<String> {
    let home = home_dir();
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(std::path::Path::to_path_buf));
    // dirs we were handed incidentally rather than chosen by the user: the home
    // dir (start-menu / desktop / taskbar / run-box launches) and the exe's own
    // folder (double-clicking termie.exe lands its cwd there). opening a tab in
    // either is surprising, so neither counts as a launch folder.
    let incidental = |p: &std::path::Path| {
        home.as_ref().is_some_and(|h| same_dir(p, std::path::Path::new(h)))
            || exe_dir.as_deref().is_some_and(|d| same_dir(p, d))
    };
    // a real process cwd the user launched in (a shell sitting in a repo) wins
    if let Some(cwd) = std::env::current_dir().ok().filter(|c| c.is_dir())
        && !incidental(&cwd)
    {
        return Some(cwd.to_string_lossy().into_owned());
    }
    // otherwise the cwd was incidental: if an explorer window launched us, open in
    // the folder it was showing (its address bar passes no working dir of its own),
    // unless that folder is itself incidental (e.g. double-clicked from the exe's
    // own folder), in which case restore the saved session instead
    let dir = cwd_path(win::explorer_dir_for(fg).as_deref())?;
    let p = std::path::Path::new(&dir);
    if !p.is_dir() || incidental(p) {
        return None;
    }
    Some(dir)
}

/// path equality that ignores a trailing separator, resolving each side first
/// so e.g. C:\Users\me and C:\Users\me\ compare equal. windows filesystems are
/// case-insensitive, so the comparison folds case there; unix ones are not
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        let s = std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned();
        #[cfg(windows)]
        return s.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
        #[cfg(not(windows))]
        return s.trim_end_matches('/').to_string();
    };
    norm(a) == norm(b)
}

#[cfg(not(windows))]
fn program_in_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

#[cfg(not(windows))]
fn linux_admin_command(has_pkexec: bool, has_sudo: bool) -> Option<&'static [&'static str]> {
    if has_pkexec {
        Some(&["pkexec", "--keep-cwd"])
    } else if has_sudo {
        Some(&["sudo", "-s"])
    } else {
        None
    }
}

/// snapshot a pane tree into the serializable session form, recording the
/// in-order leaf pane ids so the focused pane can be re-keyed after restore
fn node_to_snap(node: &Node, leaf_ids: &mut Vec<usize>) -> session::NodeSnap {
    match node {
        Node::Leaf(p) => {
            leaf_ids.push(p.id);
            session::NodeSnap::Leaf {
                cwd: cwd_path(p.term.cwd.as_deref()),
                shell: p.shell.label().to_string(),
            }
        }
        Node::Split { dir, ratio, a, b } => session::NodeSnap::Split {
            vertical: *dir == Dir::Vertical,
            ratio: *ratio,
            a: Box::new(node_to_snap(a, leaf_ids)),
            b: Box::new(node_to_snap(b, leaf_ids)),
        },
    }
}

/// available monitor rects (x, y, w, h) in physical pixels, primary first so it
/// is the fallback when a saved window overlaps none of them
fn monitor_rects(event_loop: &ActiveEventLoop) -> Vec<(i32, i32, u32, u32)> {
    let primary = event_loop.primary_monitor();
    let rect = |m: &winit::monitor::MonitorHandle| {
        let (p, s) = (m.position(), m.size());
        (p.x, p.y, s.width, s.height)
    };
    let mut rects: Vec<(i32, i32, u32, u32)> = primary.iter().map(rect).collect();
    for m in event_loop.available_monitors() {
        if Some(&m) != primary.as_ref() {
            rects.push(rect(&m));
        }
    }
    rects
}

/// overlap area of two (x, y, w, h) rects in physical pixels
fn rect_overlap(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> i64 {
    let w = ((a.0 + a.2 as i32).min(b.0 + b.2 as i32) - a.0.max(b.0)).max(0) as i64;
    let h = ((a.1 + a.3 as i32).min(b.1 + b.3 as i32) - a.1.max(b.1)).max(0) as i64;
    w * h
}

/// keep a saved window rect inside the monitor it most overlaps, or center it
/// on the primary monitor when its old display is gone. rects are physical px
fn clamp_window_bounds(
    monitors: &[(i32, i32, u32, u32)],
    b: (i32, i32, u32, u32),
) -> (i32, i32, u32, u32) {
    let target = monitors
        .iter()
        .copied()
        .filter(|&m| rect_overlap(m, b) > 0)
        .max_by_key(|&m| rect_overlap(m, b))
        .or_else(|| monitors.first().copied());
    let Some((mx, my, mw, mh)) = target else {
        return b; // no monitors known: leave the saved rect as-is
    };
    let w = b.2.min(mw);
    let h = b.3.min(mh);
    if rect_overlap((mx, my, mw, mh), b) > 0 {
        let x = b.0.clamp(mx, mx + mw as i32 - w as i32);
        let y = b.1.clamp(my, my + mh as i32 - h as i32);
        (x, y, w, h)
    } else {
        (mx + (mw as i32 - w as i32) / 2, my + (mh as i32 - h as i32) / 2, w, h)
    }
}

/// cap the client area so the complete outer window fits the monitor
fn monitor_inner_limit(
    monitor: PhysicalSize<u32>,
    inner: PhysicalSize<u32>,
    outer: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    PhysicalSize::new(
        monitor.width.saturating_sub(outer.width.saturating_sub(inner.width)).max(1),
        monitor.height.saturating_sub(outer.height.saturating_sub(inner.height)).max(1),
    )
}

/// update the live resize ceiling after a window crosses onto another screen
fn constrain_window_to_monitor(window: &Window) {
    let Some(monitor) = window.current_monitor().or_else(|| window.primary_monitor()) else {
        return;
    };
    let size = window.inner_size();
    let limit = monitor_inner_limit(monitor.size(), size, window.outer_size());
    window.set_max_inner_size(Some(limit));
    if size.width > limit.width || size.height > limit.height {
        let _ = window.request_inner_size(PhysicalSize::new(
            size.width.min(limit.width),
            size.height.min(limit.height),
        ));
    }
}

/// the layout key with no modifiers applied, for the kitty protocol's
/// un-shifted key codes (shift+2 reports as '2', not '@')
fn unshifted_char(event: &winit::event::KeyEvent) -> Option<char> {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    match event.key_without_modifiers() {
        Key::Character(s) => s.chars().next(),
        _ => None,
    }
}

/// feed pty output through the kitty-graphics scanner, then the vte parser. the
/// scanner pulls kitty APC image sequences out of the stream (vte has no APC
/// callback) and the remaining bytes flow to the terminal unchanged
fn pump_bytes(pane: &mut Pane, bytes: &[u8]) {
    let (pass, imgs) = pane.apc.feed(bytes);
    pane.parser.advance(&mut pane.term, pass);
    // images are rare; only walk the list when the scanner actually split one out
    if !imgs.is_empty() {
        let cmds: Vec<apc::KittyCmd> = imgs.iter().filter_map(|raw| apc::KittyCmd::parse(raw)).collect();
        for cmd in &cmds {
            handle_kitty(&mut pane.term, cmd);
        }
    }
}

/// apply a kitty graphics command to a pane's terminal: store/decode images,
/// anchor placements at the cursor and step the cursor past the box (the
/// spec's default movement policy; C=1 leaves it), delete, and queue the ack
fn handle_kitty(term: &mut Terminal, cmd: &apc::KittyCmd) {
    match cmd.action {
        b't' | b'T' => {
            // the display intent (its c=/r= box and cursor policy) belongs to
            // the a=T chunk; the store carries it across a chunked transfer,
            // whose completing chunk parses with the default action
            let display = (cmd.action == b'T').then(|| image::DisplayReq {
                cols: cmd.cols.min(500) as u16,
                rows: cmd.rows.min(500) as u16,
                step: !cmd.no_cursor_move,
                z: cmd.z,
                virt: cmd.unicode_placeholder,
            });
            if let Some((id, disp)) = term
                .images
                .transmit(cmd.id, cmd.format, cmd.width, cmd.height, cmd.more, display, &cmd.payload)
            {
                if let Some(d) = disp {
                    if d.virt {
                        // a virtual placement is only a prototype for unicode
                        // placeholder cells: nothing paints, the cursor holds
                        term.grid.set_virtual_placement(id, d.cols, d.rows);
                    } else {
                        term.grid.place_image(id, d.cols, d.rows, d.z);
                        let dims = term.images.get(id).map(|i| (i.width, i.height));
                        if d.step && let Some((w, h)) = dims {
                            term.advance_cursor_past_image(w, h, d.cols, d.rows);
                        }
                    }
                }
                // ack with the resolved id (an i=0 transmit gets an auto id)
                if cmd.quiet == 0 {
                    kitty_ok(term, id);
                }
            }
        }
        b'p' => {
            let dims = term.images.get(cmd.id).map(|i| (i.width, i.height));
            if let Some((w, h)) = dims {
                let (c, r) = (cmd.cols.min(500) as u16, cmd.rows.min(500) as u16);
                if cmd.unicode_placeholder {
                    term.grid.set_virtual_placement(cmd.id, c, r);
                } else {
                    term.grid.place_image(cmd.id, c, r, cmd.z);
                    if !cmd.no_cursor_move {
                        term.advance_cursor_past_image(w, h, c, r);
                    }
                }
                if cmd.quiet == 0 {
                    kitty_ok(term, cmd.id);
                }
            }
        }
        b'd' => {
            // d= names the delete target; an UPPERCASE letter also frees the
            // stored image data, but only once no surviving placement still
            // references the image (kitty: "provided that the image is not
            // referenced elsewhere"). absent d= defaults to 'a' per the spec,
            // except the legacy i=-only form which keeps freeing like before
            let sel = match (cmd.delete, cmd.id) {
                (0, 0) => b'a',
                (0, _) => b'I',
                (d, _) => d,
            };
            let free = sel.is_ascii_uppercase();
            let dropped = match sel.to_ascii_lowercase() {
                b'a' => {
                    let all: Vec<u32> =
                        term.grid.placements().iter().map(|p| p.image_id).collect();
                    term.grid.clear_placements();
                    all
                }
                b'i' if cmd.id != 0 => {
                    term.grid.remove_placements(cmd.id);
                    // i/I are the only delete targets that reach virtual (U=1)
                    // placements — the position-scoped ones never touch them
                    term.grid.remove_virtual_placement(cmd.id);
                    vec![cmd.id]
                }
                b'z' => term.grid.remove_placements_where(|p| p.z == cmd.z),
                // ranged delete: every image id in [x, y] inclusive; with i/I
                // this is a target that also reaches virtual placements. an
                // inverted range matches nothing, per the spec's wording
                b'r' => {
                    let (lo, hi) = (cmd.x, cmd.y);
                    term.grid.remove_placements_where(|p| (lo..=hi).contains(&p.image_id));
                    term.grid.remove_virtual_placements_in(lo, hi);
                    term.images.ids_in(lo, hi)
                }
                // position-scoped targets: a placement is hit when its cell
                // box (or its pixel size over the cell metrics) covers the
                // named cell, column, or row. c uses the cursor; p/q/x/y use
                // the 1-based x=/y= keys (0 = the key was absent → no match)
                s @ (b'c' | b'p' | b'q' | b'x' | b'y') => {
                    let (cw, ch) = term.cell_px();
                    let cw = if cw > 0 { cw as usize } else { 10 };
                    let ch = if ch > 0 { ch as usize } else { 20 };
                    let abs_of = |row: u64| {
                        term.grid.abs_base() + term.grid.scrollback.len() as u64 + row
                    };
                    // the cell each target names; None = that axis unconstrained
                    let (col, abs) = match s {
                        b'c' => (
                            Some(term.grid.cursor.col),
                            Some(abs_of(term.grid.cursor.row as u64)),
                        ),
                        b'x' => (cmd.x.checked_sub(1).map(|v| v as usize), None),
                        b'y' => (None, cmd.y.checked_sub(1).map(|v| abs_of(v as u64))),
                        _ => (
                            cmd.x.checked_sub(1).map(|v| v as usize),
                            cmd.y.checked_sub(1).map(|v| abs_of(v as u64)),
                        ),
                    };
                    // p and q need their cell fully named; x and y one axis
                    let valid = match s {
                        b'x' => col.is_some(),
                        b'y' => abs.is_some(),
                        _ => col.is_some() && abs.is_some(),
                    };
                    if !valid {
                        Vec::new()
                    } else {
                        let boxes: std::collections::HashMap<u32, (usize, usize)> = term
                            .grid
                            .placements()
                            .iter()
                            .map(|p| p.image_id)
                            .filter_map(|id| {
                                term.images.get(id).map(|i| {
                                    (id, ((i.width as usize).div_ceil(cw), (i.height as usize).div_ceil(ch)))
                                })
                            })
                            .collect();
                        term.grid.remove_placements_where(|p| {
                            let (nat_c, nat_r) = boxes.get(&p.image_id).copied().unwrap_or((1, 1));
                            let cols = if p.cols > 0 { p.cols as usize } else { nat_c };
                            let rows = if p.rows > 0 { p.rows as usize } else { nat_r };
                            col.map(|c| p.col <= c && c < p.col + cols.max(1)).unwrap_or(true)
                                && abs
                                    .map(|a| p.abs_line <= a && a < p.abs_line + rows.max(1) as u64)
                                    .unwrap_or(true)
                                && (s != b'q' || p.z == cmd.z)
                        })
                    }
                }
                // targets this v1 doesn't implement (n/f, or i without an
                // id): a scoped delete must never escalate to a wipe, so
                // they drop nothing
                _ => Vec::new(),
            };
            if free {
                for id in dropped {
                    // another placement of this image may have survived the
                    // scoped delete; the pixels stay until the last reference
                    // is gone (a virtual placement counts — placeholder cells
                    // still tile it)
                    if !term.grid.placements().iter().any(|p| p.image_id == id)
                        && term.grid.virtual_placement(id).is_none()
                    {
                        term.images.delete(id);
                    }
                }
            }
        }
        b'q' if cmd.quiet == 0 => {
            kitty_ok(term, cmd.id);
        }
        _ => {}
    }
}

/// queue a kitty "OK" APC response for the program (drained to the pty with the
/// other terminal responses)
fn kitty_ok(term: &mut Terminal, id: u32) {
    term.responses
        .extend_from_slice(format!("\x1b_Gi={id};OK\x1b\\").as_bytes());
}

// the spawn parameters are all distinct; a struct would only relocate them
#[allow(clippy::too_many_arguments)]
fn build_pane(
    id: usize,
    cols: usize,
    rows: usize,
    shell: ShellKind,
    load_profile: bool,
    scrollback: usize,
    cwd: Option<&str>,
    command: Option<&[String]>,
    wsl_distro: Option<&str>,
    term_program: &str,
    cell_w: u16,
    cell_h: u16,
) -> Result<Pane> {
    let pty = Pty::spawn(
        rows as u16,
        cols as u16,
        shell,
        load_profile,
        cwd,
        command,
        wsl_distro,
        term_program,
        cell_w,
        cell_h,
    )?;
    let mut term = Terminal::new(rows, cols);
    term.grid.set_scrollback_limit(scrollback);
    term.set_cell_px(cell_w, cell_h);
    Ok(Pane {
        id,
        term,
        parser: Parser::new(),
        pty,
        shell,
        ready: false,
        flash: None,
        apc: apc::ApcScanner::default(),
        status: PaneStatus::Idle,
    })
}

/// what the title-bar X button does
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    Quit,
    Minimize,
}

impl CloseAction {
    fn next(self) -> Self {
        match self {
            CloseAction::Quit => CloseAction::Minimize,
            CloseAction::Minimize => CloseAction::Quit,
        }
    }
    fn label(self) -> &'static str {
        match self {
            CloseAction::Quit => "quit",
            CloseAction::Minimize => "minimize",
        }
    }
    fn from_label(s: &str) -> Self {
        match s {
            "minimize" => CloseAction::Minimize,
            _ => CloseAction::Quit,
        }
    }
}

/// what a right-click on pane content does: open the context menu (default) or,
/// for windows-terminal muscle memory, copy-a-selection-else-paste
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RightClick {
    Menu,
    Paste,
}

impl RightClick {
    fn label(self) -> &'static str {
        match self {
            RightClick::Menu => "menu",
            RightClick::Paste => "paste",
        }
    }
    fn from_label(s: &str) -> Self {
        match s {
            "menu" => RightClick::Menu,
            "paste" => RightClick::Paste,
            other => {
                log::warn!("config: unknown right_click value `{other}` (use menu or paste); keeping menu");
                RightClick::Menu
            }
        }
    }
}

/// user-tunable settings backed by real effects (see the settings page)
#[derive(Clone, Copy)]
struct Config {
    scrollback: usize,
    copy_on_select: bool,
    shell: ShellKind,
    load_profile: bool,
    close_action: CloseAction,
    /// right-click on content: open the context menu, or copy/paste (WT muscle memory)
    right_click: RightClick,
    backend: render::BackendChoice,
    /// restore the saved tab/split layout on a bare launch
    restore_on_launch: bool,
    // global quake hotkey as (win32 modifiers, virtual-key); None disables it
    #[cfg(windows)]
    quake_key: Option<(u32, u32)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            scrollback: 10_000,
            copy_on_select: false,
            shell: ShellKind::Auto,
            load_profile: false,
            close_action: CloseAction::Quit,
            right_click: RightClick::Menu,
            backend: render::BackendChoice::Auto,
            restore_on_launch: true,
            #[cfg(windows)]
            quake_key: None,
        }
    }
}

/// all persisted settings (App-owned + renderer-owned), read at startup
struct Persisted {
    scrollback: usize,
    copy_on_select: bool,
    shell: ShellKind,
    load_profile: bool,
    close_action: CloseAction,
    right_click: RightClick,
    backend: render::BackendChoice,
    restore_on_launch: bool,
    font_size: f32,
    padding: f32,
    cursor: grid::CursorShape,
    cursor_blink: bool,
    bold_as_bright: bool,
    line_height: f32,
    theme: color::ThemeId,
    /// `theme=auto`: follow the OS light/dark setting, resolving to
    /// theme_dark / theme_light (which only matter while auto is on)
    theme_auto: bool,
    theme_dark: color::ThemeId,
    theme_light: color::ThemeId,
    font: Option<String>,
    /// base weight regular text renders at (`font_weight=`, named or 100-900)
    font_weight: u16,
    /// minimum WCAG contrast ratio text is lifted to against its cell bg
    /// (`min_contrast=`, 1-21; 1 = off). wt's minimumContrastRatio
    min_contrast: f32,
    /// window background image png (`background_image=` path) drawn behind
    /// panes at `background_image_opacity=` (0..1)
    background_image: Option<String>,
    background_image_opacity: f32,
    /// shape punctuation runs through the font's ligature rules (`ligatures=`,
    /// default on; fonts without liga/calt render identically)
    ligatures: bool,
    opacity: i32,
    #[cfg(windows)]
    quake_key: Option<(u32, u32)>,
    #[cfg(target_os = "linux")]
    quake_key: Option<String>,
    /// the quake_key line as written, kept so save_config preserves its spelling
    quake_key_raw: Option<String>,
    /// the WSL distribution `new tab: wsl` launches (None = wsl.exe default)
    wsl_distro: Option<String>,
    /// run plugins inside the OS sandbox for privilege isolation — a windows
    /// appcontainer (`plugin_sandbox=appcontainer`) or a linux bwrap jail
    /// (`plugin_sandbox=bwrap`); off by default
    plugin_sandbox: bool,
    /// system backdrop behind the window (`acrylic=true`, alias `mica`): mica
    /// on windows 11 and compositor blur on linux
    acrylic: bool,
    /// custom shell profiles, kept both raw as (`profile.` sub-key, value) so
    /// save_config re-emits every line the user wrote, and parsed into Profiles
    /// (argv plus optional .cwd / .env.<VAR> sub-keys)
    profiles_raw: Vec<(String, String)>,
    profiles: Vec<pty::Profile>,
    /// per-profile themes (`theme.<shell-or-profile>=<theme name>`): panes
    /// spawned as that shell/profile paint with that theme's palette while
    /// the window chrome keeps the global theme
    shell_themes: Vec<(String, color::ThemeId)>,
    /// paint pty output inline instead of via the request_redraw hop, shaving up
    /// to a frame of input-to-photon latency and staying tear-free under Fifo
    /// vsync; on by default — set `inline_paint=false` to use the redraw hop
    inline_paint: bool,
    /// draw the input-to-photon latency hud (`latency_hud=true`); off by default
    latency_hud: bool,
    /// daily update check against GitHub releases; `update_check=false` opts out
    update_check: bool,
    /// value of $TERM_PROGRAM for child processes. default "termie". set to a
    /// known host name (e.g. ghostty) only for apps that refuse the kitty
    /// keyboard protocol unless the name is on their allowlist
    term_program: String,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            scrollback: 10_000,
            copy_on_select: false,
            shell: ShellKind::Auto,
            load_profile: false,
            close_action: CloseAction::Quit,
            right_click: RightClick::Menu,
            backend: render::BackendChoice::Auto,
            restore_on_launch: true,
            font_size: CONTENT_PT,
            padding: 6.0,
            cursor: grid::CursorShape::Block,
            cursor_blink: true,
            bold_as_bright: true,
            line_height: 1.32,
            theme: color::ThemeId::Instrument,
            theme_auto: false,
            theme_dark: color::ThemeId::Instrument,
            theme_light: color::ThemeId::Paper,
            font: None,
            font_weight: 400,
            min_contrast: 1.0,
            background_image: None,
            background_image_opacity: 0.3,
            ligatures: true,
            opacity: 85,
            #[cfg(any(windows, target_os = "linux"))]
            quake_key: None,
            quake_key_raw: None,
            wsl_distro: None,
            plugin_sandbox: false,
            acrylic: false,
            profiles_raw: Vec::new(),
            profiles: Vec::new(),
            shell_themes: Vec::new(),
            inline_paint: true,
            latency_hud: false,
            update_check: true,
            term_program: String::from("termie"),
        }
    }
}

/// a small rolling meter of input-to-photon latency and frame intervals (ms),
/// feeding the optional latency hud. fixed-capacity so it never grows
#[derive(Default)]
struct LatencyMeter {
    input_ms: std::collections::VecDeque<f32>,
    frame_ms: std::collections::VecDeque<f32>,
}

impl LatencyMeter {
    fn record_input(&mut self, ms: f32) {
        push_capped(&mut self.input_ms, ms);
    }

    fn record_frame(&mut self, ms: f32) {
        push_capped(&mut self.frame_ms, ms);
    }

    /// a compact one-line summary, or None until there is data
    fn hud(&self) -> Option<String> {
        let lat = percentiles(&self.input_ms);
        let frame = percentiles(&self.frame_ms);
        match (lat, frame) {
            (Some((p50, p95)), Some((f50, _))) => {
                Some(format!("in->photon p50 {p50:.1}ms p95 {p95:.1}ms  frame {f50:.1}ms"))
            }
            (Some((p50, p95)), None) => Some(format!("in->photon p50 {p50:.1}ms p95 {p95:.1}ms")),
            (None, Some((f50, _))) => Some(format!("frame {f50:.1}ms")),
            (None, None) => None,
        }
    }
}

fn push_capped(q: &mut std::collections::VecDeque<f32>, v: f32) {
    const CAP: usize = 120;
    if q.len() == CAP {
        q.pop_front();
    }
    q.push_back(v);
}

/// (p50, p95) of a sample set, or None if empty
fn percentiles(q: &std::collections::VecDeque<f32>) -> Option<(f32, f32)> {
    if q.is_empty() {
        return None;
    }
    let mut v: Vec<f32> = q.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |p: f32| v[((v.len() as f32 * p) as usize).min(v.len() - 1)];
    Some((pick(0.50), pick(0.95)))
}

/// css/opentype weight, named or numeric; None leaves the setting untouched
fn font_weight_from_label(v: &str) -> Option<u16> {
    let named = match v.to_ascii_lowercase().as_str() {
        "thin" => 100,
        "extralight" => 200,
        "light" => 300,
        "normal" | "regular" => 400,
        "medium" => 500,
        "semibold" => 600,
        "bold" => 700,
        "extrabold" => 800,
        "black" => 900,
        _ => return v.parse::<u16>().ok().map(|n| n.clamp(100, 900)),
    };
    Some(named)
}

fn cursor_from_name(s: &str) -> grid::CursorShape {
    match s {
        "block" => grid::CursorShape::Block,
        "underline" => grid::CursorShape::Underline,
        _ => grid::CursorShape::Bar,
    }
}

/// whether a chrome button mutates a persisted setting (triggers a save)
fn is_settings_hot(h: Hot) -> bool {
    let common = matches!(
        h,
        Hot::FontDec
            | Hot::FontInc
            | Hot::FontCycle
            | Hot::PadDec
            | Hot::PadInc
            | Hot::OpacityDec
            | Hot::OpacityInc
            | Hot::CursorCycle
            | Hot::CursorBlink
            | Hot::ThemeSet(_)
            | Hot::ThemeAuto
            | Hot::LineHeightDec
            | Hot::LineHeightInc
            | Hot::BoldBright
            | Hot::ScrollbackDec
            | Hot::ScrollbackInc
            | Hot::CopyOnSelect
            | Hot::ShellCycle
            | Hot::LoadProfile
            | Hot::CloseActionCycle
            | Hot::BackendCycle
    );
    common || h == Hot::Mica
}

/// poll the hand-edited conf files (colors.conf, keybindings.conf) once a
/// second and post UserConfChanged when either differs — an edit applies live,
/// no restart. stat-polling keeps it dependency-free; two stats a second is
/// nothing. the config file is deliberately not watched: the app writes it
/// itself on every settings change, which would echo straight back here
fn spawn_conf_watcher(proxy: EventLoopProxy<UserEvent>) {
    let Some(base) = app_dir() else {
        return;
    };
    let paths: Vec<std::path::PathBuf> =
        ["colors.conf", "keybindings.conf"].iter().map(|f| base.join(f)).collect();
    std::thread::spawn(move || {
        let stat = |p: &std::path::PathBuf| {
            std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()))
        };
        let mut last: Vec<_> = paths.iter().map(stat).collect();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let cur: Vec<_> = paths.iter().map(stat).collect();
            if cur != last {
                last = cur;
                if proxy.send_event(UserEvent::UserConfChanged).is_err() {
                    return; // event loop is gone
                }
            }
        }
    });
}

#[cfg(not(windows))]
fn user_dir(env_name: &str, fallback: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(env_name)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(fallback)))
        .map(|p| p.join("termie"))
}

/// termie's per-user configuration directory
pub fn app_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")?;
        Some(std::path::PathBuf::from(base).join("termie"))
    }
    #[cfg(not(windows))]
    {
        user_dir("XDG_CONFIG_HOME", ".config")
    }
}

pub fn data_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    return app_dir();
    #[cfg(not(windows))]
    return user_dir("XDG_DATA_HOME", ".local/share");
}

pub fn state_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    return app_dir();
    #[cfg(not(windows))]
    return user_dir("XDG_STATE_HOME", ".local/state");
}

pub fn cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    return app_dir();
    #[cfg(not(windows))]
    return user_dir("XDG_CACHE_HOME", ".cache");
}

fn migrated_path(dir: std::path::PathBuf, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    #[cfg(not(windows))]
    if !path.exists()
        && let Some(legacy) = app_dir().map(|d| d.join(name))
        && legacy.exists()
    {
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::rename(&legacy, &path).is_ok() {
            return path;
        }
        return legacy;
    }
    path
}

/// the user's home directory (%USERPROFILE% / $HOME)
fn home_dir() -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    return std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    return std::env::var_os("HOME");
}

/// the `config` file under app_dir() — a simple key=value store for every setting
fn config_path() -> Option<std::path::PathBuf> {
    Some(app_dir()?.join("config"))
}

/// split a config command line into argv: whitespace separates, double quotes
/// group (`"C:\Program Files\Git\bin\bash.exe" -i -l` is three arguments)
fn split_cmdline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// write exported scrollback text into Downloads (or the profile dir when
/// Downloads is missing), timestamped so repeated exports never collide
fn export_scrollback(text: &str) -> std::io::Result<std::path::PathBuf> {
    let home = home_dir()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let downloads = home.join("Downloads");
    let dir = if downloads.is_dir() { downloads } else { home };
    let path = dir.join(format!("termie-scrollback-{}.txt", local_timestamp()));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// local wall-clock time as YYYYMMDD-HHMMSS for export filenames
#[cfg(windows)]
fn local_timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(unix)]
fn local_timestamp() -> String {
    let (y, mo, d, h, mi, s) = win::local_ymdhms();
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// the per-user state directory's `termie.log` sink. the release build is
/// a windowed app with no console, so without this every parser warning
/// (colors.conf typos, bad keybinding lines, unknown config keys) vanished
struct FileLog(std::sync::Mutex<std::fs::File>);

impl log::Log for FileLog {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(mut f) = self.0.lock() {
            use std::io::Write as _;
            let _ = writeln!(f, "{} [{}] {}", win::local_hm(), record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn install_file_log() {
    let Some(dir) = state_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("termie.log");
    // bound the file: start over once it passes ~512 KB
    let oversized = std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    if oversized {
        opts.write(true).truncate(true).create(true);
    } else {
        opts.append(true).create(true);
    }
    let Ok(f) = opts.open(&path) else {
        return;
    };
    if log::set_logger(Box::leak(Box::new(FileLog(std::sync::Mutex::new(f))))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

fn session_path() -> Option<std::path::PathBuf> {
    Some(migrated_path(state_dir()?, "session.json"))
}

/// installed plugin payloads are user data, separate from their configuration
fn plugins_dir() -> Option<std::path::PathBuf> {
    Some(migrated_path(data_dir()?, "plugins"))
}

/// `plugins.cfg` under app_dir() — per-plugin enabled state + granted perms.
/// one line per plugin: `id=on` or `id=off`, optionally `;perms=a,b`
fn plugins_cfg_path() -> Option<std::path::PathBuf> {
    Some(app_dir()?.join("plugins.cfg"))
}

/// persisted per-plugin state, keyed by plugin id
#[derive(Clone, Default)]
struct PluginState {
    enabled: bool,
    granted: Vec<String>,
}

/// load the plugins.cfg map (id -> state). missing file = empty map; a plugin
/// not present in the map defaults to enabled with no granted permissions, so
/// dropping a plugin into the dir Just Works while installs can persist choices
fn load_plugin_states() -> std::collections::HashMap<String, PluginState> {
    let mut map = std::collections::HashMap::new();
    let Some(path) = plugins_cfg_path() else {
        return map;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let Some((id, rest)) = line.split_once('=') else {
            continue;
        };
        let id = id.trim();
        if !plugin::id_is_safe(id) {
            continue;
        }
        let mut st = PluginState::default();
        for (i, part) in rest.split(';').enumerate() {
            let part = part.trim();
            if i == 0 {
                st.enabled = part == "on";
            } else if let Some(perms) = part.strip_prefix("perms=") {
                st.granted = perms
                    .split(',')
                    .map(str::trim)
                    .filter(|p| plugin::KNOWN_PERMISSIONS.contains(p))
                    .map(str::to_string)
                    .collect();
            }
        }
        map.insert(id.to_string(), st);
    }
    map
}

/// write the plugins.cfg map back to disk
fn save_plugin_states(map: &std::collections::HashMap<String, PluginState>) {
    use std::fmt::Write as _;
    let Some(path) = plugins_cfg_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut s = String::new();
    // stable order so the file doesn't churn between writes
    let mut ids: Vec<&String> = map.keys().collect();
    ids.sort();
    for id in ids {
        let st = &map[id];
        let _ = write!(s, "{}={}", id, if st.enabled { "on" } else { "off" });
        if !st.granted.is_empty() {
            let _ = write!(s, ";perms={}", st.granted.join(","));
        }
        s.push('\n');
    }
    let _ = std::fs::write(&path, s);
}

/// an installed plugin discovered on disk, with its validated manifest, the
/// resolved entry program path, and its persisted enable/permission state
struct Discovered {
    manifest: plugin::Manifest,
    program: String,
    enabled: bool,
    granted: Vec<String>,
}

/// parse a quake hotkey combo like "ctrl+grave" or "ctrl+shift+t" into
/// (win32 modifiers, virtual-key). returns None if empty or missing a real
/// modifier so the global hotkey simply stays off
#[cfg(windows)]
fn parse_quake_key(s: &str) -> Option<(u32, u32)> {
    // win32 MOD_* values, plus MOD_NOREPEAT so a held key fires once
    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;
    const MOD_NOREPEAT: u32 = 0x4000;
    let mut mods = MOD_NOREPEAT;
    let mut vk: Option<u32> = None;
    for part in s.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            other => vk = vk.or_else(|| vk_from_name(other)),
        }
    }
    let vk = vk?;
    // a bare key as a global hotkey would swallow it everywhere; require a mod
    if mods == MOD_NOREPEAT {
        return None;
    }
    Some((mods, vk))
}

/// map a key name to a win32 virtual-key code
#[cfg(windows)]
fn vk_from_name(name: &str) -> Option<u32> {
    let b = name.as_bytes();
    if b.len() == 1 {
        let c = b[0];
        // VK 'A'..='Z' and '0'..='9' share their ascii codepoints
        if c.is_ascii_lowercase() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|d| d.parse::<u32>().ok())
        && (1..=12).contains(&n)
    {
        return Some(0x70 + n - 1); // VK_F1..VK_F12
    }
    Some(match name {
        "grave" | "backtick" | "tilde" | "`" => 0xC0, // VK_OEM_3
        "space" => 0x20,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "enter" | "return" => 0x0D,
        "minus" | "-" => 0xBD,
        "equal" | "equals" | "=" => 0xBB,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        _ => return None,
    })
}

#[cfg(target_os = "linux")]
fn quake_portal_trigger(s: &str) -> Option<String> {
    let mut modifiers = Vec::new();
    let mut key = None;
    for part in s.split('+') {
        let part = part.trim().to_ascii_lowercase();
        match part.as_str() {
            "" => {}
            "ctrl" | "control" => modifiers.push("CTRL"),
            "alt" => modifiers.push("ALT"),
            "shift" => modifiers.push("SHIFT"),
            "win" | "super" | "meta" => modifiers.push("LOGO"),
            other if key.is_none() => key = portal_key_name(other),
            _ => return None,
        }
    }
    if modifiers.is_empty() {
        return None;
    }
    let key = key?;
    modifiers.push(&key);
    Some(modifiers.join("+"))
}

#[cfg(target_os = "linux")]
fn portal_key_name(name: &str) -> Option<String> {
    if name.len() == 1 {
        let byte = name.as_bytes()[0];
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            return Some(name.to_string());
        }
    }
    if let Some(number) = name.strip_prefix('f').and_then(|digits| digits.parse::<u8>().ok())
        && (1..=12).contains(&number)
    {
        return Some(format!("F{number}"));
    }
    Some(match name {
        "grave" | "backtick" | "tilde" | "`" => "grave",
        "space" => "space",
        "tab" => "Tab",
        "esc" | "escape" => "Escape",
        "enter" | "return" => "Return",
        "minus" | "-" => "minus",
        "equal" | "equals" | "=" => "equal",
        "left" => "Left",
        "up" => "Up",
        "right" => "Right",
        "down" => "Down",
        _ => return None,
    }
    .to_string())
}

fn load_persisted() -> Persisted {
    let Some(path) = config_path() else {
        return Persisted::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Persisted::default();
    };
    parse_persisted(&text)
}

/// parse the key=value config text into Persisted, leaving defaults for any key
/// absent or malformed; split out from load_persisted so the settings parser is
/// unit-testable without touching %APPDATA%
fn parse_persisted(text: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "scrollback" => {
                if let Ok(n) = v.parse::<usize>() {
                    // same ceiling as the settings control: history is stored
                    // as full-width lines, so an uncapped value is an OOM knob
                    if n > 100_000 {
                        log::warn!("scrollback={n} clamped to 100000");
                    }
                    p.scrollback = n.min(100_000);
                }
            }
            "copy_on_select" => p.copy_on_select = v == "true",
            "shell" => p.shell = ShellKind::from_label(v),
            "load_profile" => p.load_profile = v == "true",
            "close_action" => p.close_action = CloseAction::from_label(v),
            "right_click" => p.right_click = RightClick::from_label(v),
            "backend" => p.backend = render::BackendChoice::from_label(v),
            "restore_on_launch" => p.restore_on_launch = v != "false",
            "font_size" => {
                if let Ok(n) = v.parse() {
                    p.font_size = n;
                }
            }
            "padding" => {
                if let Ok(n) = v.parse() {
                    p.padding = n;
                }
            }
            "opacity" => {
                if let Ok(n) = v.parse() {
                    p.opacity = n;
                }
            }
            "cursor" => p.cursor = cursor_from_name(v),
            "cursor_blink" => p.cursor_blink = v != "false",
            "bold_as_bright" => p.bold_as_bright = v != "false",
            "line_height" => {
                if let Ok(x) = v.parse::<f32>() {
                    p.line_height = x;
                }
            }
            "theme" => {
                if v.eq_ignore_ascii_case("auto") {
                    p.theme_auto = true;
                } else {
                    p.theme = color::ThemeId::from_name(v);
                    p.theme_auto = false;
                }
            }
            "theme_dark" => p.theme_dark = color::ThemeId::from_name(v),
            "theme_light" => p.theme_light = color::ThemeId::from_name(v),
            "font" => {
                if !v.is_empty() {
                    p.font = Some(v.to_string());
                }
            }
            "font_weight" => {
                if let Some(w) = font_weight_from_label(v) {
                    p.font_weight = w;
                }
            }
            "min_contrast" => {
                if let Ok(x) = v.parse::<f32>() {
                    p.min_contrast = x.clamp(1.0, 21.0);
                }
            }
            "ligatures" => p.ligatures = v != "false" && v != "off",
            "background_image" => p.background_image = (!v.is_empty()).then(|| v.to_string()),
            "background_image_opacity" => {
                if let Ok(x) = v.parse::<f32>() {
                    p.background_image_opacity = x.clamp(0.0, 1.0);
                }
            }
            "quake_key" => {
                p.quake_key_raw = (!v.is_empty()).then(|| v.to_string());
                #[cfg(windows)]
                {
                    p.quake_key = parse_quake_key(v);
                    if p.quake_key.is_none() {
                        p.quake_key_raw = None;
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    p.quake_key = quake_portal_trigger(v);
                    if p.quake_key.is_none() {
                        p.quake_key_raw = None;
                    }
                }
            }
            "wsl_distro" => {
                if !v.is_empty() {
                    p.wsl_distro = Some(v.to_string());
                }
            }
            "plugin_sandbox" => {
                p.plugin_sandbox = v == "appcontainer" || v == "bwrap" || v == "on" || v == "true"
            }
            "acrylic" | "mica" => p.acrylic = v == "true" || v == "on",
            "inline_paint" => p.inline_paint = v == "true" || v == "on",
            "latency_hud" => p.latency_hud = v == "true" || v == "on",
            "update_check" => p.update_check = v != "false" && v != "off",
            "term_program" => {
                // empty falls back to the default at spawn time
                if !v.is_empty() {
                    p.term_program = v.to_string();
                }
            }
            other => {
                if let Some(name) = other.strip_prefix("theme.") {
                    if !name.is_empty() && !v.is_empty() {
                        p.shell_themes.push((name.to_string(), color::ThemeId::from_name(v)));
                    } else {
                        log::warn!("config: per-profile theme line `{other}` needs a name and a theme");
                    }
                } else if let Some(rest) = other.strip_prefix("profile.") {
                    // profile.<name>=<argv>, profile.<name>.cwd=<dir>,
                    // profile.<name>.env.<VAR>=<value>; sub-keys attach to the
                    // profile of that name whatever order the lines appear in
                    if let Some(name) = rest.strip_suffix(".cwd") {
                        if !name.is_empty() && !v.is_empty() {
                            profile_mut(&mut p.profiles, name).cwd = Some(v.to_string());
                            p.profiles_raw.push((rest.to_string(), v.to_string()));
                        } else {
                            log::warn!("config: profile line `{other}` needs a name and a directory");
                        }
                    } else if let Some((name, var)) = rest.split_once(".env.") {
                        if !name.is_empty() && !var.is_empty() {
                            profile_mut(&mut p.profiles, name).env.push((var.to_string(), v.to_string()));
                            p.profiles_raw.push((rest.to_string(), v.to_string()));
                        } else {
                            log::warn!("config: profile env line `{other}` needs a name and a variable");
                        }
                    } else {
                        let argv = split_cmdline(v);
                        if !rest.is_empty() && !argv.is_empty() {
                            profile_mut(&mut p.profiles, rest).argv = argv;
                            p.profiles_raw.push((rest.to_string(), v.to_string()));
                        } else {
                            log::warn!("config: profile line `{other}` needs a name and a command");
                        }
                    }
                } else {
                    // a typo used to be discarded with zero feedback
                    log::warn!("config: unknown key `{other}` ignored");
                }
            }
        }
    }
    // a .cwd / .env line for a profile with no command line leaves an argv-less
    // entry — drop it (it must never reach the spawn path) and say so, like the
    // other malformed-config warns
    p.profiles.retain(|pr| {
        if pr.argv.is_empty() {
            log::warn!("config: profile `{}` has .cwd/.env lines but no profile.{}=<command>; ignored", pr.name, pr.name);
        }
        !pr.argv.is_empty()
    });
    p
}

/// per-window state. step 1 of the multi-window refactor extracts the main
/// window's state here (App.pw); torn-off panes still live in App.satellites
/// until step 2 graduates them to first-class PaneWindows
struct PaneWindow {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    a11y: Option<accesskit_winit::Adapter>,
    tabs: Vec<Tab>,
    active_tab: usize,
    layout_cache: Vec<(usize, Rect)>,
    // per-window ui state: these belong to one window, so the swap-dispatch
    // (self.pw <-> a satellite) keeps them from leaking across windows
    maximized: bool,
    pane_mode: bool,
    settings_open: bool,
    /// user-toggled always-on-top (palette action), per window
    on_top: bool,
    // os focus (drives this window's cursor blink/render) + its in-flight ime
    // composition; both are per-window and must not leak through the swap
    focused: bool,
    ime_composing: bool,
    /// the composition string + winit's caret byte range, drawn inline at the
    /// focused pane's cursor until the IME commits or cancels
    ime_preedit: String,
    ime_preedit_caret: Option<(usize, usize)>,
    // this window's open right-click pane context menu (None = closed)
    pane_menu: Option<PaneMenu>,
    // this window's focused-pane git branch + the cwd it was computed for
    git: Option<String>,
    last_git_cwd: Option<String>,
    // last pointer position within this window (mouse events carry no window id)
    cursor: PhysicalPosition<f64>,
    cursor_icon: CursorIcon,
    // modal overlays carrying this window's own tab index, so a confirm/rename
    // raised here can only be resolved here (never targets another window's tab)
    confirm: Option<ConfirmState>,
    rename: Option<RenameState>,
}

fn pane_window(window: Option<Arc<Window>>, renderer: Option<Renderer>, tabs: Vec<Tab>) -> PaneWindow {
    PaneWindow {
        window,
        renderer,
        a11y: None,
        tabs,
        active_tab: 0,
        layout_cache: Vec::new(),
        maximized: false,
        pane_mode: false,
        settings_open: false,
        on_top: false,
        focused: true,
        ime_composing: false,
        ime_preedit: String::new(),
        ime_preedit_caret: None,
        pane_menu: None,
        git: None,
        last_git_cwd: None,
        cursor: PhysicalPosition::new(0.0, 0.0),
        cursor_icon: CursorIcon::Default,
        confirm: None,
        rename: None,
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    /// this process's parsed command line (always-new-window: one per process)
    cli: CliArgs,
    /// an inbound default-terminal session waiting to become tab one
    #[cfg(windows)]
    handoff: Option<defterm::Handoff>,
    /// the foreground window at process start, captured before we create ours.
    /// when the explorer address bar launches a bare `termie` it passes no working
    /// dir (we inherit explorer's home dir), so this is how we recover the folder
    /// the user typed in: the launching explorer window's current location
    launch_fg: isize,
    /// the main window's state (window/renderer/tabs/active_tab/layout)
    pw: PaneWindow,
    /// torn-off windows, each a full PaneWindow (keyed by window id at routing)
    satellites: Vec<PaneWindow>,
    /// while a satellite is swapped into self.pw for handling, the index it came
    /// from (and where the main window is parked); None means self.pw is the main
    cur_sat: Option<usize>,
    next_id: usize,
    mods: ModifiersState,
    shown: bool,
    pool: Vec<Pane>,
    selection: Option<Sel>,
    /// keyboard mark mode state; Some while active (selection mirrors it)
    mark: Option<MarkState>,
    /// --kitty-demo: inject the gradient into the first pane's first output
    kitty_demo_pending: bool,
    selecting: bool,
    /// drag-selection autoscroll: signed lines/tick while the pointer is held
    /// past the pane edge, plus the next tick deadline (about_to_wait drives it)
    sel_autoscroll: Option<isize>,
    sel_scroll_at: Option<Instant>,
    /// dragging the scroll thumb: (pane id, pointer-y offset from the thumb top)
    sb_drag: Option<(usize, f32)>,
    last_click: Option<(Instant, f64, f64)>,
    // consecutive click count in a pane's content for word/line select cycling
    click_seq: u32,
    /// last (state, pct) sent to the taskbar button, so the COM call only
    /// happens when a pane's OSC 9;4 progress actually changes
    taskbar_sent: (u8, u8),
    palette: Option<PaletteState>,
    /// searchable font-picker overlay (reuses the palette box); Some while open
    font_pick: Option<PaletteState>,
    /// installed monospace families, loaded lazily on first open
    font_families: Vec<String>,
    /// the font in use when the picker opened, restored if the pick is cancelled
    font_pick_orig: Option<String>,
    find: Option<FindState>,
    /// regex mode for the find bar; remembered across open/close (toggled by
    /// the .* button or Alt+R while find is open)
    find_regex: bool,
    /// the plugins marketplace overlay, when open
    market: Option<MarketState>,
    pressed: Option<Hot>,
    /// a tab drag owned by the app so it can cross OS-window boundaries
    tab_drag: Option<TabDrag>,
    /// recently closed tabs' launch specs, newest last; Ctrl+Shift+T pops one
    closed_tabs: Vec<ClosedTab>,
    /// hold find-follow during a temporary active_tab switch (background pane
    /// exit): close_focused_pane_by_id still runs, but match lists stay on the
    /// viewer's grid until the final after_focus_context_change
    find_follow_hold: bool,
    last_title: String,
    config: Config,
    /// user keybindings (combo -> palette action) loaded from disk; checked
    /// before the built-in shortcuts, empty when there is no config file
    keybindings: Vec<(ModifiersState, Key, PaletteAction)>,
    /// payloads of `send <text>` bindings; SendInput actions index here
    send_inputs: Vec<String>,
    /// keybindings.conf lines ignored at load; surfaced once as a status notice
    /// after the first frame (the renderer doesn't exist yet at load time)
    kb_ignored: usize,
    /// last known-good window bounds, refreshed whenever the session is marked
    /// dirty; the fallback when a save fires while the window is minimized
    last_window_bounds: Option<session::WindowBounds>,
    settings_anim: Option<Instant>,
    /// set when the focused pane changes, so its accent border eases in instead
    /// of snapping; cleared once the ease settles
    focus_anim: Option<Instant>,
    /// debug-only: TERMIE_BENCH=N auto-opens N tabs after startup to measure
    /// warm-pool tab-open latency via the TERMIE_TIMING log
    #[cfg(debug_assertions)]
    bench_left: u32,
    /// a running --drive script; steps fire from about_to_wait wakeups
    drive: Option<Drive>,
    #[cfg(debug_assertions)]
    bench_next: Option<Instant>,
    /// pool shells currently spawning on worker threads (not yet in `pool`)
    pending_warm: usize,
    /// set once the app is exiting so no new shells are spawned during teardown
    shutting_down: bool,
    /// pane-mode drag state: a divider being resized (path) or a pane being moved
    drag_divider: Option<Vec<usize>>,
    pane_drag: Option<PaneDrag>,
    #[cfg(target_os = "linux")]
    kwin_drag_bridge: Option<win::KwinDragBridge>,
    #[cfg(target_os = "linux")]
    kwin_drag_probe: Option<KwinDragProbe>,
    #[cfg(target_os = "linux")]
    kwin_drag_geometry: Vec<KwinWindowGeometry>,
    #[cfg(target_os = "linux")]
    kwin_drag_generation: u64,
    /// quake drop-down currently summoned (always-on-top at screen top)
    #[cfg(any(windows, target_os = "linux"))]
    quake_shown: bool,
    /// the global quake hotkey thread has been spawned (once per process)
    #[cfg(any(windows, target_os = "linux"))]
    quake_hotkey_spawned: bool,
    /// the colors.conf/keybindings.conf watcher thread has been spawned
    conf_watch_spawned: bool,
    #[cfg(not(windows))]
    system_dark: Option<bool>,
    #[cfg(not(windows))]
    theme_watch_spawned: bool,
    /// persisted settings loaded at startup; renderer-owned ones applied in boot
    persisted: Persisted,
    /// broadcast input: typed keys go to every pane in the active tab
    broadcast: bool,
    /// button + pane that received a forwarded press; drag motion and release
    /// stay locked to this pane even if the cursor leaves it
    mouse_down: Option<(u8, usize)>,
    /// last mouse report sent (pane, btn, pressed, motion, col, row, mods).
    /// motion is suppressed when the cell hasn't changed so any-event tracking
    /// can't flood a TUI's input buffer with identical CSI reports
    last_mouse_report: Option<(usize, u8, bool, bool, usize, usize, u8)>,
    /// url under the cursor while ctrl is held, to underline + open on click:
    /// (focused-pane viewport row, col_start, col_end exclusive)
    link: Option<(usize, usize, usize)>,
    /// system fonts are scanned lazily after first paint to keep startup fast
    system_fonts_pending: bool,
    /// the printable-ASCII glyph cache has been warmed once (off the boot path)
    ascii_warmed: bool,
    /// consecutive failed pool spawns; backs off + gives up so a broken shell
    /// can't spin a busy respawn loop with a permanently empty window
    warm_fails: usize,
    warm_backoff_until: Option<Instant>,
    /// set when a paint is deferred because a pane is mid synchronized-output
    /// (DEC 2026) frame; the safety deadline forces a paint if the frame stalls
    sync_redraw_pending: Option<Instant>,
    /// armed on every resize event; the grid/pty reflow is held until the drag
    /// settles (about_to_wait fires it once the deadline passes) so a live
    /// resize doesn't rebuild all scrollback per pixel-step
    resize_settle: Option<Instant>,
    /// pty output arrived this loop turn; about_to_wait paints once so a flood
    /// of chunks collapses to a single frame
    pty_dirty: bool,
    /// input-to-photon instrumentation: the time of the first input not yet
    /// reflected on screen (None once painted), the last present time, and a
    /// rolling meter feeding the optional latency hud
    input_at: Option<Instant>,
    last_present: Option<Instant>,
    lat: LatencyMeter,
    /// the tab/split layout changed since the last session write
    session_dirty: bool,
    /// debounce deadline for the session write; re-armed on every layout change
    /// so a burst (e.g. a divider drag) collapses to one write after it settles
    session_flush_at: Option<Instant>,
    /// when the status-bar notification readout (OSC 9/777 text) expires
    notice_until: Option<Instant>,
    /// fractional wheel-scroll remainder, so slow precision-touchpad deltas
    /// accumulate into whole lines instead of rounding away
    wheel_accum: f32,
    /// a newer release found by the update check, until installed or ignored
    update: Option<update::Update>,
    /// the daily check ran this session (it runs once, deferred after boot)
    update_checked: bool,
    /// set when this window was launched into a specific folder or command (the
    /// address bar, a `--cwd` context-menu verb, or `-- command`); such an
    /// ad-hoc window must never overwrite the saved session, so writes are
    /// suppressed while it's set
    session_ephemeral: bool,
    /// running plugin processes (out-of-process, supervised); spawned deferred
    /// after the window is shown so disabled/no plugins cost nothing at boot
    plugins: Vec<plugin::Plugin>,
    plugins_started: bool,
    /// manifest id per plugin, parallel to `plugins`, used as the `from` on
    /// bus messages so subscribers know who published
    plugin_ids: Vec<String>,
    /// granted permissions per plugin, parallel to `plugins`; gates the
    /// permissioned verbs (write_pty) and events (output_chunk)
    plugin_granted: Vec<Vec<String>>,
    /// declared Tier-1 widgets keyed by (plugin index, widget id) so two
    /// plugins can't clobber each other; rendered in the right-side dock
    plugin_widgets: Vec<(usize, String, render::DockWidget)>,
    /// in-process bus subscriptions: (subscriber plugin index, topic); topic
    /// "*" matches every published topic
    plugin_subs: Vec<(usize, String)>,
}

impl App {
    fn elevated(&self) -> bool {
        win::is_elevated() || self.cli.admin_shell
    }

    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let p = load_persisted();
        // install profiles before anything derives from them (palette entries,
        // shell labels in a restored session). each installed wsl distro joins
        // the config profiles as a synthetic "wsl: <name>" so it appears in the
        // palette, jump list, and '+' dropdown; the global wsl_distro still sets
        // the default distro for the plain wsl shell
        pty::set_profiles(with_wsl_profiles(p.profiles.clone(), win::wsl_distros()));
        let cli = parse_args(std::env::args().skip(1));
        let (keybindings, send_inputs, kb_ignored) = load_keybindings();
        #[cfg(target_os = "linux")]
        let kwin_drag_bridge = win::KwinDragBridge::new(proxy.clone());
        App {
            proxy,
            kitty_demo_pending: cli.kitty_demo,
            cli,
            #[cfg(windows)]
            handoff: None,
            // captured now, before any window of ours exists, so it still names the
            // explorer window we were launched from
            launch_fg: win::foreground_window(),
            pw: pane_window(None, None, Vec::new()),
            satellites: Vec::new(),
            cur_sat: None,
            next_id: 0,
            mods: ModifiersState::empty(),
            keybindings,
            send_inputs,
            kb_ignored,
            last_window_bounds: None,
            shown: false,
            pool: Vec::new(),
            selection: None,
            mark: None,
            selecting: false,
            sel_autoscroll: None,
            sel_scroll_at: None,
            sb_drag: None,
            last_click: None,
            click_seq: 0,
            // force one initial clear so a pinned Linux launcher can't retain
            // stale progress from an earlier process
            taskbar_sent: (u8::MAX, u8::MAX),
            palette: None,
            font_pick: None,
            font_families: Vec::new(),
            font_pick_orig: None,
            find: None,
            find_regex: false,
            market: None,
            pressed: None,
            tab_drag: None,
            closed_tabs: Vec::new(),
            find_follow_hold: false,
            last_title: String::new(),
            config: Config {
                scrollback: p.scrollback,
                copy_on_select: p.copy_on_select,
                shell: p.shell,
                load_profile: p.load_profile,
                close_action: p.close_action,
                right_click: p.right_click,
                backend: p.backend,
                restore_on_launch: p.restore_on_launch,
                #[cfg(windows)]
                quake_key: p.quake_key,
            },
            persisted: p,
            broadcast: false,
            mouse_down: None,
            last_mouse_report: None,
            link: None,
            system_fonts_pending: true,
            ascii_warmed: false,
            warm_fails: 0,
            warm_backoff_until: None,
            sync_redraw_pending: None,
            resize_settle: None,
            pty_dirty: false,
            input_at: None,
            last_present: None,
            lat: LatencyMeter::default(),
            session_dirty: false,
            session_flush_at: None,
            notice_until: None,
            wheel_accum: 0.0,
            update: None,
            update_checked: false,
            session_ephemeral: false,
            plugins: Vec::new(),
            plugins_started: false,
            plugin_ids: Vec::new(),
            plugin_granted: Vec::new(),
            plugin_widgets: Vec::new(),
            plugin_subs: Vec::new(),
            settings_anim: None,
            focus_anim: None,
            #[cfg(debug_assertions)]
            bench_left: std::env::var("TERMIE_BENCH").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            #[cfg(debug_assertions)]
            bench_next: None,
            pending_warm: 0,
            shutting_down: false,
            drag_divider: None,
            pane_drag: None,
            #[cfg(target_os = "linux")]
            kwin_drag_bridge,
            #[cfg(target_os = "linux")]
            kwin_drag_probe: None,
            #[cfg(target_os = "linux")]
            kwin_drag_geometry: Vec::new(),
            #[cfg(target_os = "linux")]
            kwin_drag_generation: 0,
            #[cfg(any(windows, target_os = "linux"))]
            quake_shown: false,
            #[cfg(any(windows, target_os = "linux"))]
            quake_hotkey_spawned: false,
            conf_watch_spawned: false,
            #[cfg(not(windows))]
            system_dark: None,
            #[cfg(not(windows))]
            theme_watch_spawned: false,
            drive: None,
        }
    }

    fn configure_renderer(&self, renderer: &mut Renderer) {
        let settings = &self.persisted;
        renderer.set_theme(self.resolved_theme());
        renderer.set_elevated(self.elevated());
        renderer.set_color_overrides(load_color_overrides());
        renderer.set_cursor_style(settings.cursor);
        renderer.set_cursor_blink(settings.cursor_blink);
        renderer.set_bold_as_bright(settings.bold_as_bright);
        renderer.set_line_height(settings.line_height);
        renderer.set_pane_pad_px(settings.padding);
        renderer.set_opacity_pct(settings.opacity);
        if let Some(font) = settings.font.as_deref() {
            renderer.set_font_by_name(font);
        }
        renderer.set_font_weight(settings.font_weight);
        renderer.set_min_contrast(settings.min_contrast);
        renderer.set_ligatures(settings.ligatures);
        if let Some(path) = settings.background_image.as_deref() {
            match std::fs::read(path).ok().and_then(|bytes| image::decode_png(&bytes)) {
                Some(image) => {
                    let image = image::downscale_rgba(&image.rgba, image.width, image.height, 1024);
                    renderer.set_background_image(Some(image), settings.background_image_opacity);
                }
                None => log::warn!("background_image: couldn't load {path} (png only)"),
            }
        }
        renderer.set_content_pt(settings.font_size);
    }

    fn boot(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        // start hidden; reveal after the first painted frame to avoid a white flash
        let (irgba, iw, ih) = win::app_icon();
        let icon = winit::window::Icon::from_rgba(irgba, iw, ih).ok();
        // load the saved session up front: its bounds size the window at creation
        // (so the renderer builds at the right size, no post-show resize) and its
        // tab tree is restored below
        let restored = if self.config.restore_on_launch {
            session_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| session::SessionFile::parse(&t))
        } else {
            None
        };
        // launch mode decides tab one and whether the saved window bounds apply:
        // only a plain bare launch from the home dir restores the session; an
        // explicit cwd/command/shell, a folder launch, or a defterm handoff is
        // ad-hoc and opens fresh, so those windows don't all stack on the saved rect
        let first_cwd = if self.cli.is_bare() { launch_cwd(self.launch_fg) } else { self.cli.cwd.clone() };
        let command = self.cli.command.clone();
        let first_shell = self.cli.shell.as_deref().map(ShellKind::from_label);
        #[cfg(windows)]
        let has_handoff = self.handoff.is_some();
        #[cfg(not(windows))]
        let has_handoff = false;
        let restore_bounds = (!has_handoff
            && command.is_none()
            && first_cwd.is_none()
            && first_shell.is_none())
        .then(|| restored.as_ref().and_then(|s| s.window.as_ref()))
        .flatten();
        // clamp the saved rect to a currently-visible monitor (so a window saved
        // on a now-disconnected display doesn't open off-screen); the size goes on
        // the attributes so the renderer builds at the right size, while position
        // and maximize are applied after creation (winit-on-windows friendly)
        let monitors = monitor_rects(event_loop);
        let placement = restore_bounds.map(|b| clamp_window_bounds(&monitors, (b.x, b.y, b.width, b.height)));
        let restore_max = restore_bounds.is_some_and(|b| b.maximized);
        let mut attrs = Window::default_attributes()
            .with_title("termie")
            .with_window_icon(icon)
            .with_decorations(false)
            .with_visible(false)
            // below this the title-bar controls + tabs and the status-bar clusters
            // would overlap (no room for all the chrome); clamp so the window is
            // always usable
            .with_min_inner_size(LogicalSize::new(560.0, 380.0));
        if let Some((max_width, max_height)) = monitors
            .iter()
            .map(|m| (m.2, m.3))
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
        {
            attrs = attrs.with_max_inner_size(PhysicalSize::new(max_width, max_height));
        }
        let attrs = match placement {
            Some((_, _, w, h)) => attrs.with_inner_size(PhysicalSize::new(w, h)),
            None => attrs.with_inner_size(LogicalSize::new(1000.0, 640.0)),
        };
        // app id so wayland and x11 match every window to termie.desktop
        let attrs = platform_window_attrs(attrs);
        let window = Arc::new(event_loop.create_window(attrs)?);
        if let Some((x, y, _, _)) = placement {
            window.set_outer_position(PhysicalPosition::new(x, y));
        }
        if restore_max {
            window.set_maximized(true);
        }
        constrain_window_to_monitor(&window);
        timing("window created");

        win::clipboard_init(&window);
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw() {
                win::apply_window_effects(h.hwnd.get());
                if self.persisted.acrylic {
                    win::apply_backdrop(h.hwnd.get(), true);
                }
                // a --drive run must never touch the user's session in either
                // direction: the window can't activate (their typing can't
                // land here, chords don't need focus) and paints as focused
                // so its output is what a focused window would show
                if self.cli.drive.is_some() {
                    win::set_no_activate(h.hwnd.get());
                }
            }
        #[cfg(not(windows))]
        if self.persisted.acrylic {
            window.set_blur(true);
        }
        if let Some(path) = self.cli.drive.clone() {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let steps = parse_drive_script(&text);
                    if steps.is_empty() {
                        log::warn!("--drive: no runnable steps in {path}");
                    } else {
                        self.drive = Some(Drive { steps, next: 0, started: None });
                        self.pw.focused = true;
                    }
                }
                Err(e) => log::warn!("--drive: couldn't read {path}: {e}"),
            }
        }

        // overlap the slow first pwsh spawn with the ~300ms gpu init below: kick it
        // off now so it is already producing its prompt by the time the window
        // appears. it arrives via PaneReady and becomes tab one. spawned at the
        // canonical size since the renderer (final content size) isn't built yet —
        // the first-output relayout resizes it before any content is painted (the
        // pty reader only starts at PaneReady, so no 80x24 content is ever shown).
        // only for a bare launch; cli/restore launches install tab one synchronously
        if self.cli.is_bare() {
            self.spawn_pool_shell(80, 24);
        }

        let mut renderer = Renderer::new(
            window.clone(),
            event_loop.owned_display_handle(),
            CONTENT_PT,
            CHROME_PT,
            self.config.backend,
            false,
        )?;
        self.configure_renderer(&mut renderer);
        timing("renderer ready (gpu init)");
        window.set_ime_allowed(true);
        self.pw.a11y = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));
        self.pw.window = Some(window.clone());
        self.pw.renderer = Some(renderer);

        self.pw.active_tab = 0;
        // register the global quake hotkey once (opt-in via the quake_key setting)
        #[cfg(windows)]
        if !self.quake_hotkey_spawned
            && let Some((mods, vk)) = self.config.quake_key
        {
            self.quake_hotkey_spawned = true;
            let proxy = self.proxy.clone();
            let ok = win::spawn_global_hotkey(1, mods, vk, move || {
                let _ = proxy.send_event(UserEvent::ToggleQuake);
            });
            if !ok {
                log::warn!("quake hotkey unavailable (already in use by another app)");
            }
        }
        #[cfg(target_os = "linux")]
        if !self.quake_hotkey_spawned
            && let Some(trigger) = self.persisted.quake_key.clone()
        {
            self.quake_hotkey_spawned = true;
            let proxy = self.proxy.clone();
            if !win::spawn_global_hotkey(trigger, move || {
                let _ = proxy.send_event(UserEvent::ToggleQuake);
            }) {
                log::warn!("quake hotkey worker could not start");
            }
        }
        // watch colors.conf/keybindings.conf so hand edits apply live
        if !self.conf_watch_spawned {
            self.conf_watch_spawned = true;
            // drop a fully-commented keybindings.conf on first run so the format
            // and every action are discoverable; if the watcher then reloads it,
            // every line is a comment so it just re-applies the defaults
            write_keybindings_template_if_absent();
            spawn_conf_watcher(self.proxy.clone());
        }
        #[cfg(not(windows))]
        if self.persisted.theme_auto {
            self.ensure_theme_watcher();
        }
        // an inbound default-terminal session becomes tab one — ephemeral like
        // an explicit cli launch, so it never overwrites the saved session
        #[cfg(windows)]
        let adopted = if let Some(h) = self.handoff.take() {
            self.session_ephemeral = true;
            let (cols, rows) = self.content_pane_size();
            let title = h.title.clone();
            let pane = self.spawn_handoff_pane(h, cols, rows);
            self.install_first_tab(pane);
            if !title.is_empty()
                && let Some(t) = self.pw.tabs.first_mut()
            {
                t.title = Some(title);
                self.sync_tabs();
            }
            true
        } else {
            false
        };
        #[cfg(not(windows))]
        let adopted = false;
        if adopted {
            // tab one is the handed-off console session
        } else if !self.cli.tabs.is_empty() {
            // wt-style layout verbs: ephemeral like any explicit cli launch, so
            // the scripted window never overwrites the saved session
            self.session_ephemeral = true;
            let tabs = self.cli.tabs.clone();
            self.restore_session(session::SessionFile { active_tab: 0, tabs, window: None });
        } else if command.is_some() || first_cwd.is_some() || first_shell.is_some() {
            self.session_ephemeral = true;
            let (cols, rows) = self.content_pane_size();
            match self.spawn_pane(cols, rows, first_cwd, first_shell, command.as_deref()) {
                Ok(pane) => self.install_first_tab(pane),
                Err(e) => log::error!("failed to spawn the requested command: {e}"),
            }
        } else if let Some(sf) = restored {
            // bare launch: rebuild the saved tab/split layout with fresh shells in
            // the saved dirs. if nothing restores, the warm pool below installs a
            // single shell as the fallback
            self.restore_session(sf);
        }
        // start the first shells now — the pane size is final once settings are
        // applied, so the async pwsh spawn overlaps the first paint + reveal
        // below instead of starting after them. the first pool shell becomes
        // tab one. pwsh startup never blocks the window appearing
        self.warm_pool();
        // start the power-on reveal clock now so the whole animation plays once
        // the window is visible, not during the (invisible) gpu-init wait
        if let Some(r) = self.pw.renderer.as_mut() {
            r.begin_reveal();
        }
        self.paint();
        window.set_visible(true);
        timing("window shown");
        self.shown = true;
        window.request_redraw();
        // surface any keybindings.conf parse errors now that the status bar exists
        if self.kb_ignored > 0 {
            let noun = if self.kb_ignored == 1 { "line" } else { "lines" };
            let msg = format!("keybindings.conf: {} {noun} ignored", self.kb_ignored);
            self.show_notice(&msg);
        }
        // refresh the launcher's shell actions and keep the default-terminal
        // registration pointed at this exe, both off the startup render path
        let entries = jumplist_entries();
        std::thread::spawn(move || {
            win::refresh_defterm_server_path();
            win::update_jumplist(&entries);
        });
        // while this instance runs, serve default-terminal handoffs directly:
        // a console app launched outside a terminal opens as a tab here
        #[cfg(windows)]
        if win::defterm_registered() {
            let proxy = self.proxy.clone();
            defterm::serve_running(move |h| match h {
                Some(h) => proxy.send_event(UserEvent::Handoff(h)).is_ok(),
                None => true,
            });
        }
        Ok(())
    }

    /// wire a pane's pty output thread to the event loop (call once it's owned
    /// here on the main thread, so no early output is dropped)
    fn start_reader(&self, pane: &mut Pane) {
        let proxy = self.proxy.clone();
        let id = pane.id;
        pane.pty.start_reader(move |msg| match msg {
            PtyMsg::Output(b) => {
                let _ = proxy.send_event(UserEvent::Pty { id, bytes: b });
            }
            PtyMsg::Exited => {
                let _ = proxy.send_event(UserEvent::Exited { id });
            }
        });
    }

    /// build a pane synchronously and start its reader (boot + split fallback)
    fn spawn_pane(&mut self, cols: usize, rows: usize, cwd: Option<String>, shell: Option<ShellKind>, command: Option<&[String]>) -> Result<Pane> {
        let id = self.next_id;
        self.next_id += 1;
        let shell = shell.unwrap_or(self.config.shell);
        let (cw, ch) = self
            .pw
            .renderer
            .as_ref()
            .map(|r| r.cell_px())
            .unwrap_or((0, 0));
        let mut pane = build_pane(
            id,
            cols,
            rows,
            shell,
            self.config.load_profile,
            self.config.scrollback,
            cwd.as_deref(),
            command,
            self.persisted.wsl_distro.as_deref(),
            &self.persisted.term_program,
            cw,
            ch,
        )?;
        self.start_reader(&mut pane);
        Ok(pane)
    }

    /// adopt a default-terminal handoff session as a pane: the ConPTY already
    /// runs inside the console host, so no shell is spawned — the pipes are
    /// wrapped and the session resized to the real pane geometry
    #[cfg(windows)]
    fn spawn_handoff_pane(&mut self, h: defterm::Handoff, cols: usize, rows: usize) -> Pane {
        let id = self.next_id;
        self.next_id += 1;
        let pty = Pty::from_handoff(h.reader, h.writer, h.signal, h.reference, h.server, h.client);
        let (cw, ch) = self.pw.renderer.as_ref().map(|r| r.cell_px()).unwrap_or((0, 0));
        let mut term = Terminal::new(rows, cols);
        term.grid.set_scrollback_limit(self.config.scrollback);
        term.set_cell_px(cw, ch);
        let mut pane = Pane {
            id,
            term,
            parser: Parser::new(),
            pty,
            shell: self.config.shell,
            // the client is already running; resizing is safe from the start
            ready: true,
            flash: None,
            apc: apc::ApcScanner::default(),
            status: PaneStatus::Idle,
        };
        pane.resize(rows, cols);
        self.start_reader(&mut pane);
        pane
    }

    /// install a freshly-built pane (its reader already started) as the sole
    /// tab — the boot adoption path, shared by the first async pool shell and a
    /// synchronous cli-launched command so the two can't diverge
    fn install_first_tab(&mut self, pane: Pane) {
        let fid = pane.id;
        self.pw.tabs.push(Tab {
            focused: fid,
            root: Some(Node::Leaf(pane)),
            zoom: None,
            title: None,
            attention: false,
            color: None,
        });
        self.pw.active_tab = 0;
        self.relayout_all();
        self.sync_tabs();
        self.redraw();
    }

    fn redraw(&self) {
        if let Some(w) = &self.pw.window {
            w.request_redraw();
        }
    }

    /// request a redraw on every torn-off window whose renderer is mid-animation
    /// (reveal / hover / tab-slide / overlay bloom). about_to_wait only drives the
    /// main window, so without this a satellite animation would stall until the
    /// next incidental event. returns true if any satellite is still animating, so
    /// the caller keeps the ~60fps tick alive until they settle
    fn pump_satellite_redraws(&self) -> bool {
        let mut any = false;
        for s in &self.satellites {
            if let (Some(w), Some(r)) = (s.window.as_ref(), s.renderer.as_ref())
                && (r.startup_fading() || r.hover_animating() || r.tab_animating() || r.overlay_animating())
            {
                w.request_redraw();
                any = true;
            }
        }
        any
    }

    /// is there a visible, blinking cursor on the focused pane that needs the
    /// periodic blink tick? (false when blink is off, cursor hidden, or scrolled)
    fn blinking_cursor_on_screen(&self) -> bool {
        let Some(r) = self.pw.renderer.as_ref() else {
            return false;
        };
        let cfg = r.cursor_blink();
        self.pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|root| find_pane(root, t.focused)))
            .map(|p| {
                let cur = &p.term.grid.cursor;
                // an app's DECSCUSR blink bit overrides the configured default
                cur.shape_blink.unwrap_or(cfg) && cur.visible && p.term.grid.view_offset == 0
            })
            .unwrap_or(false)
    }

    /// any visible pane mid bell-flash (keeps the tick alive so it fades out)
    fn any_flash(&self) -> bool {
        let Some(root) = self.pw.tabs.get(self.pw.active_tab).and_then(|t| t.root.as_ref()) else {
            return false;
        };
        let mut any = false;
        each_pane(root, &mut |p| {
            // keep ticking through the fade-out tail (see PaneView.flash easing)
            if p.flash.map(|t| t.elapsed().as_millis() < 220).unwrap_or(false) {
                any = true;
            }
        });
        any
    }

    /// cols/rows for a single pane filling the whole content area
    fn content_pane_size(&self) -> (usize, usize) {
        if let Some(r) = self.pw.renderer.as_ref() {
            let (_, _, cols, rows) = r.pane_metrics(r.content_rect());
            (cols, rows)
        } else {
            (80, 24)
        }
    }

    /// keep one fully-started shell ready so opening a tab feels instant
    fn warm_pool(&mut self) {
        if self.shutting_down || self.pw.renderer.is_none() {
            return;
        }
        // gave up after repeated spawn failures: stop trying (no CPU burn)
        if self.warm_fails >= MAX_WARM_FAILS {
            return;
        }
        // hold off respawning while a backoff from a recent failure is active
        if let Some(t) = self.warm_backoff_until
            && Instant::now() < t {
                return;
            }
        // build shells on worker threads so the slow pwsh spawn never blocks the
        // event loop; dispatch all that are needed at once so they spawn in
        // parallel and the pool fills fast. each arrives via UserEvent::PaneReady
        let (cols, rows) = self.content_pane_size();
        while self.pool.len() + self.pending_warm < POOL_TARGET {
            self.spawn_pool_shell(cols, rows);
        }
    }

    /// dispatch one pool-shell spawn on a worker thread (the async warm-pool
    /// unit). it arrives via UserEvent::PaneReady, where it becomes tab one (bare
    /// launch) or joins the pool
    fn spawn_pool_shell(&mut self, cols: usize, rows: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let (shell, profile, sb) = (self.config.shell, self.config.load_profile, self.config.scrollback);
        let proxy = self.proxy.clone();
        let wsl = self.persisted.wsl_distro.clone();
        let term_program = self.persisted.term_program.clone();
        let (cw, ch) = self
            .pw
            .renderer
            .as_ref()
            .map(|r| r.cell_px())
            .unwrap_or((0, 0));
        self.pending_warm += 1;
        std::thread::spawn(move || {
            let pane = build_pane(
                id,
                cols,
                rows,
                shell,
                profile,
                sb,
                None,
                None,
                wsl.as_deref(),
                &term_program,
                cw,
                ch,
            )
            .ok()
            .map(Box::new);
            let _ = proxy.send_event(UserEvent::PaneReady(pane));
        });
    }

    /// kill every pre-warmed shell (called on shutdown); also latches the
    /// shutting-down flag so no further shells are spawned during teardown
    fn kill_pool(&mut self) {
        self.shutting_down = true;
        for p in &mut self.pool {
            p.pty.kill();
        }
        self.pool.clear();
        // torn-off windows: kill their shells too
        for sat in &mut self.satellites {
            for tab in &mut sat.tabs {
                if let Some(root) = tab.root.as_mut() {
                    kill_all(root);
                }
            }
        }
        // shutdown chokepoint (hit on every exit path) — tear down plugins too
        self.kill_plugins();
    }

    fn release_window_resources(&mut self) {
        for pw in &mut self.satellites {
            drop(pw.a11y.take());
            if let Some(renderer) = pw.renderer.take() {
                renderer.shutdown();
            }
            drop(pw.window.take());
        }
        drop(self.pw.a11y.take());
        if let Some(renderer) = self.pw.renderer.take() {
            renderer.shutdown();
        }
        drop(self.pw.window.take());
    }

    /// discover + spawn enabled plugins once, after the window is shown. each
    /// plugin is a separate process wired to the event loop via the proxy; a
    /// plugin's stdout line arrives as UserEvent::Plugin. failures are logged and
    /// skipped so a broken plugin can never block startup or the core
    fn start_plugins(&mut self) {
        // only launch enabled plugins; disabled ones still appear in the
        // marketplace UI but never spawn a process
        let sandbox = self.persisted.plugin_sandbox;
        for d in discover_plugins().into_iter().filter(|d| d.enabled) {
            let id = d.manifest.id.clone();
            // the index this plugin will occupy once pushed
            let idx = self.plugins.len();
            let proxy = self.proxy.clone();
            let on_msg = move |msg| {
                let _ = proxy.send_event(UserEvent::Plugin { id: idx, msg });
            };
            // sandboxed launch is opt-in; on failure we fail closed (skip the
            // plugin) rather than run it unconfined
            match spawn_plugin(sandbox, &id, &d, on_msg) {
                Ok(mut p) => {
                    // handshake: tell the plugin our api version + the perms the
                    // user actually granted (intersected with what it declared)
                    let granted: Vec<String> = d
                        .granted
                        .iter()
                        .filter(|g| d.manifest.permissions.contains(g))
                        .cloned()
                        .collect();
                    p.send(&plugin::HostEvent::Hello {
                        api_version: plugin::API_VERSION,
                        permissions: granted.clone(),
                    });
                    // keep ids/granted parallel to `plugins` (push together so
                    // indices never skew) so a publisher can be named and
                    // permissioned verbs can be checked
                    self.plugins.push(p);
                    self.plugin_ids.push(id);
                    self.plugin_granted.push(granted);
                }
                Err(e) => log::warn!("plugin {id} failed to spawn: {e}"),
            }
        }
    }

    /// broadcast a host event to every running plugin
    fn plugins_broadcast(&mut self, ev: &plugin::HostEvent) {
        for p in &mut self.plugins {
            p.send(ev);
        }
    }

    /// broadcast an event only to plugins granted `perm` (permissioned events
    /// like pane_state stay invisible to plugins that never asked)
    fn plugins_broadcast_gated(&mut self, perm: &str, ev: &plugin::HostEvent) {
        for (p, granted) in self.plugins.iter_mut().zip(&self.plugin_granted) {
            if granted.iter().any(|g| g == perm) {
                p.send(ev);
            }
        }
    }

    fn kill_plugins(&mut self) {
        for p in &mut self.plugins {
            // ask politely first so a plugin can flush/save, then force it
            p.send(&plugin::HostEvent::Shutdown);
            p.kill();
        }
        self.plugins.clear();
        self.plugin_ids.clear();
        self.plugin_granted.clear();
        self.plugin_subs.clear();
    }

    /// handle one command from plugin `pidx`. widgets, notify, and the bus are
    /// unprivileged; write_pty requires the granted permission (see plugin_granted)
    fn handle_plugin_cmd(&mut self, pidx: usize, cmd: plugin::PluginCmd) {
        use plugin::PluginCmd as C;
        match cmd {
            C::Ready { name, api_version } => {
                log::info!("plugin ready: {name} (api {api_version})");
            }
            C::Notify { text } => {
                log::info!("plugin notify: {text}");
            }
            // plugin widgets: upsert by (plugin, id) and rebuild the dock. a
            // Tier-2 widget also carries an immediate-mode draw list (mapped into
            // the renderer's dock mirror here)
            C::DeclareWidget(w) | C::UpdateWidget(w) => {
                let draw = w
                    .draw
                    .into_iter()
                    .map(|d| match d {
                        plugin::DrawCmd::Rect { x, y, w: rw, h, color } => {
                            render::DockDraw::Rect { x, y, w: rw, h, color }
                        }
                        plugin::DrawCmd::Text { x, y, text, color } => {
                            render::DockDraw::Text { x, y, text, color }
                        }
                    })
                    .collect();
                let dw = render::DockWidget { title: w.title, lines: w.lines, draw, canvas_h: w.canvas_h };
                match self
                    .plugin_widgets
                    .iter_mut()
                    .find(|(p, id, _)| *p == pidx && *id == w.id)
                {
                    Some(slot) => slot.2 = dw,
                    None => self.plugin_widgets.push((pidx, w.id, dw)),
                }
                self.rebuild_dock();
            }
            // in-process bus: record a subscription (topic "*" = all)
            C::Subscribe { topic } => {
                if !self.plugin_subs.iter().any(|(p, t)| *p == pidx && *t == topic) {
                    self.plugin_subs.push((pidx, topic));
                }
            }
            // in-process bus: fan a published message out to every subscriber of
            // this topic (or "*"), tagged with the publisher's manifest id. the
            // publisher doesn't receive its own message
            C::Publish { topic, body } => {
                let from = self.plugin_ids.get(pidx).cloned().unwrap_or_default();
                let targets: Vec<usize> = self
                    .plugin_subs
                    .iter()
                    .filter(|(p, t)| *p != pidx && (t == &topic || t == "*"))
                    .map(|(p, _)| *p)
                    .collect();
                if !targets.is_empty() {
                    let ev = plugin::HostEvent::Message {
                        from,
                        topic,
                        body,
                    };
                    for p in targets {
                        if let Some(plugin) = self.plugins.get_mut(p) {
                            plugin.send(&ev);
                        }
                    }
                }
            }
            // permissioned: only honor write_pty if the user granted it to this
            // plugin; the bytes are sent to the focused pane like a paste
            C::WritePty { data } => {
                let allowed = self
                    .plugin_granted
                    .get(pidx)
                    .map(|g| g.iter().any(|p| p == "write_pty"))
                    .unwrap_or(false);
                if !allowed {
                    log::warn!("plugin {pidx} write_pty denied (permission not granted)");
                    return;
                }
                if let Some(id) = self.active_focused_id()
                    && let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut())
                        && let Some(p) = find_pane_mut(root, id) {
                            p.pty.write(data.as_bytes());
                        }
            }
            C::Unknown(t) => log::warn!("plugin sent unknown command: {t}"),
        }
    }

    /// push the current widget set into the renderer's dock; if the dock's
    /// presence toggled it changed content_rect, so panes must relayout
    fn rebuild_dock(&mut self) {
        let widgets: Vec<render::DockWidget> =
            self.plugin_widgets.iter().map(|(_, _, w)| w.clone()).collect();
        let toggled = self
            .pw.renderer
            .as_mut()
            .map(|r| r.set_dock(widgets))
            .unwrap_or(false);
        if toggled {
            self.relayout_all();
        }
        self.redraw();
    }

    /// push the current scrollback limit onto every live pane + the warm pool
    fn apply_scrollback(&mut self) {
        let n = self.config.scrollback;
        for tab in &mut self.pw.tabs {
            if let Some(root) = tab.root.as_mut() {
                each_pane_mut(root, &mut |p| p.term.grid.set_scrollback_limit(n));
            }
        }
        for sp in &mut self.pool {
            sp.term.grid.set_scrollback_limit(n);
        }
    }

    fn focused_pane_rect(&self) -> Option<Rect> {
        let id = self.active_focused_id()?;
        self.pw.layout_cache
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, r)| *r)
    }

    /// (row, col) under a pixel position within the focused pane
    fn cell_in_focused(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let rect = self.focused_pane_rect()?;
        let r = self.pw.renderer.as_ref()?;
        let (col, row) = r.cell_at(rect, x, y);
        Some((row, col))
    }

    /// signed autoscroll speed (lines per tick) while a drag-selection holds
    /// the pointer past the focused pane's top/bottom edge; further = faster,
    /// positive scrolls into history (pointer above the pane)
    fn sel_edge_speed(&self, py: f32) -> Option<isize> {
        let (_, ry, _, rh) = self.focused_pane_rect()?;
        let over = if py < ry {
            ry - py
        } else if py > ry + rh {
            -(py - (ry + rh))
        } else {
            return None;
        };
        let mag = 1 + (over.abs() / 24.0) as isize;
        Some(mag.min(8) * over.signum() as isize)
    }

    /// the web url under a pixel position in the focused pane, as
    /// (viewport row, col_start, col_end, url); only when the point is actually
    /// inside the pane, not clamped to its edge
    fn focused_url_at(&self, x: f32, y: f32) -> Option<(usize, usize, usize, String)> {
        let (rx, ry, rw, rh) = self.focused_pane_rect()?;
        if x < rx || y < ry || x >= rx + rw || y >= ry + rh {
            return None;
        }
        let (col, row) = self.pw.renderer.as_ref()?.cell_at((rx, ry, rw, rh), x, y);
        let id = self.active_focused_id()?;
        let root = self.pw.tabs.get(self.pw.active_tab)?.root.as_ref()?;
        let p = find_pane(root, id)?;
        let g = &p.term.grid;
        // an explicit OSC 8 hyperlink on the cell wins over url autodetection
        let cell_link = g.line_at(row).get(col).map(|c| c.link).unwrap_or(0);
        if let Some(uri) = g.link_uri(cell_link) {
            let (start, end) = g.link_span(row, col, cell_link);
            return Some((row, start, end, uri.to_string()));
        }
        let (start, end, url) = g.url_at(row, col)?;
        Some((row, start, end, url))
    }

    /// which pane (id) sits under a pixel position
    fn pane_at(&self, x: f32, y: f32) -> Option<usize> {
        self.pw.layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(id, _)| *id)
    }

    fn pane_drop_at(pw: &PaneWindow, x: f32, y: f32) -> Option<PaneDropTarget> {
        let window = pw.window.as_ref()?.id();
        let (pane, rect) = pw
            .layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)?;
        Some(PaneDropTarget {
            window,
            tab: pw.active_tab,
            pane: *pane,
            side: pane_drop_side(*rect, x, y),
            rect: *rect,
        })
    }

    fn pane_tab_drop_at(pw: &PaneWindow, x: f32, y: f32) -> Option<(WindowId, usize)> {
        let window = pw.window.as_ref()?.id();
        let hit = pw.renderer.as_ref()?.hit_test(x, y);
        let index = pane_tab_drop_index(hit, pw.tabs.len())?;
        Some((window, index))
    }

    fn pane_drop_at_screen(&self, point: PhysicalPosition<i32>) -> Option<PaneDropDestination> {
        let hit = |pw: &PaneWindow| {
            let window = pw.window.as_ref()?;
            let origin = window.inner_position().ok()?;
            let x = point.x - origin.x;
            let y = point.y - origin.y;
            let size = window.inner_size();
            if x < 0 || y < 0 || x >= size.width as i32 || y >= size.height as i32 {
                return None;
            }
            Self::pane_drop_at(pw, x as f32, y as f32)
                .map(PaneDropDestination::Dock)
                .or_else(|| {
                    Self::pane_tab_drop_at(pw, x as f32, y as f32)
                        .map(|(window, index)| PaneDropDestination::Tab(window, index))
                })
        };
        self.satellites.iter().find_map(hit).or_else(|| hit(&self.pw))
    }

    #[cfg(target_os = "linux")]
    fn kwin_drag_point(
        &self,
        source: WindowId,
        local: PhysicalPosition<f64>,
    ) -> Option<PhysicalPosition<f64>> {
        let geometry = self.kwin_drag_geometry.iter().find(|geometry| geometry.window == source)?;
        let scale = self
            .pane_window_for(source)
            .and_then(|pw| pw.window.as_ref())
            .map(|window| window.scale_factor())?;
        Some(PhysicalPosition::new(
            geometry.x + local.x / scale,
            geometry.y + local.y / scale,
        ))
    }

    #[cfg(target_os = "linux")]
    fn kwin_tab_drop_at(&self, point: PhysicalPosition<f64>) -> Option<(WindowId, usize)> {
        for geometry in self.kwin_drag_geometry.iter().rev() {
            if point.x < geometry.x
                || point.y < geometry.y
                || point.x >= geometry.x + geometry.w
                || point.y >= geometry.y + geometry.h
            {
                continue;
            }
            let pw = self.pane_window_for(geometry.window)?;
            let scale = pw.window.as_ref()?.scale_factor();
            let x = ((point.x - geometry.x) * scale) as f32;
            let y = ((point.y - geometry.y) * scale) as f32;
            let index = pw
                .renderer
                .as_ref()
                .and_then(|renderer| match renderer.hit_test(x, y) {
                    Hit::Button(Hot::Tab(index) | Hot::TabClose(index)) => Some(index),
                    _ => None,
                })
                .unwrap_or(pw.tabs.len());
            return Some((geometry.window, index));
        }
        None
    }

    #[cfg(target_os = "linux")]
    fn kwin_pane_drop_at(&self, point: PhysicalPosition<f64>) -> Option<PaneDropDestination> {
        for geometry in self.kwin_drag_geometry.iter().rev() {
            if point.x < geometry.x
                || point.y < geometry.y
                || point.x >= geometry.x + geometry.w
                || point.y >= geometry.y + geometry.h
            {
                continue;
            }
            let pw = self.pane_window_for(geometry.window)?;
            let scale = pw.window.as_ref()?.scale_factor();
            let x = ((point.x - geometry.x) * scale) as f32;
            let y = ((point.y - geometry.y) * scale) as f32;
            return Self::pane_drop_at(pw, x, y)
                .map(PaneDropDestination::Dock)
                .or_else(|| {
                    Self::pane_tab_drop_at(pw, x, y)
                        .map(|(window, index)| PaneDropDestination::Tab(window, index))
                });
        }
        None
    }

    fn show_pane_drop(&mut self, target: Option<PaneDropTarget>) {
        let apply = |pw: &mut PaneWindow| {
            let own = pw.window.as_ref().map(|window| window.id());
            let drop = target.filter(|target| own == Some(target.window)).map(|target| (target.rect, target.side));
            if let Some(renderer) = pw.renderer.as_mut() {
                renderer.set_pane_drop(drop);
            }
            if let Some(window) = pw.window.as_ref() {
                window.request_redraw();
            }
        };
        apply(&mut self.pw);
        for pw in &mut self.satellites {
            apply(pw);
        }
    }

    fn show_tab_drop(&mut self, target: Option<(WindowId, usize)>) {
        let apply = |pw: &mut PaneWindow| {
            let own = pw.window.as_ref().map(|window| window.id());
            let index = target.filter(|(window, _)| own == Some(*window)).map(|(_, index)| index);
            if let Some(renderer) = pw.renderer.as_mut() {
                renderer.set_tab_drop(index);
            }
            if let Some(window) = pw.window.as_ref() {
                window.request_redraw();
            }
        };
        apply(&mut self.pw);
        for pw in &mut self.satellites {
            apply(pw);
        }
    }

    fn show_drag_preview(&mut self, preview: Option<(WindowId, f32, f32, String, bool)>) {
        let apply = |pw: &mut PaneWindow| {
            let own = pw.window.as_ref().map(|window| window.id());
            let own_preview = preview
                .as_ref()
                .filter(|(window, ..)| own == Some(*window))
                .map(|(_, x, y, label, pane)| (*x, *y, label.clone(), *pane));
            if let Some(renderer) = pw.renderer.as_mut() {
                renderer.set_drag_preview(own_preview);
            }
            if let Some(window) = pw.window.as_ref() {
                window.request_redraw();
            }
        };
        apply(&mut self.pw);
        for pw in &mut self.satellites {
            apply(pw);
        }
    }

    fn reset_drag_cursors(&mut self) {
        let reset = |pw: &mut PaneWindow| {
            pw.cursor_icon = CursorIcon::Default;
            if let Some(window) = pw.window.as_ref() {
                window.set_cursor(CursorIcon::Default);
            }
        };
        reset(&mut self.pw);
        for pw in &mut self.satellites {
            reset(pw);
        }
    }

    fn focused_grid(&self) -> Option<&crate::grid::Grid> {
        let id = self.active_focused_id()?;
        let root = self.pw.tabs.get(self.pw.active_tab)?.root.as_ref()?;
        find_pane(root, id).map(|p| &p.term.grid)
    }

    fn focused_grid_mut(&mut self) -> Option<&mut crate::grid::Grid> {
        let id = self.active_focused_id()?;
        let root = self.pw.tabs.get_mut(self.pw.active_tab)?.root.as_mut()?;
        find_pane_mut(root, id).map(|p| &mut p.term.grid)
    }

    /// open the font picker: a searchable list of every installed monospace
    /// family, previewed live as you move through it, committed on Enter and
    /// reverted on Esc. reuses the command-palette overlay box
    fn open_font_picker(&mut self) {
        if self.font_families.is_empty() {
            // the system-font scan is deferred to about_to_wait; force it now
            // so the very first open still has the full list
            if let Some(r) = self.pw.renderer.as_mut() {
                r.ensure_system_fonts();
                self.font_families = r.monospace_families();
            }
        }
        if self.font_families.is_empty() {
            self.show_notice("no monospace fonts found");
            return;
        }
        self.font_pick_orig =
            self.pw.renderer.as_ref().map(|r| r.font_name().to_string());
        self.font_pick = Some(PaletteState {
            query: String::new(),
            selected: 0,
            mode: PaletteMode::Commands,
        });
        // start the highlight on the current font if it's in the list
        if let Some(cur) = self.font_pick_orig.as_deref() {
            let list = self.font_pick_filter("");
            if let Some(i) = list.iter().position(|f| f.eq_ignore_ascii_case(cur))
                && let Some(p) = self.font_pick.as_mut()
            {
                p.selected = i;
            }
        }
        self.redraw();
    }

    /// monospace families matching the picker query (fuzzy, best-first);
    /// empty query returns the full list
    fn font_pick_filter(&self, query: &str) -> Vec<String> {
        let q = query.trim();
        if q.is_empty() {
            return self.font_families.clone();
        }
        let mut scored: Vec<(i32, &String)> = self
            .font_families
            .iter()
            .filter_map(|f| fuzzy_score(q, &f.to_ascii_lowercase()).map(|s| (s, f)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, f)| f.clone()).collect()
    }

    fn palette_choices(&self, mode: PaletteMode, query: &str) -> Vec<(String, PaletteAction)> {
        match mode {
            PaletteMode::Commands => palette_filter(query)
                .into_iter()
                .map(|(label, action)| (label.to_string(), action))
                .collect(),
            PaletteMode::Tabs => {
                let labels: Vec<String> = self.pw.tabs.iter().map(tab_label).collect();
                tab_filter(query, &labels)
                    .into_iter()
                    .map(|(label, tab)| (label, PaletteAction::SelectTab(tab)))
                    .collect()
            }
        }
    }

    fn open_tab_search(&mut self) {
        self.palette = Some(PaletteState {
            query: String::new(),
            selected: self.pw.active_tab,
            mode: PaletteMode::Tabs,
        });
        self.redraw();
    }

    fn run_palette_choice(
        &mut self,
        mode: PaletteMode,
        query: &str,
        selected: usize,
        event_loop: &ActiveEventLoop,
    ) {
        let choices = self.palette_choices(mode, query);
        let index = selected.min(choices.len().saturating_sub(1));
        if let Some(&(_, action)) = choices.get(index) {
            self.run_action(action, event_loop);
        }
    }

    /// preview the highlighted font without committing (live as you navigate)
    fn font_pick_preview(&mut self) {
        let Some(p) = self.font_pick.as_ref() else {
            return;
        };
        let list = self.font_pick_filter(&p.query);
        let Some(name) = list.get(p.selected).cloned() else {
            return;
        };
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_font_by_name(&name);
        }
        self.relayout_all();
        self.redraw();
    }

    /// commit (Some) or cancel (None, restoring the original) the font pick
    fn close_font_picker(&mut self, commit: bool) {
        let orig = self.font_pick_orig.take();
        let picked = self.font_pick.take();
        let chosen = if commit {
            picked.and_then(|p| self.font_pick_filter(&p.query).get(p.selected).cloned())
        } else {
            None
        };
        if let Some(name) = chosen {
            if let Some(r) = self.pw.renderer.as_mut() {
                r.set_font_by_name(&name);
            }
            self.persisted.font = Some(name);
            self.relayout_all();
            self.save_config();
        } else if let (Some(name), Some(r)) = (orig, self.pw.renderer.as_mut()) {
            // cancel, or enter with nothing under the highlight: revert the
            // live preview to the font that was active when the picker opened
            r.set_font_by_name(&name);
            self.relayout_all();
        }
        self.redraw();
    }

    fn open_find(&mut self) {
        // a single-line selection on the focused pane seeds the query, the
        // way editors prefill find; multi-line selections don't make sense
        // as a substring search so they leave the box empty
        let seed = self
            .selection
            .filter(|s| Some(s.pane) == self.active_focused_id())
            .and_then(|s| {
                self.pw.tabs
                    .get(self.pw.active_tab)
                    .and_then(|t| t.root.as_ref())
                    .and_then(|r| find_pane(r, s.pane))
                    .filter(|p| p.term.grid.reflow_gen == s.reflow_gen)
                    .map(|p| p.term.grid.selected_text(s.start, s.end, s.block))
            })
            .filter(|t| !t.is_empty() && !t.contains('\n') && t.chars().count() <= 128)
            .unwrap_or_default();
        let seeded = !seed.is_empty();
        self.find = Some(FindState {
            query: seed,
            matches: Vec::new(),
            current: 0,
            bad: false,
        });
        if seeded {
            self.find_recompute();
        }
        self.redraw();
    }

    /// re-run the search for the current query against the focused pane and jump
    /// to the first match. in regex mode a pattern that fails to compile shows
    /// as "bad pattern" instead of silently matching nothing
    fn find_recompute(&mut self) {
        let query = match &self.find {
            Some(f) => f.query.clone(),
            None => return,
        };
        let mut bad = false;
        let matches = if self.find_regex && !query.is_empty() {
            match regex::Regex::compile(&query) {
                Some(re) => self
                    .focused_grid()
                    .map(|g| g.search_regex(&re))
                    .unwrap_or_default(),
                None => {
                    bad = true;
                    Vec::new()
                }
            }
        } else {
            self.focused_grid().map(|g| g.search(&query)).unwrap_or_default()
        };
        if let Some(f) = self.find.as_mut() {
            let (m, cur) = find_after_grid_change(matches);
            f.matches = m;
            f.current = cur;
            f.bad = bad;
        }
        self.find_scroll_to_current();
        self.redraw();
    }

    /// focus identity for the find-follow-focus rule: (active_tab, focused pane id).
    /// only the pane id is stable for the view-change predicate; tab index is
    /// carried so callers can restore viewer slots after background closes
    fn focus_identity(&self) -> Option<(usize, usize)> {
        let tab = self.pw.tabs.get(self.pw.active_tab)?;
        Some((self.pw.active_tab, tab.focused))
    }

    /// finish a UI update that may have retargeted the focused pane: re-run find
    /// when it is open and the focused *pane id* changed, otherwise plain redraw.
    /// when `find_follow_hold` is set (temporary owner-tab switch for a
    /// background exit), only redraw so the viewer's match list stays intact
    fn after_focus_context_change(&mut self, before: Option<(usize, usize)>) {
        let after = self.focus_identity();
        if find_must_follow_focus(self.find.is_some(), before, after, self.find_follow_hold) {
            self.find_recompute();
        } else {
            self.redraw();
        }
    }

    /// close a pane that may live in a non-active tab without yanking find onto
    /// that tab: hold find-follow across the temporary switch, restore the
    /// viewer's tab, then recompute once against the final identity
    fn close_pane_keeping_viewer(&mut self, id: usize, event_loop: &ActiveEventLoop) {
        let owner = self.pw.tabs.iter().position(|t| {
            t.root.as_ref().map(|r| find_pane(r, id).is_some()).unwrap_or(false)
        });
        let Some(ti) = owner else {
            return;
        };
        let viewer_before = self.focus_identity();
        let prev_active = self.pw.active_tab;
        let tabs_before = self.pw.tabs.len();
        self.find_follow_hold = true;
        self.pw.active_tab = ti;
        self.close_focused_pane_by_id(id, event_loop);
        self.find_follow_hold = false;
        if self.pw.tabs.is_empty() {
            return;
        }
        let tabs_after = self.pw.tabs.len();
        let tab_removed = tabs_after < tabs_before;
        if let Some(restored) = restore_viewer_tab(prev_active, ti, tabs_after, tab_removed)
            && restored != self.pw.active_tab
        {
            self.pw.active_tab = restored;
            self.relayout_all();
        }
        self.sync_tabs();
        self.after_focus_context_change(viewer_before);
    }

    fn find_scroll_to_current(&mut self) {
        let target = self
            .find
            .as_ref()
            .and_then(|f| f.matches.get(f.current).copied());
        if let Some((g, _, _)) = target
            && let Some(grid) = self.focused_grid_mut() {
                grid.scroll_to_global(g);
            }
    }

    fn find_step(&mut self, forward: bool) {
        let len = self.find.as_ref().map(|f| f.matches.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        if let Some(f) = self.find.as_mut() {
            f.current = if forward {
                (f.current + 1) % len
            } else {
                (f.current + len - 1) % len
            };
        }
        self.find_scroll_to_current();
        self.redraw();
    }

    /// build the renderer's find overlay view: the query/count for the box plus
    /// on-screen match rects (viewport row, col, len, is_current) for the focused
    /// pane
    fn build_find_view(&self) -> Option<render::FindView> {
        let f = self.find.as_ref()?;
        let mut vps = Vec::new();
        if !f.query.is_empty()
            && let Some(g) = self.focused_grid() {
                for (i, &(gl, col, len)) in f.matches.iter().enumerate() {
                    if let Some(vr) = g.global_to_viewport(gl) {
                        vps.push((vr, col, len, i == f.current));
                    }
                }
            }
        Some(render::FindView {
            query: f.query.clone(),
            count: f.matches.len(),
            current: f.current,
            matches: vps,
            regex_on: self.find_regex,
            bad: f.bad,
        })
    }

    /// the pane whose scroll-thumb grab strip is under (cx, cy), with its thumb
    /// geometry, or None. the strip is the thin bar plus a few px of slop so the
    /// 2px thumb is still easy to grab
    fn scrollbar_hit(&self, cx: f32, cy: f32) -> Option<(usize, render::ScrollThumb)> {
        let r = self.pw.renderer.as_ref()?;
        let root = self.pw.tabs.get(self.pw.active_tab)?.root.as_ref()?;
        for (id, rect) in &self.pw.layout_cache {
            let Some(p) = find_pane(root, *id) else { continue };
            let g = &p.term.grid;
            let Some(t) = r.scrollbar_for(*rect, g.scrollback.len(), g.view_offset) else {
                continue;
            };
            let slop = 8.0;
            if cx >= t.track_x - slop
                && cx <= t.track_x + t.track_w + slop
                && cy >= t.track_y
                && cy <= t.track_y + t.track_h
            {
                return Some((*id, t));
            }
        }
        None
    }

    /// set pane `id`'s scroll offset from a pointer-y, honouring the grab offset
    /// recorded when the thumb was first pressed
    fn apply_scrollbar_drag(&mut self, id: usize, cy: f32) {
        let grab_dy = match self.sb_drag {
            Some((p, dy)) if p == id => dy,
            _ => 0.0,
        };
        let Some(rect) = self.pw.layout_cache.iter().find(|(i, _)| *i == id).map(|(_, r)| *r) else {
            return;
        };
        let scrollback = match self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|root| find_pane(root, id))
        {
            Some(p) => p.term.grid.scrollback.len(),
            None => return,
        };
        let Some(off) = self
            .pw.renderer
            .as_ref()
            .map(|r| r.scroll_offset_at(rect, scrollback, cy - grab_dy))
        else {
            return;
        };
        if let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut())
            && let Some(p) = find_pane_mut(root, id)
        {
            p.term.grid.view_offset = off.min(p.term.grid.scrollback.len());
        }
    }

    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else {
            return;
        };
        let text = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, sel.pane))
            // a reflow since mouse-down re-based the line ids; copying through
            // stale anchors would grab lines the user never highlighted
            .filter(|p| p.term.grid.reflow_gen == sel.reflow_gen)
            .map(|p| p.term.grid.selected_text(sel.start, sel.end, sel.block))
            .unwrap_or_default();
        if !text.is_empty() {
            win::clipboard_set(&text);
        }
    }

    fn select_all(&mut self) {
        let Some(pane) = self.active_focused_id() else {
            return;
        };
        self.selection = self.focused_grid().and_then(|g| {
            let (start, end) = g.full_span()?;
            Some(Sel { pane, start, end, block: false, reflow_gen: g.reflow_gen })
        });
        self.redraw();
    }

    fn paste(&mut self) {
        let text = win::clipboard_get();
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let Some(id) = self.active_focused_id() else {
            return;
        };
        let bracketed = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, id))
            .map(|p| p.term.bracketed_paste)
            .unwrap_or(false);
        let mut bytes = Vec::new();
        if bracketed {
            // a crafted clipboard carrying its own end-bracket could close the
            // paste early and run the rest as keystrokes; strip the sequence
            let sanitized = normalized.replace("\x1b[201~", "");
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(sanitized.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(normalized.as_bytes());
        }
        // without bracketed paste a multiline paste runs each line as its own
        // command the moment it lands; hold it behind a confirm so a stray paste
        // can't fire a string of commands. bracketed-paste programs (modern
        // shells, full-screen apps) buffer the whole paste safely, so they go straight
        let multiline = normalized.trim_end_matches('\r').contains('\r');
        if !bracketed && multiline {
            let lines = normalized.split('\r').filter(|l| !l.is_empty()).count();
            self.pw.confirm = Some(ConfirmState {
                prompt: format!("paste {lines} lines into a program with no paste protection?"),
                hint: "enter: paste \u{b7} esc: cancel".to_string(),
                action: ConfirmAction::PasteBytes { pane: id, bytes },
            });
            self.redraw();
        } else {
            self.send_paste_bytes(id, &bytes);
        }
    }

    /// write already-encoded paste bytes to a pane by id
    fn send_paste_bytes(&mut self, pane: usize, bytes: &[u8]) {
        if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
            && let Some(root) = tab.root.as_mut()
            && let Some(p) = find_pane_mut(root, pane)
        {
            // pasting is input: snap a history-scrolled view to the bottom
            p.term.grid.view_offset = 0;
            p.pty.write(bytes);
        }
    }

    /// run a confirmed modal action
    fn run_confirm(&mut self, action: ConfirmAction, event_loop: &ActiveEventLoop) {
        match action {
            ConfirmAction::PasteBytes { pane, bytes } => self.send_paste_bytes(pane, &bytes),
            ConfirmAction::CloseTab { tab } => self.do_close_tab(tab, event_loop),
            ConfirmAction::CloseOthers { keep } => self.do_close_others(keep),
            ConfirmAction::Quit => self.quit_app(event_loop),
            ConfirmAction::InstallUpdate => {
                if let Some(u) = self.update.clone() {
                    if update::can_install() {
                        self.show_notice(&format!("downloading {}\u{2026}", u.version));
                        let proxy = self.proxy.clone();
                        std::thread::spawn(move || {
                            let _ = proxy.send_event(UserEvent::UpdateDownloaded(update::download(&u)));
                        });
                    } else {
                        win::open_url(&format!(
                            "https://github.com/zeo/termie/releases/tag/v{}",
                            u.version
                        ));
                        self.show_notice(&format!("release page for {} opened", u.version));
                    }
                }
            }
            ConfirmAction::CloseWindow => {
                // kill + empty the swapped-in window's tabs; satellite_event's
                // post-swap cleanup removes the now-empty window
                for tab in &mut self.pw.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
                self.pw.tabs.clear();
            }
        }
    }

    /// panes across every tab of the current window
    fn window_pane_count(&self) -> usize {
        let mut n = 0;
        for tab in &self.pw.tabs {
            if let Some(root) = tab.root.as_ref() {
                each_pane(root, &mut |_| n += 1);
            }
        }
        n
    }

    /// the actual shutdown: kill every shell, flush the session, exit
    fn quit_app(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = self.pw.window.as_ref() {
            win::set_taskbar_progress(window, 0, 0);
        }
        for tab in &mut self.pw.tabs {
            if let Some(root) = tab.root.as_mut() {
                kill_all(root);
            }
        }
        self.flush_session_now();
        self.kill_pool();
        win::clipboard_shutdown();
        self.release_window_resources();
        event_loop.exit();
    }

    /// quit, held behind a confirm when more than one pane or tab is alive —
    /// the same count gate tab-close uses, so Alt+F4 can't silently take down
    /// a window full of working agents
    fn request_quit(&mut self, event_loop: &ActiveEventLoop) {
        let panes = self.window_pane_count();
        if self.pw.tabs.len() > 1 || panes > 1 {
            self.pw.confirm = Some(ConfirmState {
                prompt: format!(
                    "quit with {panes} panes across {} tabs?",
                    self.pw.tabs.len()
                ),
                hint: "enter: quit \u{b7} esc: cancel".to_string(),
                action: ConfirmAction::Quit,
            });
            self.redraw();
        } else {
            self.quit_app(event_loop);
        }
    }

    /// render one frame: window title + every visible pane
    fn paint(&mut self) {
        // a minimized or zero-area window has no real surface to present to;
        // painting it anyway lets the compositor keep a stretched/stale frame
        // that flashes huge and blurry on restore/alt-tab. skip the frame and
        // leave pty_dirty set so the resize+focus events on restore repaint the
        // latest grid at the true size
        if let Some(w) = self.pw.window.as_ref() {
            if w.is_minimized().unwrap_or(false) {
                return;
            }
            let s = w.inner_size();
            if s.width == 0 || s.height == 0 {
                return;
            }
        }
        let clock = win::local_hm();
        let focus_ease = self.focus_ease();
        let git = self.pw.git.clone();
        let sessions = self.pw.tabs.len();
        // the font picker reuses the palette overlay box: when it's open, feed
        // build_palette the filtered font list instead of the action list
        let palette_view = if let Some(fp) = self.font_pick.as_ref() {
            Some(render::PaletteView {
                query: fp.query.clone(),
                items: self.font_pick_filter(&fp.query),
                selected: fp.selected,
                scope: "fonts",
            })
        } else {
            self.palette.as_ref().map(|p| {
                let items: Vec<String> =
                    self.palette_choices(p.mode, &p.query).into_iter().map(|(l, _)| l).collect();
                render::PaletteView {
                    query: p.query.clone(),
                    selected: p.selected.min(items.len().saturating_sub(1)),
                    items,
                    scope: match p.mode {
                        PaletteMode::Commands => "commands",
                        PaletteMode::Tabs => "tabs",
                    },
                }
            })
        };
        let find_view = self.build_find_view();
        let market_view = self.market.as_ref().map(|m| render::MarketView {
            rows: m
                .rows
                .iter()
                .map(|r| render::MarketRowView {
                    name: r.name.clone(),
                    version: r.version.clone(),
                    description: r.description.clone(),
                    permissions: r.permissions.clone(),
                    installed: r.installed,
                    enabled: r.enabled,
                })
                .collect(),
            selected: m.selected,
            status: m.status.clone(),
            loading: m.loading,
            fetch_failed: m.fetch_failed,
        });
        let config = self.config;
        let settings_open = self.pw.settings_open;
        let settings_p = self.settings_p();
        let tab_color_of = |t: usize| self.pw.tabs.get(t).and_then(|tab| tab.color);
        let pane_menu_view = self.pw.pane_menu.as_ref().map(|m| {
            let items: Vec<String> = match m.target {
                MenuTarget::Pane => render::PANE_MENU_ITEMS.iter().map(|s| s.to_string()).collect(),
                MenuTarget::Tab(_) => render::TAB_MENU_ITEMS.iter().map(|s| s.to_string()).collect(),
                MenuTarget::TabColor(t) => {
                    let current = tab_color_of(t);
                    render::TAB_COLOR_ITEMS
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let sel = if i == 0 { current.is_none() } else { current == Some(i as u8) };
                            if sel { format!("{s} \u{f00c}") } else { s.to_string() }
                        })
                        .collect()
                }
                MenuTarget::NewTab => new_tab_menu_entries().into_iter().map(|(l, _)| l).collect(),
            };
            // the color menu's rows carry their swatch tint (row 0 is "none")
            let accents = match m.target {
                MenuTarget::TabColor(_) => {
                    (0..render::TAB_COLOR_ITEMS.len()).map(|i| (i > 0).then_some(i as u8)).collect()
                }
                _ => Vec::new(),
            };
            render::PaneMenuView { x: m.x, y: m.y, hovered: m.hovered, items, accents }
        });
        let hud = if self.persisted.latency_hud { self.lat.hud() } else { None };
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_latency_hud(hud);
            r.set_status(git, clock, sessions);
            r.set_palette(palette_view);
            r.set_pane_menu(pane_menu_view);
            r.set_find(find_view);
            r.set_market(market_view);
            r.set_confirm(self.pw.confirm.as_ref().map(|c| render::ConfirmView {
                prompt: c.prompt.clone(),
                hint: c.hint.clone(),
            }));
            r.set_rename(self.pw.rename.as_ref().map(|rs| render::RenameView { buf: rs.buf.clone() }));
            r.set_settings(render::SettingsView {
                scrollback: config.scrollback,
                copy_on_select: config.copy_on_select,
                load_profile: config.load_profile,
                theme_auto: self.persisted.theme_auto,
                acrylic: self.persisted.acrylic,
                shell_name: config.shell.label(),
                close_action_name: config.close_action.label(),
                backend_name: config.backend.label(),
            });
            r.set_settings_panel(settings_open, settings_p);
        }
        let title = self
            .pw.tabs
            .get(self.pw.active_tab)
            .filter(|t| {
                t.root
                    .as_ref()
                    .and_then(|r| find_pane(r, t.focused))
                    .is_some_and(|p| p.term.cwd.is_some() || !p.term.title.is_empty())
            })
            .map(|t| format!("{} — termie", tab_label(t)))
            .unwrap_or_else(|| "termie".to_string());
        // an elevated window must say so everywhere it's named
        let title = if self.elevated() { format!("{title} [admin]") } else { title };
        if self.last_title != title {
            if let Some(w) = &self.pw.window {
                w.set_title(&title);
            }
            self.last_title = title;
        }
        let mut present_now: Option<Instant> = None;
        let App {
            pw,
            selection,
            link,
            persisted,
            ..
        } = self;
        let PaneWindow {
            renderer,
            tabs,
            active_tab,
            layout_cache,
            maximized,
            focused,
            ime_preedit,
            ime_preedit_caret,
            ..
        } = pw;
        if let Some(r) = renderer.as_mut() {
            // render even before the first pane exists (async startup) so the
            // window/chrome can appear immediately; views is just empty then
            let views: Vec<PaneView> = match tabs.get(*active_tab) {
                Some(tab) => tab
                    .root
                    .as_ref()
                    .map(|root| {
                        layout_cache
                            .iter()
                            .filter_map(|(id, rect)| {
                                find_pane(root, *id).map(|p| PaneView {
                                    term: &p.term,
                                    rect: *rect,
                                    focused: *id == tab.focused,
                                    sel: selection.filter(|s| s.pane == *id).and_then(|s| sel_view_span(&p.term.grid, &s)),
                                    flash: p
                                        .flash
                                        .map(|t| {
                                            // hold full for 120ms, then ease to 0 by 220ms
                                            let e = t.elapsed().as_millis() as f32;
                                            (1.0 - (e - 120.0) / 100.0).clamp(0.0, 1.0)
                                        })
                                        .unwrap_or(0.0),
                                    link: if *id == tab.focused { *link } else { None },
                                    theme: persisted
                                        .shell_themes
                                        .iter()
                                        .find(|(n, _)| n == p.shell.label())
                                        .map(|&(_, id)| id),
                                    status: p.status.rank(),
                                    preedit: (*id == tab.focused && !ime_preedit.is_empty()).then_some(
                                        render::PreeditView { text: ime_preedit, caret: *ime_preedit_caret },
                                    ),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            match r.render(&views, *focused, *maximized, focus_ease, false) {
                Ok(()) => present_now = Some(Instant::now()),
                Err(e) => log::error!("render error: {e:#}"),
            }
        }
        // input-to-photon + frame-interval sampling for the latency hud
        if let Some(now) = present_now {
            if let Some(prev) = self.last_present.replace(now) {
                self.lat.record_frame((now - prev).as_secs_f32() * 1000.0);
            }
            if let Some(t) = self.input_at.take() {
                self.lat.record_input((now - t).as_secs_f32() * 1000.0);
            }
        }
    }

    /// recompute every tab's pane rects and resize each pane's term + pty
    fn relayout_all(&mut self) {
        let Some(r) = self.pw.renderer.as_ref() else {
            return;
        };
        // a settings tab has no panes; clear so stale rects don't linger
        self.pw.layout_cache.clear();
        let content = r.content_rect();
        let (_, _, pool_cols, pool_rows) = r.pane_metrics(content);
        // keep ready pool shells sized to a full content pane; resizing a shell
        // mid-PSReadLine-startup wedges it, so only touch ready ones. only the
        // MAIN window draws from the shared pool, so don't resize it to a
        // torn-off window's size while a satellite is swapped into self.pw
        // (that would ping-pong the pool and lose the warm-open size match)
        let (cell_w, cell_h) = r.cell_px();
        if self.cur_sat.is_none() {
            for sp in &mut self.pool {
                sp.term.set_cell_px(cell_w, cell_h);
                if sp.ready && (sp.term.grid.cols != pool_cols || sp.term.grid.rows != pool_rows) {
                    sp.resize(pool_rows, pool_cols);
                }
            }
        }
        for (ti, tab) in self.pw.tabs.iter_mut().enumerate() {
            // a zoomed leaf fills the whole content area; drop a stale zoom whose
            // pane no longer exists (validated before the mutable root borrow)
            let zoom = tab
                .zoom
                .filter(|&zid| tab.root.as_ref().map(|r| find_pane(r, zid).is_some()).unwrap_or(false));
            tab.zoom = zoom;
            let Some(root) = tab.root.as_mut() else {
                continue;
            };
            let mut rects = Vec::new();
            if let Some(zid) = zoom {
                rects.push((zid, content));
            } else {
                layout(root, content, &mut rects);
            }
            for (id, rect) in &rects {
                let (_, _, cols, rows) = r.pane_metrics(*rect);
                if let Some(p) = find_pane_mut(root, *id) {
                    p.term.set_cell_px(cell_w, cell_h);
                    // skip redundant resizes, and never resize a shell that hasn't
                    // produced output yet — resizing pwsh mid-PSReadLine-startup
                    // wedges it (same guard the warm pool already uses above)
                    if p.ready && (p.term.grid.rows != rows || p.term.grid.cols != cols) {
                        p.resize(rows, cols);
                    }
                }
            }
            if ti == self.pw.active_tab {
                self.pw.layout_cache = rects;
            }
        }
    }

    /// toggle tmux-style zoom on the focused pane: it fills the whole content
    /// area, hiding its siblings, until toggled off (no-op with a single pane)
    fn toggle_zoom(&mut self) {
        if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) {
            tab.zoom = if tab.zoom.is_some() { None } else { Some(tab.focused) };
        }
        self.relayout_all();
        self.redraw();
    }

    fn sync_tabs(&mut self) {
        // viewing a tab in a focused window acknowledges its bell, and its
        // finished panes are seen — their done badges retire
        if self.pw.focused
            && let Some(t) = self.pw.tabs.get_mut(self.pw.active_tab)
        {
            t.attention = false;
            if let Some(root) = t.root.as_mut() {
                each_pane_mut(root, &mut |p| {
                    if matches!(p.status, PaneStatus::Done(_)) {
                        p.status = PaneStatus::Idle;
                    }
                });
            }
        }
        let labels: Vec<String> = self.pw.tabs.iter().map(tab_label).collect();
        // per-tab badge rollup: failed > bell > done > running > nothing
        let attention: Vec<u8> = self
            .pw.tabs
            .iter()
            .map(|t| {
                let mut s = if t.attention { 3 } else { 0 };
                if let Some(root) = t.root.as_ref() {
                    each_pane(root, &mut |p| s = s.max(p.status.rank()));
                }
                s
            })
            .collect();
        let active = self.pw.active_tab;
        let cwd: Option<String> = self
            .pw.tabs
            .get(active)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, t.focused)))
            .and_then(|p| p.term.cwd.clone());
        // only walk the filesystem for .git/HEAD when the cwd actually changed
        if cwd != self.pw.last_git_cwd {
            self.pw.git = git_branch(cwd.as_deref());
            self.pw.last_git_cwd = cwd;
        }
        let colors: Vec<Option<u8>> = self.pw.tabs.iter().map(|t| t.color).collect();
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_tabs(labels, active);
            r.set_tab_status(attention);
            r.set_tab_colors(colors);
        }
        // sync_tabs runs after every structural / focus / cwd change (never per
        // frame), so it's the chokepoint to schedule a debounced session write
        self.mark_session_dirty();
    }

    fn active_focused_id(&self) -> Option<usize> {
        self.pw.tabs.get(self.pw.active_tab).map(|t| t.focused)
    }

    /// push the current accessibility tree to the adapter — a cheap no-op when no
    /// assistive tech is attached (the flatten only runs while active)
    fn update_a11y(&mut self) {
        if let Some(mut adapter) = self.pw.a11y.take() {
            adapter.update_if_active(|| self.build_a11y_update());
            self.pw.a11y = Some(adapter);
        }
    }

    fn update_window_a11y(&mut self, id: WindowId) {
        if self.pw.window.as_ref().map(|window| window.id()) == Some(id) {
            self.update_a11y();
        } else if let Some(index) = self.satellite_for(id) {
            self.with_window(index, |app| app.update_a11y());
        }
    }

    fn update_all_a11y(&mut self) {
        self.update_a11y();
        for index in 0..self.satellites.len() {
            self.with_window(index, |app| app.update_a11y());
        }
    }

    fn build_a11y_update(&self) -> accesskit::TreeUpdate {
        let bounds = self.pw.window.as_ref().map(|w| {
            let s = w.inner_size();
            accesskit::Rect::new(0.0, 0.0, s.width as f64, s.height as f64)
        });
        let text = self
            .active_focused_id()
            .and_then(|id| {
                self.pw.tabs
                    .get(self.pw.active_tab)
                    .and_then(|t| t.root.as_ref())
                    .and_then(|r| find_pane(r, id))
            })
            .map(|p| a11y::flatten(&p.term))
            .unwrap_or_default();
        a11y::build_tree(&text, "termie", bounds)
    }

    /// write bytes to the focused pane, or every pane in broadcast mode — the
    /// shared sink for keyboard input and committed IME text (no esc/alt prefix)
    fn write_to_focused(&mut self, bytes: &[u8]) {
        let Some(id) = self.active_focused_id() else {
            return;
        };
        if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
            && let Some(root) = tab.root.as_mut()
        {
            // typing snaps a history-scrolled view back to the live bottom
            // (output alone no longer does — it would yank the user mid-read)
            if self.broadcast {
                each_pane_mut(root, &mut |p| {
                    p.term.grid.view_offset = 0;
                    p.pty.write(bytes);
                });
            } else if let Some(p) = find_pane_mut(root, id) {
                p.term.grid.view_offset = 0;
                p.pty.write(bytes);
            }
        }
    }

    /// IME lifecycle for whichever window is swapped into self.pw: the preedit
    /// draws inline until the composition commits or cancels
    fn on_ime(&mut self, ime: Ime) {
        match ime {
            Ime::Enabled => {}
            Ime::Preedit(text, cursor) => {
                self.pw.ime_composing = !text.is_empty();
                self.pw.ime_preedit = text;
                self.pw.ime_preedit_caret = cursor;
                self.apply_ime_area();
                self.redraw();
            }
            Ime::Commit(text) => {
                self.pw.ime_composing = false;
                self.pw.ime_preedit.clear();
                self.pw.ime_preedit_caret = None;
                self.write_to_focused(text.as_bytes());
                self.redraw();
            }
            Ime::Disabled => {
                self.pw.ime_composing = false;
                self.pw.ime_preedit.clear();
                self.pw.ime_preedit_caret = None;
                self.redraw();
            }
        }
    }

    /// park the OS IME candidate window at the focused pane's cursor cell,
    /// advanced to the caret within an in-flight composition so the candidate
    /// list tracks the character being converted
    fn apply_ime_area(&mut self) {
        let Some(id) = self.active_focused_id() else {
            return;
        };
        let caret_cells = self
            .pw
            .ime_preedit_caret
            .map(|(s, _)| render::preedit_cell_offset(&self.pw.ime_preedit, s))
            .unwrap_or(0);
        let rect = self.pw.layout_cache.iter().find(|(pid, _)| *pid == id).map(|(_, r)| *r);
        let cursor = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, id))
            .map(|p| {
                let g = &p.term.grid;
                (g.cursor.row, (g.cursor.col + caret_cells).min(g.cols.saturating_sub(1)))
            });
        let area = match (rect, cursor, self.pw.renderer.as_ref()) {
            (Some(rect), Some((row, col)), Some(r)) => Some(r.cell_screen_rect(rect, row, col)),
            _ => None,
        };
        if let (Some((x, y, cw, ch)), Some(w)) = (area, self.pw.window.as_ref()) {
            w.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(x, y),
                winit::dpi::PhysicalSize::new(cw, ch),
            );
        }
    }

    /// the focused pane's working directory (from OSC 7), as a filesystem path
    fn focused_cwd(&self) -> Option<String> {
        let id = self.active_focused_id()?;
        let root = self.pw.tabs.get(self.pw.active_tab)?.root.as_ref()?;
        let p = find_pane(root, id)?;
        cwd_path(p.term.cwd.as_deref())
    }

    /// the shell kind the focused pane was spawned with
    fn focused_shell(&self) -> Option<ShellKind> {
        let id = self.active_focused_id()?;
        let root = self.pw.tabs.get(self.pw.active_tab)?.root.as_ref()?;
        Some(find_pane(root, id)?.shell)
    }

    fn new_tab(&mut self) {
        self.new_tab_cwd(None, None);
    }

    /// a default-terminal session arriving while termie runs opens as a new
    /// tab in this window, the way windows terminal does it
    #[cfg(windows)]
    fn handoff_tab(&mut self, h: defterm::Handoff) {
        log::info!("defterm: opening handoff tab (title={:?})", h.title);
        if self.pw.renderer.is_none() {
            return;
        }
        let before = self.focus_identity();
        let (cols, rows) = self.content_pane_size();
        let title = h.title.clone();
        let pane = self.spawn_handoff_pane(h, cols, rows);
        let fid = pane.id;
        self.pw.tabs.push(Tab {
            focused: fid,
            root: Some(Node::Leaf(pane)),
            zoom: None,
            title: (!title.is_empty()).then_some(title),
            attention: false,
            color: None,
        });
        self.pw.active_tab = self.pw.tabs.len() - 1;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
        // the user just launched a console app; bring its window forward
        if let Some(w) = &self.pw.window {
            w.focus_window();
        }
        self.redraw();
    }

    /// open a new tab; a Some(cwd) or Some(shell) spawns a fresh shell, while
    /// None/None grabs a warm pool shell for an instant default-shell home tab
    fn new_tab_cwd(&mut self, cwd: Option<String>, shell: Option<ShellKind>) {
        if self.pw.renderer.is_none() {
            return;
        }
        let t0 = Instant::now();
        let (cols, rows) = self.content_pane_size();
        let from_pool = cwd.is_none()
            && shell.is_none()
            && self
                .pool
                .iter()
                .any(|p| p.term.grid.cols == cols && p.term.grid.rows == rows);
        let pane = if cwd.is_none()
            && shell.is_none()
            && let Some(i) = self
                .pool
                .iter()
                .position(|p| p.term.grid.cols == cols && p.term.grid.rows == rows)
        {
            // a ready pool shell already has its prompt — opening the tab is just
            // a move + relayout, no shell spawn on the critical path
            Ok(self.pool.remove(i))
        } else {
            self.spawn_pane(cols, rows, cwd, shell, None)
        };
        if let Ok(pane) = pane {
            let before = self.focus_identity();
            let fid = pane.id;
            self.pw.tabs.push(Tab {
                focused: fid,
                root: Some(Node::Leaf(pane)),
                zoom: None,
                title: None,
                attention: false,
                color: None,
            });
            self.pw.active_tab = self.pw.tabs.len() - 1;
            self.relayout_all();
            self.sync_tabs();
            self.after_focus_context_change(before);
            self.warm_pool();
            timing(&format!(
                "new tab ({}) in {:.2}ms",
                if from_pool { "warm pool" } else { "fresh spawn" },
                t0.elapsed().as_secs_f64() * 1000.0
            ));
        }
    }

    /// open (or focus) the settings tab
    /// open the slide-in settings panel (resets scroll to the top)
    fn open_settings(&mut self) {
        if !self.pw.settings_open {
            self.pw.settings_open = true;
            self.settings_anim = Some(Instant::now());
            self.refresh_settings_plugins();
            if let Some(r) = self.pw.renderer.as_mut() {
                r.reset_settings_scroll();
            }
            self.redraw();
        }
    }

    /// push the installed-plugin list (name, enabled) into the settings panel;
    /// the row order matches discover_plugins so a toggle can index back into it
    fn refresh_settings_plugins(&mut self) {
        let list: Vec<(String, bool)> = discover_plugins()
            .into_iter()
            .map(|d| (d.manifest.name, d.enabled))
            .collect();
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_plugins(list);
        }
    }

    fn close_settings(&mut self) {
        if self.pw.settings_open {
            self.pw.settings_open = false;
            self.settings_anim = Some(Instant::now());
            self.redraw();
        }
    }

    fn toggle_settings(&mut self) {
        if self.pw.settings_open {
            self.close_settings();
        } else {
            self.open_settings();
        }
    }

    /// docked fraction of the settings panel (0 = hidden, 1 = fully in)
    fn settings_p(&self) -> f32 {
        match self.settings_anim {
            None => {
                if self.pw.settings_open {
                    1.0
                } else {
                    0.0
                }
            }
            Some(t) => {
                let e = (t.elapsed().as_secs_f32() / self.settings_anim_dur()).clamp(0.0, 1.0);
                if self.pw.settings_open {
                    // ease-out cubic: rushes in, then settles
                    1.0_f32 - (1.0_f32 - e).powi(3)
                } else {
                    // ease-in-out (smoothstep): the close eases away from rest
                    // instead of snapping. a plain ease-out front-loaded the
                    // motion, so the panel looked like it vanished instantly
                    1.0_f32 - e * e * (3.0_f32 - 2.0_f32 * e)
                }
            }
        }
    }

    /// settings slide duration; the close runs a little longer so the slide back
    /// to the terminal is clearly visible rather than a blink
    fn settings_anim_dur(&self) -> f32 {
        if self.pw.settings_open { 0.14 } else { 0.22 }
    }

    /// 0→1 fade for the focused-pane accent border after a focus change, so it
    /// eases in rather than snapping (1.0 once settled)
    fn focus_ease(&self) -> f32 {
        const DUR: f32 = 0.16;
        match self.focus_anim {
            None => 1.0,
            Some(t) => {
                let e = (t.elapsed().as_secs_f32() / DUR).clamp(0.0, 1.0);
                1.0 - (1.0 - e).powi(3)
            }
        }
    }

    /// run an action; returns whether the key was consumed. all actions consume
    /// except prompt-jump, which passes through to the program when there are no
    /// OSC-133 marks to jump between
    fn run_action(&mut self, a: PaletteAction, event_loop: &ActiveEventLoop) -> bool {
        match a {
            PaletteAction::JumpPromptPrev => {
                let moved = self.focused_grid_mut().map(|g| g.jump_prompt(false)).unwrap_or(false);
                if moved {
                    self.redraw();
                }
                return moved;
            }
            PaletteAction::JumpPromptNext => {
                let moved = self.focused_grid_mut().map(|g| g.jump_prompt(true)).unwrap_or(false);
                if moved {
                    self.redraw();
                }
                return moved;
            }
            // scroll_view clamps to the scrollback length, so these are safe
            // no-ops on the alt screen (which has no history)
            PaletteAction::ScrollPageUp => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(g.rows.saturating_sub(1) as isize);
                }
                self.redraw();
            }
            PaletteAction::ScrollPageDown => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(-(g.rows.saturating_sub(1) as isize));
                }
                self.redraw();
            }
            PaletteAction::ScrollTop => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(g.scrollback.len() as isize);
                }
                self.redraw();
            }
            PaletteAction::ScrollBottom => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(-(g.view_offset as isize));
                }
                self.redraw();
            }
            PaletteAction::ClearScrollback => {
                if let Some(g) = self.focused_grid_mut() {
                    g.clear_scrollback();
                }
                self.redraw();
            }
            PaletteAction::ExportScrollback => {
                if let Some(g) = self.focused_grid_mut() {
                    let text = g.full_text();
                    match export_scrollback(&text) {
                        Ok(path) => self.show_notice(&format!("exported to {}", path.display())),
                        Err(e) => self.show_notice(&format!("export failed: {e}")),
                    }
                }
                self.redraw();
            }
            PaletteAction::InstallUpdate => {
                if let Some(u) = &self.update {
                    // an update is already known: straight to the confirm
                    self.pw.confirm = Some(ConfirmState {
                        prompt: if cfg!(windows) {
                            format!("install termie {} and restart?", u.version)
                        } else {
                            format!("open the termie {} release page?", u.version)
                        },
                        hint: if cfg!(windows) {
                            "enter: update \u{b7} esc: not now".to_string()
                        } else {
                            "enter: open \u{b7} esc: not now".to_string()
                        },
                        action: ConfirmAction::InstallUpdate,
                    });
                    self.redraw();
                } else {
                    // fresh manual check; the result event carries manual=true
                    self.show_notice("checking for updates\u{2026}");
                    update::mark_checked();
                    let proxy = self.proxy.clone();
                    update::check(move |found| {
                        let _ = proxy.send_event(UserEvent::UpdateCheckDone(found, true));
                    });
                }
            }
            PaletteAction::NewTab => self.new_tab(),
            PaletteAction::NewTabHere => {
                let cwd = self.focused_cwd();
                self.new_tab_cwd(cwd, None);
            }
            PaletteAction::DuplicateTab => {
                let cwd = self.focused_cwd();
                let shell = self.focused_shell();
                self.new_tab_cwd(cwd, shell);
            }
            PaletteAction::NewShell(s) => {
                let cwd = self.focused_cwd();
                self.new_tab_cwd(cwd, Some(s));
            }
            PaletteAction::NewWindow => {
                let cwd = self.focused_cwd();
                let shell = self.focused_shell();
                let (cols, rows) = self.content_pane_size();
                let pane = self.spawn_pane(cols, rows, cwd, shell, None);
                let point = self.pw.window.as_ref().and_then(|window| {
                    window
                        .inner_position()
                        .ok()
                        .map(|origin| PhysicalPosition::new(origin.x + 48, origin.y + 48))
                });
                if let Ok(pane) = pane {
                    let tab = tab_from_pane(pane);
                    if let Some(tab) = self.open_tab_window(event_loop, tab, point) {
                        self.insert_tab_here(tab, self.pw.tabs.len());
                        self.show_notice("couldn't open a new window");
                    }
                } else {
                    self.show_notice("couldn't open a new window");
                }
                self.redraw();
            }
            PaletteAction::AdminWindow => {
                #[cfg(windows)]
                {
                    let mut args = String::from("--admin-shell");
                    // a label with a quote would mangle the relaunch argv; fall
                    // back to the default shell rather than risk it
                    if let Some(s) = self.focused_shell().map(|s| s.label().to_string())
                        && !s.contains('"')
                    {
                        args.push_str(&format!(" --shell \"{s}\""));
                    }
                    if let Some(d) = self.focused_cwd()
                        && !d.contains('"')
                    {
                        // a trailing backslash (drive roots) would escape the
                        // closing quote under argv rules; double it
                        let d = if d.ends_with('\\') { format!("{d}\\") } else { d };
                        args.push_str(&format!(" --cwd \"{d}\""));
                    }
                    if !win::launch_elevated(args.trim()) {
                        self.show_notice("admin window cancelled");
                    }
                }
                #[cfg(not(windows))]
                {
                    let command = linux_admin_command(program_in_path("pkexec"), program_in_path("sudo"));
                    let launched = command.is_some_and(|command| {
                        let mut args = Vec::new();
                        if let Some(cwd) = self.focused_cwd() {
                            args.extend(["--cwd".to_string(), cwd]);
                        }
                        args.push("--admin-shell".to_string());
                        args.push("--".to_string());
                        args.extend(command.iter().map(|arg| (*arg).to_string()));
                        std::env::current_exe()
                            .and_then(|exe| std::process::Command::new(exe).args(args).spawn())
                            .is_ok()
                    });
                    if !launched {
                        self.show_notice("pkexec or sudo is unavailable");
                    }
                }
                self.redraw();
            }
            PaletteAction::SplitV => self.split_focused(Dir::Vertical),
            PaletteAction::SplitH => self.split_focused(Dir::Horizontal),
            PaletteAction::NextTab => {
                let n = self.pw.tabs.len();
                if n > 1 {
                    self.switch_tab((self.pw.active_tab + 1) % n);
                }
            }
            PaletteAction::PrevTab => {
                let n = self.pw.tabs.len();
                if n > 1 {
                    self.switch_tab((self.pw.active_tab + n - 1) % n);
                }
            }
            PaletteAction::TabSearch => self.open_tab_search(),
            PaletteAction::MoveTabLeft => self.shift_active_tab(-1),
            PaletteAction::MoveTabRight => self.shift_active_tab(1),
            PaletteAction::CloseTab => {
                let i = self.pw.active_tab;
                self.close_tab(i, event_loop);
            }
            PaletteAction::ReopenTab => self.reopen_closed_tab(),
            PaletteAction::Settings => self.open_settings(),
            PaletteAction::Plugins => self.open_market(),
            PaletteAction::PaneMode => self.set_pane_mode(true),
            PaletteAction::SendInput(ix) => {
                if let (Some(id), Some(text)) =
                    (self.active_focused_id(), self.send_inputs.get(ix))
                {
                    let bytes = text.clone().into_bytes();
                    self.send_paste_bytes(id, &bytes);
                }
            }
            PaletteAction::MarkMode => self.set_mark_mode(true),
            PaletteAction::SelectAll => self.select_all(),
            PaletteAction::JumpAttention => self.jump_attention(),
            PaletteAction::FontPicker => self.open_font_picker(),
            PaletteAction::ToggleOnTop => {
                self.pw.on_top = !self.pw.on_top;
                if let Some(w) = self.pw.window.as_ref() {
                    w.set_window_level(if self.pw.on_top {
                        WindowLevel::AlwaysOnTop
                    } else {
                        WindowLevel::Normal
                    });
                    win::set_window_above(w, self.pw.on_top);
                }
                self.show_notice(if self.pw.on_top { "always on top" } else { "normal stacking" });
                self.redraw();
            }
            PaletteAction::DefaultTerminal => {
                let enabling = !win::defterm_registered();
                let changed = if enabling {
                    win::register_defterm()
                } else {
                    win::unregister_defterm()
                };
                #[cfg(windows)]
                if changed && enabling {
                    // start serving immediately so it works without a relaunch
                    let proxy = self.proxy.clone();
                    defterm::serve_running(move |h| match h {
                        Some(h) => proxy.send_event(UserEvent::Handoff(h)).is_ok(),
                        None => true,
                    });
                }
                self.show_notice(match (changed, enabling) {
                    (true, true) => "termie is now the default terminal",
                    (true, false) => "termie is no longer the default terminal",
                    (false, _) => "could not update the default terminal",
                });
                self.redraw();
            }
            #[cfg(any(windows, target_os = "linux"))]
            PaletteAction::Quake => self.toggle_quake(),
            PaletteAction::Theme => {
                self.persisted.theme_auto = false;
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.cycle_theme();
                }
                if let Some(r) = self.main_pw().renderer.as_ref() {
                    self.persisted.theme = r.theme();
                }
                self.redraw();
                self.save_config();
            }
            PaletteAction::Quit => self.request_quit(event_loop),
            PaletteAction::ToggleSettings => self.toggle_settings(),
            PaletteAction::FontInc => self.nudge_font(1.0),
            PaletteAction::FontDec => self.nudge_font(-1.0),
            PaletteAction::FontReset => self.nudge_font(0.0),
            PaletteAction::ToggleBroadcast => {
                self.broadcast = !self.broadcast;
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_broadcast(self.broadcast);
                }
                self.redraw();
            }
            PaletteAction::OpenFind => self.open_find(),
            PaletteAction::OpenPalette => {
                self.palette = Some(PaletteState {
                    query: String::new(),
                    selected: 0,
                    mode: PaletteMode::Commands,
                });
                self.redraw();
            }
            PaletteAction::Copy => self.copy_selection(),
            PaletteAction::Paste => self.paste(),
            PaletteAction::CloseFocusedPane => self.close_focused_pane(event_loop),
            PaletteAction::ToggleZoom => self.toggle_zoom(),
            PaletteAction::ToggleFullscreen => self.toggle_fullscreen(),
            PaletteAction::RenameTab => {
                if let Some(tab) = self.pw.tabs.get(self.pw.active_tab) {
                    let buf = tab.title.clone().unwrap_or_default();
                    self.pw.rename = Some(RenameState { tab: self.pw.active_tab, buf });
                    self.redraw();
                }
            }
            PaletteAction::SelectTab(n) => self.switch_tab(n),
            PaletteAction::SetTabColor(i) => {
                if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) {
                    tab.color = (i > 0).then_some(i as u8);
                    self.sync_tabs();
                }
                self.redraw();
            }
        }
        true
    }

    /// merge the installed plugins (from disk) with the remote catalog into the
    /// overlay's row list. installed rows come first, then catalog-only entries
    fn market_rows(&self, catalog: &[plugin::market::Entry]) -> Vec<MarketRow> {
        let installed = discover_plugins();
        let mut rows: Vec<MarketRow> = installed
            .iter()
            .map(|d| {
                let cat = catalog.iter().find(|e| e.id == d.manifest.id);
                MarketRow {
                    id: d.manifest.id.clone(),
                    name: d.manifest.name.clone(),
                    version: d.manifest.version.clone(),
                    description: cat.map(|e| e.description.clone()).unwrap_or_default(),
                    permissions: d.manifest.permissions.clone(),
                    installed: true,
                    enabled: d.enabled,
                    url: cat.map(|e| e.url.clone()),
                }
            })
            .collect();
        // catalog entries that aren't installed yet
        for e in catalog {
            if !rows.iter().any(|r| r.id == e.id) {
                rows.push(MarketRow {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    version: e.version.clone(),
                    description: e.description.clone(),
                    permissions: e.permissions.clone(),
                    installed: false,
                    enabled: false,
                    url: Some(e.url.clone()),
                });
            }
        }
        rows
    }

    /// open the plugins marketplace overlay. lists installed plugins immediately;
    /// the remote catalog is fetched on a worker so the UI never blocks on the
    /// network, arriving later via UserEvent::Market
    fn open_market(&mut self) {
        let rows = self.market_rows(&[]);
        self.market = Some(MarketState {
            rows,
            selected: 0,
            status: "fetching catalog\u{2026}".to_string(),
            loading: true,
            fetch_failed: false,
        });
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let catalog = plugin::market::fetch_index();
            let _ = proxy.send_event(UserEvent::Market(catalog));
        });
        self.redraw();
    }

    /// persist one plugin's enabled state to plugins.cfg, preserving others
    fn set_plugin_enabled(&mut self, id: &str, enabled: bool) {
        let mut states = load_plugin_states();
        let st = states.entry(id.to_string()).or_default();
        st.enabled = enabled;
        save_plugin_states(&states);
    }

    /// restart the plugin host so enable/disable/install take effect. cheap:
    /// kills the running plugin processes and re-discovers from disk
    fn restart_plugins(&mut self) {
        self.kill_plugins();
        self.plugin_widgets.clear();
        self.rebuild_dock();
        self.start_plugins();
    }

    /// route a left-click in the open marketplace to a row, its action chip, or
    /// the close control — mouse parity with the keyboard navigation
    fn market_click(&mut self, cx: f32, cy: f32) {
        let Some(hit) = self.pw.renderer.as_ref().and_then(|r| r.market_hit_at(cx, cy)) else {
            return;
        };
        match hit {
            render::MarketHit::Close => {
                self.market = None;
                self.redraw();
            }
            render::MarketHit::Chip(i) => {
                if let Some(m) = self.market.as_mut() {
                    m.selected = i;
                }
                self.market_activate();
            }
            render::MarketHit::Card(i) => {
                if let Some(m) = self.market.as_mut() {
                    m.selected = i;
                }
                self.redraw();
            }
        }
    }

    /// act on the selected marketplace row (Enter or a chip click): toggle enable
    /// for an installed plugin, or install an available one
    fn market_activate(&mut self) {
        let Some(m) = self.market.as_ref() else {
            return;
        };
        let Some(row) = m.rows.get(m.selected).cloned() else {
            return;
        };
        if row.installed {
            let now = !row.enabled;
            self.set_plugin_enabled(&row.id, now);
            self.restart_plugins();
            if let Some(m) = self.market.as_mut() {
                if let Some(r) = m.rows.get_mut(m.selected) {
                    r.enabled = now;
                }
                m.status = format!("{} {}", row.id, if now { "enabled" } else { "disabled" });
            }
            self.redraw();
        } else if let Some(url) = row.url.clone() {
            // install from the catalog (download happens synchronously here; the
            // catalog is small and installs are user-initiated, rare events)
            if let Some(m) = self.market.as_mut() {
                m.status = format!("installing {}…", row.id);
            }
            self.redraw();
            let entry = plugin::market::Entry {
                id: row.id.clone(),
                name: row.name.clone(),
                version: row.version.clone(),
                description: row.description.clone(),
                url,
                permissions: row.permissions.clone(),
            };
            let Some(pdir) = plugins_dir() else {
                return;
            };
            let tmp = std::env::temp_dir();
            match plugin::market::install(&entry, &pdir, &tmp) {
                Ok(_) => {
                    self.set_plugin_enabled(&row.id, true);
                    self.restart_plugins();
                    self.refresh_market_rows();
                    if let Some(m) = self.market.as_mut() {
                        m.status = format!("installed {}", row.id);
                    }
                }
                Err(e) => {
                    if let Some(m) = self.market.as_mut() {
                        m.status = format!("install failed: {e}");
                    }
                }
            }
            self.redraw();
        }
    }

    /// remove the selected installed plugin (does nothing for catalog-only rows)
    fn market_remove(&mut self) {
        let Some(m) = self.market.as_ref() else {
            return;
        };
        let Some(row) = m.rows.get(m.selected).cloned() else {
            return;
        };
        if !row.installed {
            return;
        }
        let Some(pdir) = plugins_dir() else {
            return;
        };
        // disable first so a running process is stopped, then delete on disk
        self.set_plugin_enabled(&row.id, false);
        self.restart_plugins();
        match plugin::market::remove(&row.id, &pdir) {
            Ok(()) => {
                self.refresh_market_rows();
                if let Some(m) = self.market.as_mut() {
                    m.status = format!("removed {}", row.id);
                }
            }
            Err(e) => {
                if let Some(m) = self.market.as_mut() {
                    m.status = format!("remove failed: {e}");
                }
            }
        }
        self.redraw();
    }

    /// rebuild the overlay rows from disk, keeping the remote catalog info that
    /// rows already carry (so installed-from-catalog rows keep their url)
    fn refresh_market_rows(&mut self) {
        let catalog: Vec<plugin::market::Entry> = self
            .market
            .as_ref()
            .map(|m| {
                m.rows
                    .iter()
                    .filter_map(|r| {
                        r.url.as_ref().map(|u| plugin::market::Entry {
                            id: r.id.clone(),
                            name: r.name.clone(),
                            version: r.version.clone(),
                            description: r.description.clone(),
                            url: u.clone(),
                            permissions: r.permissions.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let rows = self.market_rows(&catalog);
        if let Some(m) = self.market.as_mut() {
            m.selected = m.selected.min(rows.len().saturating_sub(1));
            m.rows = rows;
        }
    }

    /// route a key to the open marketplace overlay; returns true if it consumed it
    fn market_input(&mut self, key: &Key) -> bool {
        if self.market.is_none() {
            return false;
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.market = None;
                self.redraw();
            }
            Key::Named(NamedKey::ArrowDown) => {
                if let Some(m) = self.market.as_mut() {
                    let n = m.rows.len().max(1);
                    m.selected = (m.selected + 1) % n;
                }
                self.redraw();
            }
            Key::Named(NamedKey::ArrowUp) => {
                if let Some(m) = self.market.as_mut() {
                    let n = m.rows.len().max(1);
                    m.selected = (m.selected + n - 1) % n;
                }
                self.redraw();
            }
            Key::Named(NamedKey::Enter) => self.market_activate(),
            Key::Character(s) if s.as_str() == "r" => self.market_remove(),
            _ => return false,
        }
        true
    }

    /// request closing a tab; one holding several panes confirms first so a
    /// stray Ctrl+W / middle-click can't drop multiple shells at once (session
    /// restore can't undo it — the closed tab leaves the saved layout)
    fn close_tab(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        // closing a tab shifts indices, so any in-flight drag-reorder is stale
        self.tab_drag = None;
        let panes = self
            .pw.tabs
            .get(idx)
            .and_then(|t| t.root.as_ref())
            .map(pane_count)
            .unwrap_or(0);
        if panes > 1 {
            self.pw.confirm = Some(ConfirmState {
                prompt: format!("close this tab? it has {panes} panes"),
                hint: "enter: close \u{b7} esc: cancel".to_string(),
                action: ConfirmAction::CloseTab { tab: idx },
            });
            self.redraw();
        } else {
            self.do_close_tab(idx, event_loop);
        }
    }

    /// stash a closing tab's launch spec on the reopen stack (bounded, oldest
    /// dropped) so Ctrl+Shift+T can bring it back
    fn remember_closed_tab(&mut self, idx: usize) {
        let Some(tab) = self.pw.tabs.get(idx) else {
            return;
        };
        let Some(pane) = tab.root.as_ref().and_then(|r| find_pane(r, tab.focused)) else {
            return;
        };
        let closed = ClosedTab {
            shell: pane.shell,
            cwd: cwd_path(pane.term.cwd.as_deref()),
            title: tab.title.clone(),
        };
        push_closed_tab(&mut self.closed_tabs, closed);
    }

    /// re-spawn the most recently closed tab from its stored launch spec, or
    /// show a notice when nothing has been closed
    fn reopen_closed_tab(&mut self) {
        let Some(spec) = self.closed_tabs.pop() else {
            self.show_notice("no closed tabs");
            return;
        };
        self.new_tab_cwd(spec.cwd, Some(spec.shell));
        if let Some(title) = spec.title
            && let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
        {
            tab.title = Some(title);
            self.sync_tabs();
        }
    }

    fn do_close_tab(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.pw.tabs.len() {
            return;
        }
        // a background shell exit can close a tab while a tab menu or a close
        // confirm is open; their captured indices go stale as slots shift left.
        // re-target the menu (or drop it with its tab), and dismiss a pending
        // close confirm whose prompt no longer matches reality
        match self.pw.pane_menu.as_ref().map(|m| m.target) {
            Some(MenuTarget::Tab(t)) | Some(MenuTarget::TabColor(t)) if t == idx => {
                self.pw.pane_menu = None;
            }
            Some(MenuTarget::Tab(t)) if t > idx => {
                if let Some(m) = self.pw.pane_menu.as_mut() {
                    m.target = MenuTarget::Tab(t - 1);
                }
            }
            Some(MenuTarget::TabColor(t)) if t > idx => {
                if let Some(m) = self.pw.pane_menu.as_mut() {
                    m.target = MenuTarget::TabColor(t - 1);
                }
            }
            _ => {}
        }
        if self
            .pw.confirm
            .as_ref()
            .is_some_and(|c| matches!(c.action, ConfirmAction::CloseTab { .. } | ConfirmAction::CloseOthers { .. }))
        {
            self.pw.confirm = None;
        }
        self.remember_closed_tab(idx);
        // capture *before* remove: after remove the active slot already points
        // at a surviving tab (or is out of range), so a post-remove capture
        // compares the new identity to itself and skips find recompute — leaving
        // matches from the killed pane live
        let before = self.focus_identity();
        let pane_ids: Vec<usize> = self.pw.tabs.iter().map(|t| t.focused).collect();
        let active = self.pw.active_tab;
        let mut tab = self.pw.tabs.remove(idx);
        if let Some(root) = tab.root.as_mut() {
            kill_all(root);
        }
        if self.pw.tabs.is_empty() {
            // emptying the MAIN window exits the app; emptying a torn-off window
            // (cur_sat set) just closes that window — satellite_event removes it
            // after the swap-back
            if self.cur_sat.is_none() {
                self.kill_pool();
                win::clipboard_shutdown();
                self.release_window_resources();
                event_loop.exit();
            }
            return;
        }
        // same arithmetic the unit tests drive: active index after the slot dies
        if let Some((new_active, _)) = focus_identity_after_tab_close(&pane_ids, active, idx) {
            self.pw.active_tab = new_active;
        } else {
            self.pw.active_tab = self.pw.active_tab.min(self.pw.tabs.len() - 1);
        }
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx >= self.pw.tabs.len() || idx == self.pw.active_tab {
            return;
        }
        let before = self.focus_identity();
        self.pw.active_tab = idx;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
    }

    /// close every tab except `keep`, confirming once (like close_tab does per
    /// tab) when any of the doomed tabs holds more than one pane
    fn close_others(&mut self, keep: usize) {
        if self.pw.tabs.len() < 2 || keep >= self.pw.tabs.len() {
            return;
        }
        let multi = self
            .pw.tabs
            .iter()
            .enumerate()
            .any(|(i, t)| i != keep && t.root.as_ref().map(pane_count).unwrap_or(0) > 1);
        if multi {
            let n = self.pw.tabs.len() - 1;
            let noun = if n == 1 { "tab" } else { "tabs" };
            self.pw.confirm = Some(ConfirmState {
                prompt: format!("close {n} other {noun}?"),
                hint: "enter: close \u{b7} esc: cancel".to_string(),
                action: ConfirmAction::CloseOthers { keep },
            });
            self.redraw();
        } else {
            self.do_close_others(keep);
        }
    }

    fn do_close_others(&mut self, keep: usize) {
        if keep >= self.pw.tabs.len() {
            return;
        }
        self.tab_drag = None;
        // remember the doomed tabs in strip order before they're killed
        for i in 0..self.pw.tabs.len() {
            if i != keep {
                self.remember_closed_tab(i);
            }
        }
        let before = self.focus_identity();
        for (i, tab) in self.pw.tabs.iter_mut().enumerate() {
            if i != keep
                && let Some(root) = tab.root.as_mut()
            {
                kill_all(root);
            }
        }
        let kept = self.pw.tabs.remove(keep);
        self.pw.tabs.clear();
        self.pw.tabs.push(kept);
        self.pw.active_tab = 0;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
    }

    /// move the tab at `from` so it sits at `to` (drag reorder / keyboard nudge);
    /// the active tab stays the same tab through the shuffle
    fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.pw.tabs.len();
        if from == to || from >= n || to >= n {
            return;
        }
        let tab = self.pw.tabs.remove(from);
        self.pw.tabs.insert(to, tab);
        self.pw.active_tab = active_after_move(self.pw.active_tab, from, to);
        self.sync_tabs();
        self.redraw();
    }

    fn remove_tab_for_transfer(&mut self, index: usize, allow_empty: bool) -> Option<Tab> {
        if index >= self.pw.tabs.len() || (!allow_empty && self.pw.tabs.len() == 1) {
            return None;
        }
        let before = self.focus_identity();
        let tab = self.pw.tabs.remove(index);
        if self.pw.tabs.is_empty() {
            self.pw.active_tab = 0;
        } else {
            self.pw.active_tab = if index < self.pw.active_tab {
                self.pw.active_tab - 1
            } else {
                self.pw.active_tab.min(self.pw.tabs.len() - 1)
            };
            self.relayout_all();
            self.sync_tabs();
            self.after_focus_context_change(before);
        }
        Some(tab)
    }

    fn take_tab_from_window(&mut self, source: WindowId, index: usize, allow_main_empty: bool) -> Option<Tab> {
        let main = self.main_pw().window.as_ref().map(|w| w.id());
        let allow_empty = main != Some(source) || allow_main_empty;
        if self.pw.window.as_ref().map(|w| w.id()) == Some(source) {
            return self.remove_tab_for_transfer(index, allow_empty);
        }
        let slot = self.satellite_for(source)?;
        let mut tab = None;
        self.with_window(slot, |app| {
            tab = app.remove_tab_for_transfer(index, allow_empty);
        });
        tab
    }

    fn insert_tab_here(&mut self, tab: Tab, index: usize) {
        let before = self.focus_identity();
        let index = index.min(self.pw.tabs.len());
        self.pw.tabs.insert(index, tab);
        self.pw.active_tab = index;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
        self.redraw();
    }

    fn insert_tab_into_window(&mut self, target: WindowId, tab: Tab, index: usize) -> Option<Tab> {
        if self.pw.window.as_ref().map(|w| w.id()) == Some(target) {
            self.insert_tab_here(tab, index);
            return None;
        }
        let Some(slot) = self.satellite_for(target) else {
            return Some(tab);
        };
        let mut tab = Some(tab);
        self.with_window(slot, |app| {
            app.insert_tab_here(tab.take().expect("tab transferred once"), index);
        });
        None
    }

    fn window_at_screen(&self, point: PhysicalPosition<i32>) -> Option<(WindowId, usize)> {
        let hit = |pw: &PaneWindow| {
            let window = pw.window.as_ref()?;
            let origin = window.inner_position().ok()?;
            let size = window.inner_size();
            let x = point.x - origin.x;
            let y = point.y - origin.y;
            if x < 0 || y < 0 || x >= size.width as i32 || y >= size.height as i32 {
                return None;
            }
            let index = pw
                .renderer
                .as_ref()
                .and_then(|r| match r.hit_test(x as f32, y as f32) {
                    Hit::Button(Hot::Tab(i) | Hot::TabClose(i)) => Some(i),
                    _ => None,
                })
                .unwrap_or(pw.tabs.len());
            Some((window.id(), index))
        };
        self.satellites.iter().find_map(hit).or_else(|| hit(&self.pw))
    }

    fn window_tab_count(&self, id: WindowId) -> Option<usize> {
        self.pane_window_for(id).map(|pw| pw.tabs.len())
    }

    fn pane_count_for_window(&self, id: WindowId) -> Option<usize> {
        self.pane_window_for(id).map(|pw| {
            pw.tabs
                .iter()
                .filter_map(|tab| tab.root.as_ref())
                .map(pane_count)
                .sum()
        })
    }

    fn pane_window_for(&self, id: WindowId) -> Option<&PaneWindow> {
        self.satellites
            .iter()
            .chain(std::iter::once(&self.pw))
            .find(|pw| pw.window.as_ref().map(|window| window.id()) == Some(id))
    }

    #[cfg(target_os = "linux")]
    fn pane_window_for_mut(&mut self, id: WindowId) -> Option<&mut PaneWindow> {
        self.satellites
            .iter_mut()
            .chain(std::iter::once(&mut self.pw))
            .find(|pw| pw.window.as_ref().map(|window| window.id()) == Some(id))
    }

    #[cfg(target_os = "linux")]
    fn begin_kwin_drag_probe(&mut self) {
        if self.kwin_drag_bridge.is_none() {
            return;
        }
        self.cancel_kwin_drag_probe();
        self.kwin_drag_generation = self.kwin_drag_generation.wrapping_add(1);
        let generation = self.kwin_drag_generation;
        let mut tagged = Vec::new();
        let tag = |pw: &mut PaneWindow, tagged: &mut Vec<(WindowId, String)>| {
            if let Some(window) = pw.window.as_ref() {
                let title = window.title();
                window.set_title(&kwin_drag_title(&title, tagged.len()));
                tagged.push((window.id(), title));
            }
        };
        tag(&mut self.pw, &mut tagged);
        for pw in &mut self.satellites {
            tag(pw, &mut tagged);
        }
        self.kwin_drag_probe = Some(KwinDragProbe {
            generation,
            tagged,
            started: Instant::now(),
            script: None,
        });
    }

    #[cfg(target_os = "linux")]
    fn poll_kwin_drag_probe(&mut self) {
        let Some(probe) = self.kwin_drag_probe.as_ref() else {
            return;
        };
        if probe.started.elapsed() >= Duration::from_secs(1) {
            self.cancel_kwin_drag_probe();
            return;
        }
        if probe.script.is_some() || probe.started.elapsed() < Duration::from_millis(20) {
            return;
        }
        let generation = probe.generation;
        let script = self.kwin_drag_bridge.as_ref().and_then(|bridge| bridge.request(generation));
        if let Some(script) = script {
            if let Some(probe) = self.kwin_drag_probe.as_mut() {
                probe.script = Some(script);
            }
        } else {
            self.cancel_kwin_drag_probe();
        }
    }

    #[cfg(target_os = "linux")]
    fn accept_kwin_drag_geometry(&mut self, snapshot: win::KwinDragSnapshot) {
        let Some(probe) = self.kwin_drag_probe.take() else {
            return;
        };
        if let Some(script) = probe.script.as_deref() {
            win::unload_kwin_script(script);
        }
        for (index, (window, title)) in probe.tagged.iter().enumerate() {
            if let Some(window) = self
                .pane_window_for_mut(*window)
                .and_then(|pw| pw.window.as_ref())
                .filter(|window| window.title() == kwin_drag_title(title, index))
            {
                window.set_title(title);
            }
        }
        if snapshot.generation != probe.generation
            || (self.tab_drag.is_none() && self.pane_drag.is_none())
        {
            return;
        }
        self.kwin_drag_geometry = snapshot
            .windows
            .into_iter()
            .filter_map(|(index, x, y, w, h)| {
                probe.tagged.get(index).map(|(window, _)| KwinWindowGeometry {
                    window: *window,
                    x,
                    y,
                    w,
                    h,
                })
            })
            .collect();
    }

    #[cfg(target_os = "linux")]
    fn cancel_kwin_drag_probe(&mut self) {
        let Some(probe) = self.kwin_drag_probe.take() else {
            self.kwin_drag_geometry.clear();
            return;
        };
        if let Some(script) = probe.script.as_deref() {
            win::unload_kwin_script(script);
        }
        for (index, (window, title)) in probe.tagged.into_iter().enumerate() {
            if let Some(window) = self
                .pane_window_for_mut(window)
                .and_then(|pw| pw.window.as_ref())
                .filter(|window| window.title() == kwin_drag_title(&title, index))
            {
                window.set_title(&title);
            }
        }
        self.kwin_drag_geometry.clear();
    }

    fn reposition_window(&self, id: WindowId, point: PhysicalPosition<i32>, grab: PhysicalPosition<f64>) {
        let window = self
            .pane_window_for(id)
            .and_then(|pw| pw.window.as_ref());
        if let Some(window) = window {
            window.set_outer_position(drag_window_origin(point, grab));
            constrain_window_to_monitor(window);
        }
    }

    fn focus_window(&self, id: WindowId) {
        if let Some(window) = self.pane_window_for(id).and_then(|pw| pw.window.as_ref()) {
            window.focus_window();
        }
    }

    fn cleanup_empty_windows(&mut self) {
        if self.cur_sat.is_some() {
            return;
        }
        if self.pw.tabs.is_empty()
            && let Some(index) = self.satellites.iter().position(|window| !window.tabs.is_empty())
        {
            std::mem::swap(&mut self.pw, &mut self.satellites[index]);
        }
        self.satellites.retain(|window| !window.tabs.is_empty());
    }

    fn create_satellite_window(
        &self,
        event_loop: &ActiveEventLoop,
        point: Option<PhysicalPosition<i32>>,
    ) -> Result<PaneWindow> {
        let (rgba, width, height) = win::app_icon();
        let icon = winit::window::Icon::from_rgba(rgba, width, height).ok();
        let attrs = satellite_window_attrs(icon);
        let window = Arc::new(event_loop.create_window(attrs)?);
        if let Some(point) = point {
            window.set_outer_position(point);
        }
        constrain_window_to_monitor(&window);
        window.set_ime_allowed(true);
        #[cfg(windows)]
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(handle) = handle.as_raw()
        {
            win::apply_window_effects(handle.hwnd.get());
            if self.persisted.acrylic {
                win::apply_backdrop(handle.hwnd.get(), true);
            }
        }
        #[cfg(not(windows))]
        if self.persisted.acrylic {
            window.set_blur(true);
        }
        let mut renderer = Renderer::new(
            window.clone(),
            event_loop.owned_display_handle(),
            CONTENT_PT,
            CHROME_PT,
            self.config.backend,
            true,
        )?;
        self.configure_renderer(&mut renderer);
        let mut pw = pane_window(Some(window.clone()), Some(renderer), Vec::new());
        pw.a11y = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));
        Ok(pw)
    }

    fn open_tab_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        tab: Tab,
        point: Option<PhysicalPosition<i32>>,
    ) -> Option<Tab> {
        let mut pw = match self.create_satellite_window(event_loop, point) {
            Ok(pw) => pw,
            Err(_) => return Some(tab),
        };
        pw.tabs.push(tab);
        self.satellites.push(pw);
        let slot = self.satellites.len() - 1;
        self.with_window(slot, |app| {
            if let Some(renderer) = app.pw.renderer.as_mut() {
                renderer.begin_reveal();
            }
            app.relayout_all();
            app.sync_tabs();
            app.paint();
        });
        if let Some(window) = self.satellites[slot].window.as_ref() {
            window.set_visible(true);
            window.focus_window();
        }
        None
    }

    fn open_launch_window(&mut self, event_loop: &ActiveEventLoop, request: instance::LaunchRequest) {
        let point = self.main_pw().window.as_ref().and_then(|window| {
            window
                .inner_position()
                .ok()
                .map(|origin| PhysicalPosition::new(origin.x + 48, origin.y + 48))
        });
        let pw = match self.create_satellite_window(event_loop, point) {
            Ok(pw) => pw,
            Err(error) => {
                log::error!("forwarded launch window failed: {error:#}");
                self.show_notice("couldn't open the requested window");
                return;
            }
        };
        let mut cli = parse_args(request.args.into_iter());
        let base = request.process_cwd.as_deref();
        if let Some(cwd) = cli.cwd.as_deref() {
            cli.cwd = Some(resolve_launch_path(cwd, base));
        }
        for tab in &mut cli.tabs {
            resolve_layout_dirs(&mut tab.root, base);
        }

        self.satellites.push(pw);
        let slot = self.satellites.len() - 1;
        self.with_window(slot, |app| {
            if cli.tabs.is_empty() {
                let cwd = if cli.is_bare() {
                    request.launch_cwd
                } else if cli.command.is_some() {
                    cli.cwd.or(request.process_cwd)
                } else {
                    cli.cwd
                };
                let shell = cli.shell.as_deref().map(ShellKind::from_label);
                let (cols, rows) = app.content_pane_size();
                if let Ok(pane) = app.spawn_pane(cols, rows, cwd, shell, cli.command.as_deref()) {
                    app.install_first_tab(pane);
                }
            } else {
                app.restore_session(session::SessionFile {
                    active_tab: 0,
                    tabs: cli.tabs,
                    window: None,
                });
            }
            if let Some(renderer) = app.pw.renderer.as_mut() {
                renderer.begin_reveal();
            }
            app.paint();
        });
        if self.satellites[slot].tabs.is_empty() {
            self.satellites.remove(slot);
            self.show_notice("couldn't start the requested shell");
            return;
        }
        if let Some(window) = self.satellites[slot].window.as_ref() {
            window.set_visible(true);
            window.focus_window();
        }
    }

    fn finish_tab_drag(&mut self, event_loop: &ActiveEventLoop) {
        let Some(drag) = self.tab_drag.take() else {
            return;
        };
        #[cfg(target_os = "linux")]
        self.cancel_kwin_drag_probe();
        self.show_tab_drop(None);
        self.show_drag_preview(None);
        self.reset_drag_cursors();
        if !drag.left_strip {
            return;
        }
        let target = drag
            .target
            .or_else(|| {
                if drag.left_window {
                    None
                } else {
                    drag.screen.and_then(|point| self.window_at_screen(point))
                }
            })
            .filter(|(window, _)| self.pane_window_for(*window).is_some());
        if let Some((target, _)) = target
            && target == drag.source
        {
            return;
        }
        if target.is_none()
            && self.window_tab_count(drag.source) == Some(1)
        {
            if let Some(point) = drag.screen {
                self.reposition_window(drag.source, point, drag.start);
            }
            return;
        }
        let Some(tab) = self.take_tab_from_window(drag.source, drag.index, target.is_some()) else {
            return;
        };
        if let Some((target, index)) = target {
            if let Some(tab) = self.insert_tab_into_window(target, tab, index) {
                let _ = self.insert_tab_into_window(drag.source, tab, drag.index);
                self.show_notice("the destination window closed during the drag");
            } else {
                self.focus_window(target);
            }
        } else if let Some(tab) = self.open_tab_window(
            event_loop,
            tab,
            drag.screen.map(|point| drag_window_origin(point, drag.start)),
        ) {
            let _ = self.insert_tab_into_window(drag.source, tab, drag.index);
            self.show_notice("couldn't open a new window");
        }
        self.cleanup_empty_windows();
    }

    fn remove_pane_for_transfer(&mut self, tab_index: usize, pane_id: usize, allow_empty: bool) -> Option<Pane> {
        let pane_total = self
            .pw
            .tabs
            .get(tab_index)
            .and_then(|tab| tab.root.as_ref())
            .map(pane_count)?;
        if pane_total == 1 && self.pw.tabs.len() == 1 && !allow_empty {
            return None;
        }
        let before = self.focus_identity();
        let tab = self.pw.tabs.get_mut(tab_index)?;
        let root = tab.root.take()?;
        let mut pane = None;
        tab.root = extract_pane(root, pane_id, &mut pane);
        pane.as_ref()?;
        if let Some(root) = tab.root.as_ref() {
            if tab.focused == pane_id {
                tab.focused = first_leaf(root);
            }
            if tab.zoom == Some(pane_id) {
                tab.zoom = None;
            }
        } else {
            self.pw.tabs.remove(tab_index);
            if self.pw.tabs.is_empty() {
                self.pw.active_tab = 0;
            } else {
                self.pw.active_tab = self.pw.active_tab.min(self.pw.tabs.len() - 1);
            }
        }
        if !self.pw.tabs.is_empty() {
            self.relayout_all();
            self.sync_tabs();
            self.after_focus_context_change(before);
        }
        pane
    }

    fn take_pane_from_window(
        &mut self,
        source: WindowId,
        tab: usize,
        pane: usize,
        allow_main_empty: bool,
    ) -> Option<Pane> {
        let main = self.main_pw().window.as_ref().map(|window| window.id());
        let allow_empty = main != Some(source) || allow_main_empty;
        if self.pw.window.as_ref().map(|window| window.id()) == Some(source) {
            return self.remove_pane_for_transfer(tab, pane, allow_empty);
        }
        let slot = self.satellite_for(source)?;
        let mut moved = None;
        self.with_window(slot, |app| {
            moved = app.remove_pane_for_transfer(tab, pane, allow_empty);
        });
        moved
    }

    fn insert_pane_here(&mut self, tab_index: usize, target: usize, pane: Pane, side: PaneDropSide) -> Option<Pane> {
        let before = self.focus_identity();
        let moved_id = pane.id;
        let Some(tab) = self.pw.tabs.get_mut(tab_index) else {
            return Some(pane);
        };
        let Some(root) = tab.root.take() else {
            return Some(pane);
        };
        let mut pane = Some(pane);
        tab.root = Some(insert_pane(root, target, &mut pane, side));
        if pane.is_some() {
            return pane;
        }
        tab.focused = moved_id;
        tab.zoom = None;
        self.pw.active_tab = tab_index;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
        self.redraw();
        None
    }

    fn insert_pane_into_window(&mut self, target: PaneDropTarget, pane: Pane) -> Option<Pane> {
        if self.pw.window.as_ref().map(|window| window.id()) == Some(target.window) {
            return self.insert_pane_here(target.tab, target.pane, pane, target.side);
        }
        let Some(slot) = self.satellite_for(target.window) else {
            return Some(pane);
        };
        let mut pane = Some(pane);
        self.with_window(slot, |app| {
            pane = app.insert_pane_here(
                target.tab,
                target.pane,
                pane.take().expect("pane transferred once"),
                target.side,
            );
        });
        pane
    }

    fn finish_pane_drag(&mut self, event_loop: &ActiveEventLoop) {
        let Some(drag) = self.pane_drag.take() else {
            return;
        };
        #[cfg(target_os = "linux")]
        self.cancel_kwin_drag_probe();
        self.show_pane_drop(None);
        self.show_tab_drop(None);
        self.show_drag_preview(None);
        self.reset_drag_cursors();
        if !drag.moved {
            return;
        }
        let target = drag
            .target
            .or_else(|| {
                if drag.left_window {
                    None
                } else {
                    drag.screen.and_then(|point| self.pane_drop_at_screen(point))
                }
            })
            .filter(|target| match target {
                PaneDropDestination::Dock(target) => self.pane_window_for(target.window).is_some(),
                PaneDropDestination::Tab(window, _) => self.pane_window_for(*window).is_some(),
            });
        if let Some(PaneDropDestination::Dock(target)) = target
            && target.window == drag.source_window
            && target.tab == drag.source_tab
            && target.pane == drag.pane
        {
            return;
        }
        if let Some(PaneDropDestination::Tab(window, _)) = target
            && window == drag.source_window
            && self
                .pane_window_for(window)
                .and_then(|pw| pw.tabs.get(drag.source_tab))
                .and_then(|tab| tab.root.as_ref())
                .map(pane_count)
                == Some(1)
        {
            return;
        }
        if target.is_none()
            && self.pane_count_for_window(drag.source_window) == Some(1)
        {
            if let Some(point) = drag.screen {
                self.reposition_window(drag.source_window, point, drag.start);
            }
            return;
        }
        let Some(pane) = self.take_pane_from_window(
            drag.source_window,
            drag.source_tab,
            drag.pane,
            target.is_some(),
        ) else {
            return;
        };
        if let Some(target) = target {
            match target {
                PaneDropDestination::Dock(target) => {
                    if let Some(pane) = self.insert_pane_into_window(target, pane) {
                        let fallback = tab_from_pane(pane);
                        let _ = self.insert_tab_into_window(
                            drag.source_window,
                            fallback,
                            drag.source_tab,
                        );
                    } else {
                        self.focus_window(target.window);
                    }
                }
                PaneDropDestination::Tab(window, index) => {
                    let tab = tab_from_pane(pane);
                    if let Some(tab) = self.insert_tab_into_window(window, tab, index) {
                        let _ = self.insert_tab_into_window(
                            drag.source_window,
                            tab,
                            drag.source_tab,
                        );
                    } else {
                        self.focus_window(window);
                    }
                }
            }
        } else {
            let tab = tab_from_pane(pane);
            if let Some(tab) = self.open_tab_window(
                event_loop,
                tab,
                drag.screen.map(|point| drag_window_origin(point, drag.start)),
            ) {
                let _ = self.insert_tab_into_window(drag.source_window, tab, drag.source_tab);
                self.show_notice("couldn't open a new window");
            }
        }
        self.cleanup_empty_windows();
    }

    fn shift_active_tab(&mut self, delta: i32) {
        let to = self.pw.active_tab as i32 + delta;
        if to >= 0 && (to as usize) < self.pw.tabs.len() {
            self.move_tab(self.pw.active_tab, to as usize);
        }
    }

    fn split_focused(&mut self, dir: Dir) {
        let Some(focused) = self.active_focused_id() else {
            return;
        };
        let cwd = self.focused_cwd();
        // splitting a wsl/cmd/custom pane keeps that shell, like duplicate-tab
        let shell = self.focused_shell().filter(|&s| s != self.config.shell);
        // a known cwd or an inherited non-default shell means spawn fresh
        // (pool shells live in home and run the default shell); else prefer a
        // ready pool shell (instant — relayout resizes it to the split rect,
        // safe since it's past startup), spawning fresh at the post-split
        // rect only as a fallback so pwsh is never resized mid-startup
        let pane = if cwd.is_none()
            && shell.is_none()
            && let Some(i) = self.pool.iter().position(|p| p.ready)
        {
            self.pool.remove(i)
        } else {
            let foc_rect = self
                .pw.layout_cache
                .iter()
                .find(|(i, _)| *i == focused)
                .map(|(_, r)| *r);
            let (cols, rows) = match (self.pw.renderer.as_ref(), foc_rect) {
                (Some(r), Some((x, y, w, h))) => {
                    let b = match dir {
                        Dir::Vertical => {
                            let half = (w / 2.0).floor();
                            (x + half, y, w - half, h)
                        }
                        Dir::Horizontal => {
                            let half = (h / 2.0).floor();
                            (x, y + half, w, h - half)
                        }
                    };
                    let (_, _, c, rr) = r.pane_metrics(b);
                    (c, rr)
                }
                _ => self.content_pane_size(),
            };
            let Ok(p) = self.spawn_pane(cols, rows, cwd, shell, None) else {
                return;
            };
            p
        };
        let new_id = pane.id;
        let before = self.focus_identity();
        let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) else {
            return;
        };
        let Some(root) = tab.root.take() else {
            return;
        };
        let target = tab.focused;
        let mut new = Some(pane);
        let root = split_pane(root, target, dir, &mut new);
        tab.root = Some(root);
        if let Some(mut leftover) = new.take() {
            // target not found (shouldn't happen) — clean up the spawned pane
            leftover.pty.kill();
        } else {
            tab.focused = new_id;
        }
        // ease the new pane's accent border in
        self.focus_anim = Some(Instant::now());
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
        self.warm_pool();
    }

    fn close_focused_pane(&mut self, event_loop: &ActiveEventLoop) {
        let before = self.focus_identity();
        let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) else {
            return;
        };
        let target = tab.focused;
        let Some(root) = tab.root.take() else {
            return;
        };
        match close_pane(root, target) {
            Some(node) => {
                tab.focused = first_leaf(&node);
                tab.root = Some(node);
                // ease the surviving pane's accent border in
                self.focus_anim = Some(Instant::now());
                self.relayout_all();
                self.sync_tabs();
                self.after_focus_context_change(before);
            }
            None => {
                // last pane in the tab closed → close the tab
                let idx = self.pw.active_tab;
                self.close_tab(idx, event_loop);
            }
        }
    }

    /// a file landed on this window (self.pw is the receiving window: the
    /// satellite path swaps its state in before dispatching here). winit
    /// reports no drop position, but the cursor still sits exactly where the
    /// user let go — ask the OS. a drop on the tab strip opens a tab at the
    /// folder (wt-style); a drop on the content lands in the pane under the
    /// pointer, like right-click, and types the quoted path at its prompt
    fn on_dropped_file(&mut self, path: &std::path::Path) {
        let at = Some((self.pw.cursor.x as f32, self.pw.cursor.y as f32));
        let hit = at.zip(self.pw.renderer.as_ref()).map(|((x, y), r)| r.hit_test(x, y));
        if matches!(
            hit,
            Some(
                Hit::TitleBar
                    | Hit::Button(Hot::Tab(_) | Hot::TabClose(_) | Hot::NewTab | Hot::NewTabMenu)
            )
        ) {
            let dir = if path.is_dir() { Some(path) } else { path.parent() };
            if let Some(d) = dir {
                self.new_tab_cwd(Some(d.to_string_lossy().into_owned()), None);
            }
            return;
        }
        if matches!(hit, Some(Hit::Content))
            && let Some((x, y)) = at
        {
            self.focus_pane_at(x, y);
        }
        if let Some(id) = self.active_focused_id() {
            // quote the typed path if it has spaces so the shell treats it
            // as a single argument
            let s = path.to_string_lossy();
            let text = if s.contains(' ') { format!("\"{s}\" ") } else { format!("{s} ") };
            if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
                && let Some(root) = tab.root.as_mut()
                && let Some(p) = find_pane_mut(root, id)
            {
                p.pty.write(text.as_bytes());
            }
        }
    }

    fn focus_pane_at(&mut self, x: f32, y: f32) {
        let hit = self
            .pw.layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(id, _)| *id);
        let before = self.focus_identity();
        let changed = if let (Some(id), Some(tab)) = (hit, self.pw.tabs.get_mut(self.pw.active_tab)) {
            if tab.focused != id {
                tab.focused = id;
                true
            } else {
                false
            }
        } else {
            false
        };
        if changed {
            // tab label + git track the focused pane; ease the accent border in
            self.focus_anim = Some(Instant::now());
            self.sync_tabs();
            self.after_focus_context_change(before);
            if let (Some(id), false) = (hit, self.plugins.is_empty()) {
                self.plugins_broadcast(&plugin::HostEvent::FocusChanged { pane: id as u64 });
            }
        }
    }

    fn button_action(&mut self, event_loop: &ActiveEventLoop, hot: Hot) {
        match hot {
            Hot::Minimize => {
                if let Some(w) = &self.pw.window {
                    w.set_minimized(true);
                }
            }
            Hot::Maximize => {
                self.pw.maximized = !self.pw.maximized;
                if let Some(w) = &self.pw.window {
                    w.set_maximized(self.pw.maximized);
                }
            }
            Hot::Close => {
                if self.config.close_action == CloseAction::Minimize {
                    if let Some(w) = &self.pw.window {
                        w.set_minimized(true);
                    }
                } else if self.cur_sat.is_some() {
                    // a torn-off window's close button kills only that window's
                    // shells and empties it; satellite_event removes the now-empty
                    // window after the swap-back (never exits the app). more
                    // than one pane gets the same count-gated confirm as quit
                    let panes = self.window_pane_count();
                    if panes > 1 {
                        self.pw.confirm = Some(ConfirmState {
                            prompt: format!("close this window's {panes} panes?"),
                            hint: "enter: close \u{b7} esc: cancel".to_string(),
                            action: ConfirmAction::CloseWindow,
                        });
                        self.redraw();
                        return;
                    }
                    for tab in &mut self.pw.tabs {
                        if let Some(root) = tab.root.as_mut() {
                            kill_all(root);
                        }
                    }
                    self.pw.tabs.clear();
                } else {
                    self.request_quit(event_loop);
                }
            }
            Hot::Gear => self.toggle_settings(),
            Hot::PanelClose => self.close_settings(),
            Hot::ThemeSet(id) => {
                // picking a concrete theme turns follow-the-OS off
                self.persisted.theme_auto = false;
                self.persisted.theme = id;
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_theme(id);
                }
                self.redraw();
            }
            Hot::ThemeAuto => {
                self.persisted.theme_auto = true;
                #[cfg(not(windows))]
                {
                    self.ensure_theme_watcher();
                }
                self.apply_os_theme();
            }
            Hot::SplitV => self.split_focused(Dir::Vertical),
            Hot::SplitH => self.split_focused(Dir::Horizontal),
            Hot::PaneMode => self.set_pane_mode(!self.pw.pane_mode),
            Hot::NewTab => self.new_tab(),
            Hot::NewTabMenu => self.open_newtab_menu(),
            Hot::Tab(i) => self.switch_tab(i),
            Hot::TabClose(i) => self.close_tab(i, event_loop),
            Hot::FontDec | Hot::FontInc => {
                let d = if hot == Hot::FontInc { 1.0 } else { -1.0 };
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_content_pt(r.content_pt() + d);
                }
                self.relayout_all();
                self.redraw();
            }
            Hot::FontCycle => {
                // the settings FONT control opens the searchable picker over
                // every installed monospace family (was: cycle a hardcoded few)
                self.open_font_picker();
            }
            Hot::PadDec | Hot::PadInc => {
                let d = if hot == Hot::PadInc { 2.0 } else { -2.0 };
                let changed = self
                    .pw.renderer
                    .as_mut()
                    .map(|r| r.set_pane_pad(d))
                    .unwrap_or(false);
                if changed {
                    self.relayout_all();
                }
                self.redraw();
            }
            Hot::OpacityDec | Hot::OpacityInc => {
                let d = if hot == Hot::OpacityInc { 5 } else { -5 };
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.nudge_opacity(d);
                }
                self.redraw();
            }
            Hot::LineHeightDec | Hot::LineHeightInc => {
                let d = if hot == Hot::LineHeightInc { 0.04 } else { -0.04 };
                if let Some(r) = self.pw.renderer.as_mut() {
                    let lh = r.line_height();
                    r.set_line_height(lh + d);
                }
                // cell height changed, so every pane's grid needs re-fitting
                self.relayout_all();
                self.redraw();
            }
            Hot::BoldBright => {
                if let Some(r) = self.pw.renderer.as_mut() {
                    let on = !r.bold_as_bright();
                    r.set_bold_as_bright(on);
                }
                self.redraw();
            }
            Hot::Mica => {
                self.persisted.acrylic = !self.persisted.acrylic;
                let on = self.persisted.acrylic;
                for pw in std::iter::once(&self.pw).chain(self.satellites.iter()) {
                    if let Some(w) = pw.window.as_ref() {
                        #[cfg(not(windows))]
                        w.set_blur(on);
                        #[cfg(windows)]
                        if let Ok(handle) = w.window_handle()
                        && let RawWindowHandle::Win32(h) = handle.as_raw()
                        {
                            win::apply_backdrop(h.hwnd.get(), on);
                        }
                    }
                }
                self.redraw();
            }
            Hot::CursorCycle => {
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.cycle_cursor();
                }
                self.redraw();
            }
            Hot::CursorBlink => {
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.toggle_cursor_blink();
                }
                self.redraw();
            }
            Hot::ScrollbackDec | Hot::ScrollbackInc => {
                let step = if hot == Hot::ScrollbackInc { 1000 } else { -1000 };
                let next = (self.config.scrollback as i64 + step).clamp(0, 100_000);
                self.config.scrollback = next as usize;
                self.apply_scrollback();
                self.redraw();
            }
            Hot::CopyOnSelect => {
                self.config.copy_on_select = !self.config.copy_on_select;
                self.redraw();
            }
            Hot::LoadProfile => {
                self.config.load_profile = !self.config.load_profile;
                self.redraw();
            }
            Hot::ShellCycle => {
                self.config.shell = self.config.shell.next();
                self.redraw();
            }
            Hot::CloseActionCycle => {
                self.config.close_action = self.config.close_action.next();
                self.redraw();
            }
            Hot::BackendCycle => {
                // applies on next launch (persisted below); backend can't swap live
                self.config.backend = self.config.backend.next();
                self.redraw();
            }
            Hot::OpenPlugins => {
                self.close_settings();
                self.open_market();
            }
            Hot::PluginToggle(i) => {
                // flip the i-th installed plugin's enabled state in place; the
                // settings list order matches discover_plugins (set on open)
                let discovered = discover_plugins();
                if let Some(d) = discovered.get(i) {
                    let id = d.manifest.id.clone();
                    let now = !d.enabled;
                    self.set_plugin_enabled(&id, now);
                    self.restart_plugins();
                    self.refresh_settings_plugins();
                    self.redraw();
                }
            }
        }
        // persist whenever a setting changed
        if is_settings_hot(hot) {
            self.save_config();
        }
    }

    /// write every setting (App-owned + renderer-owned) to the config file
    /// the theme to paint with right now: the configured one, or under
    /// `theme=auto` whichever of theme_dark/theme_light matches the OS mode
    fn resolved_theme(&self) -> color::ThemeId {
        if !self.persisted.theme_auto {
            return self.persisted.theme;
        }
        #[cfg(windows)]
        let dark = self
            .main_pw()
            .window
            .as_ref()
            .and_then(|w| w.theme())
            .map(|t| t == winit::window::Theme::Dark)
            .unwrap_or(true);
        #[cfg(not(windows))]
        let dark = self.system_dark.unwrap_or(true);
        if dark { self.persisted.theme_dark } else { self.persisted.theme_light }
    }

    #[cfg(not(windows))]
    fn ensure_theme_watcher(&mut self) {
        if self.theme_watch_spawned {
            return;
        }
        self.theme_watch_spawned = true;
        let proxy = self.proxy.clone();
        win::watch_system_theme(move |dark| {
            let _ = proxy.send_event(UserEvent::SystemThemeChanged(dark));
        });
    }

    /// re-resolve and apply the auto theme on every window (called on the OS
    /// light/dark switch and when auto is turned on)
    fn apply_os_theme(&mut self) {
        let theme = self.resolved_theme();
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_theme(theme);
        }
        for sat in &mut self.satellites {
            if let Some(r) = sat.renderer.as_mut() {
                r.set_theme(theme);
            }
            if let Some(w) = sat.window.as_ref() {
                w.request_redraw();
            }
        }
        self.redraw();
    }

    fn save_config(&self) {
        use std::fmt::Write as _;
        // never persist a partial file: renderer-owned keys would be dropped and
        // fall back to defaults on the next load. always read the MAIN window's
        // renderer (a satellite may be swapped into self.pw when this is called)
        let Some(r) = self.main_pw().renderer.as_ref() else {
            return;
        };
        let Some(path) = config_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut s = String::new();
        let _ = writeln!(s, "scrollback={}", self.config.scrollback);
        let _ = writeln!(s, "copy_on_select={}", self.config.copy_on_select);
        let _ = writeln!(s, "shell={}", self.config.shell.label());
        let _ = writeln!(s, "load_profile={}", self.config.load_profile);
        let _ = writeln!(s, "close_action={}", self.config.close_action.label());
        let _ = writeln!(s, "right_click={}", self.config.right_click.label());
        let _ = writeln!(s, "backend={}", self.config.backend.label());
        let _ = writeln!(s, "restore_on_launch={}", self.config.restore_on_launch);
        let _ = writeln!(s, "font_size={}", r.content_pt() as i32);
        let _ = writeln!(s, "padding={}", r.pane_pad_px() as i32);
        let _ = writeln!(s, "opacity={}", r.opacity_pct());
        let _ = writeln!(s, "cursor={}", r.cursor_style_name());
        let _ = writeln!(s, "cursor_blink={}", r.cursor_blink());
        let _ = writeln!(s, "bold_as_bright={}", r.bold_as_bright());
        let _ = writeln!(s, "line_height={}", r.line_height());
        if self.persisted.theme_auto {
            let _ = writeln!(s, "theme=auto");
        } else {
            let _ = writeln!(s, "theme={}", r.theme().name());
        }
        if self.persisted.theme_dark != color::ThemeId::Instrument {
            let _ = writeln!(s, "theme_dark={}", self.persisted.theme_dark.name());
        }
        if self.persisted.theme_light != color::ThemeId::Paper {
            let _ = writeln!(s, "theme_light={}", self.persisted.theme_light.name());
        }
        let _ = writeln!(s, "font={}", r.font_name());
        if r.font_weight() != 400 {
            let _ = writeln!(s, "font_weight={}", r.font_weight());
        }
        if r.min_contrast() > 1.0 {
            let _ = writeln!(s, "min_contrast={}", r.min_contrast());
        }
        if let Some(bi) = &self.persisted.background_image {
            let _ = writeln!(s, "background_image={bi}");
            let _ = writeln!(s, "background_image_opacity={}", self.persisted.background_image_opacity);
        }
        if !r.ligatures() {
            let _ = writeln!(s, "ligatures=false");
        }
        if let Some(q) = &self.persisted.quake_key_raw {
            // an opt-in the panel can't edit; dropping it here silently killed
            // the user's drop-down hotkey on every settings change
            let _ = writeln!(s, "quake_key={q}");
        }
        if let Some(d) = &self.persisted.wsl_distro {
            let _ = writeln!(s, "wsl_distro={d}");
        }
        if self.persisted.plugin_sandbox {
            let spelling = if cfg!(windows) { "appcontainer" } else { "bwrap" };
            let _ = writeln!(s, "plugin_sandbox={spelling}");
        }
        if self.persisted.acrylic {
            let _ = writeln!(s, "acrylic=true");
        }
        for (name, line) in &self.persisted.profiles_raw {
            // config-file-only like quake_key: re-emit exactly as written
            let _ = writeln!(s, "profile.{name}={line}");
        }
        for (name, id) in &self.persisted.shell_themes {
            // config-file-only like profiles: panes of this shell/profile
            // render with their own theme
            let _ = writeln!(s, "theme.{name}={}", id.name());
        }
        if self.persisted.term_program != "termie" {
            // default is termie; only persist an override so the file stays short
            let _ = writeln!(s, "term_program={}", self.persisted.term_program);
        }
        if !self.persisted.inline_paint {
            // on by default; persist only the opt-out
            let _ = writeln!(s, "inline_paint=false");
        }
        if self.persisted.latency_hud {
            let _ = writeln!(s, "latency_hud=true");
        }
        if !self.persisted.update_check {
            // on by default; persist only the opt-out
            let _ = writeln!(s, "update_check=false");
        }
        let _ = std::fs::write(&path, s);
    }

    /// build a snapshot of the current window's tabs + split tree for persistence
    fn session_snapshot(&self) -> session::SessionFile {
        // always snapshot the MAIN window (a satellite may be swapped into self.pw
        // when a quit/close is triggered from a torn-off window)
        let main = self.main_pw();
        let mut tabs = Vec::new();
        for tab in &main.tabs {
            let Some(root) = tab.root.as_ref() else {
                continue;
            };
            let mut leaf_ids = Vec::new();
            let root = node_to_snap(root, &mut leaf_ids);
            let focused_leaf = leaf_ids.iter().position(|&id| id == tab.focused).unwrap_or(0);
            tabs.push(session::TabSnap { focused_leaf, root, title: tab.title.clone(), color: tab.color });
        }
        // capture the window's outer position + inner size for next launch; a
        // minimized window reports zero size, so fall back to the last good
        // bounds instead of writing a session with no window key (which would
        // silently reset placement after a quit-while-minimized)
        let window = self.live_window_bounds().or_else(|| self.last_window_bounds.clone());
        session::SessionFile { active_tab: main.active_tab, tabs, window }
    }

    /// the main window's current bounds, or None while minimized/degenerate
    fn live_window_bounds(&self) -> Option<session::WindowBounds> {
        let w = self.main_pw().window.as_ref()?;
        let size = w.inner_size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        let pos = w
            .outer_position()
            .ok()
            .map(|p| (p.x, p.y))
            .or_else(|| self.last_window_bounds.as_ref().map(|p| (p.x, p.y)))
            .unwrap_or((0, 0));
        Some(session::WindowBounds {
            x: pos.0,
            y: pos.1,
            width: size.width,
            height: size.height,
            maximized: w.is_maximized(),
        })
    }

    /// mark the layout changed and (re)arm the debounced session write so a burst
    /// of mutations collapses to one write ~750ms after the last change
    fn mark_session_dirty(&mut self) {
        // an ad-hoc cwd/command window never persists, so don't even arm the
        // debounce timer for it
        if self.session_ephemeral {
            return;
        }
        // remember the bounds while they're real, so a save that fires while
        // minimized still has something good to persist
        if let Some(b) = self.live_window_bounds() {
            self.last_window_bounds = Some(b);
        }
        self.session_dirty = true;
        self.session_flush_at = Some(Instant::now() + Duration::from_millis(750));
    }

    /// write the session atomically (temp + rename) so a reader never sees a
    /// half-written file; never clobber a good session with an empty one, nor
    /// with an ad-hoc folder/command window that shouldn't persist
    fn write_session(&self) {
        if self.session_ephemeral {
            return;
        }
        let snap = self.session_snapshot();
        if snap.tabs.is_empty() {
            return;
        }
        let Some(path) = session_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = snap.to_json_string();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            // rename-over-existing is atomic enough on windows; on the rare
            // failure (target briefly open) fall back to a direct write
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::write(&path, &text);
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// flush the current session now (clean-exit path) so the latest layout —
    /// even an unmutated fresh one — is saved for next launch
    fn flush_session_now(&mut self) {
        self.session_dirty = false;
        self.session_flush_at = None;
        self.write_session();
    }

    /// rebuild a saved pane tree by spawning fresh shells; pushes each leaf's new
    /// pane id into leaf_ids in order. None if a shell fails to spawn
    fn rebuild_node(
        &mut self,
        snap: &session::NodeSnap,
        cols: usize,
        rows: usize,
        leaf_ids: &mut Vec<usize>,
    ) -> Option<Node> {
        match snap {
            session::NodeSnap::Leaf { cwd, shell } => {
                let kind = ShellKind::from_label(shell);
                let pane = self.spawn_pane(cols, rows, cwd.clone(), Some(kind), None).ok()?;
                leaf_ids.push(pane.id);
                Some(Node::Leaf(pane))
            }
            session::NodeSnap::Split { vertical, ratio, a, b } => {
                let dir = if *vertical { Dir::Vertical } else { Dir::Horizontal };
                let a = Box::new(self.rebuild_node(a, cols, rows, leaf_ids)?);
                let b = Box::new(self.rebuild_node(b, cols, rows, leaf_ids)?);
                Some(Node::Split { dir, ratio: *ratio, a, b })
            }
        }
    }

    /// restore tabs from a saved session by spawning fresh shells in the saved
    /// directories. leaves self.pw.tabs empty on total failure so the caller falls
    /// back to a single shell
    fn restore_session(&mut self, sf: session::SessionFile) {
        let (cols, rows) = self.content_pane_size();
        for tab in &sf.tabs {
            let mut leaf_ids = Vec::new();
            let Some(root) = self.rebuild_node(&tab.root, cols, rows, &mut leaf_ids) else {
                continue;
            };
            let focused = leaf_ids
                .get(tab.focused_leaf)
                .copied()
                .or_else(|| leaf_ids.first().copied())
                .unwrap_or(0);
            self.pw.tabs.push(Tab {
                focused,
                root: Some(root),
                zoom: None,
                title: tab.title.clone(),
                attention: false,
                color: tab.color,
            });
        }
        if self.pw.tabs.is_empty() {
            return;
        }
        self.pw.active_tab = sf.active_tab.min(self.pw.tabs.len() - 1);
        self.relayout_all();
        self.sync_tabs();
    }

    fn set_pane_mode(&mut self, on: bool) {
        self.pw.pane_mode = on;
        if !on {
            self.drag_divider = None;
            self.pane_drag = None;
            #[cfg(target_os = "linux")]
            self.cancel_kwin_drag_probe();
            self.show_pane_drop(None);
            self.show_tab_drop(None);
        }
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_pane_mode(on);
        }
        self.redraw();
    }

    /// enter/leave keyboard mark mode: a selection cursor starts on the shell
    /// cursor and moves with the arrows; leaving clears the highlight
    fn set_mark_mode(&mut self, on: bool) {
        if on {
            let Some(pane) = self.active_focused_id() else {
                return;
            };
            let Some((cur, cur_abs, reflow_gen)) = self.focused_grid_mut().map(|g| {
                // start on the live screen, at the shell cursor
                g.scroll_view(-(g.view_offset as isize));
                let cur =
                    (g.cursor.row.min(g.rows.saturating_sub(1)), g.cursor.col.min(g.cols.saturating_sub(1)));
                (cur, (g.viewport_to_abs(cur.0), cur.1), g.reflow_gen)
            }) else {
                return;
            };
            self.mark = Some(MarkState { cur, anchor: None });
            self.selection = Some(Sel { pane, start: cur_abs, end: cur_abs, block: false, reflow_gen });
        } else {
            self.mark = None;
            self.selection = None;
        }
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_mark_mode(self.mark.is_some());
        }
        self.redraw();
    }

    /// one key in mark mode: arrows move the cursor (scrolling into history at
    /// the edges), shift extends, enter/ctrl+c copy and exit, esc exits
    fn mark_key(&mut self, logical: &Key) {
        use NamedKey as N;
        let shift = self.mods.shift_key();
        let ctrl = self.mods.control_key();
        let (Some(pane), Some((rows, cols))) = (
            self.active_focused_id(),
            self.focused_grid_mut().map(|g| (g.rows, g.cols)),
        ) else {
            self.set_mark_mode(false);
            return;
        };
        let Some(MarkState { mut cur, anchor }) = self.mark else {
            return;
        };
        // content-anchor the pre-move cell now: the move below may scroll the
        // view, and a viewport-anchored shift origin would drift with it
        let before_abs = self
            .focused_grid()
            .map(|g| (g.viewport_to_abs(cur.0), cur.1));
        match logical {
            Key::Named(N::Escape) => {
                self.set_mark_mode(false);
                return;
            }
            Key::Named(N::Enter) => {
                self.copy_selection();
                self.set_mark_mode(false);
                return;
            }
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => {
                self.copy_selection();
                self.set_mark_mode(false);
                return;
            }
            Key::Character(c) if ctrl && shift && c.eq_ignore_ascii_case("m") => {
                self.set_mark_mode(false);
                return;
            }
            Key::Named(N::ArrowLeft | N::ArrowRight) if ctrl => {
                let Some(start) = before_abs else {
                    return;
                };
                if let Some(g) = self.focused_grid_mut() {
                    let target = g.word_boundary(start, *logical == Key::Named(N::ArrowRight));
                    let top = g.viewport_to_abs(0);
                    let bottom = g.viewport_to_abs(rows.saturating_sub(1));
                    if target.0 < top {
                        g.scroll_view(top.saturating_sub(target.0).min(isize::MAX as u64) as isize);
                    } else if target.0 > bottom {
                        g.scroll_view(-(target.0.saturating_sub(bottom).min(isize::MAX as u64) as isize));
                    }
                    if let Some(row) = g.abs_to_viewport(target.0) {
                        cur = (row, target.1);
                    }
                }
            }
            Key::Named(N::ArrowLeft) => cur.1 = cur.1.saturating_sub(1),
            Key::Named(N::ArrowRight) => cur.1 = (cur.1 + 1).min(cols.saturating_sub(1)),
            Key::Named(N::ArrowUp) => {
                if cur.0 > 0 {
                    cur.0 -= 1;
                } else if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(1);
                }
            }
            Key::Named(N::ArrowDown) => {
                if cur.0 + 1 < rows {
                    cur.0 += 1;
                } else if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(-1);
                }
            }
            Key::Named(N::Home) if ctrl => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(g.scrollback.len() as isize);
                }
                cur = (0, 0);
            }
            Key::Named(N::End) if ctrl => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(-(g.view_offset as isize));
                }
                cur.0 = rows.saturating_sub(1);
                cur.1 = self.focused_grid_mut().map(|g| g.line_last_col(cur.0)).unwrap_or(0);
            }
            Key::Named(N::Home) => cur.1 = 0,
            Key::Named(N::End) => {
                cur.1 = self.focused_grid_mut().map(|g| g.line_last_col(cur.0)).unwrap_or(0);
            }
            // page moves scroll the view under a stationary cursor, so a page
            // is exactly a page whether or not history remains
            Key::Named(N::PageUp) => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(rows.saturating_sub(1) as isize);
                }
            }
            Key::Named(N::PageDown) => {
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(-(rows.saturating_sub(1) as isize));
                }
            }
            _ => return,
        }
        // shift starts (or keeps) an anchor at the pre-move cell; a plain move
        // collapses the selection back to just the cursor
        let anchor = if shift { anchor.or(before_abs) } else { None };
        let Some((cur_abs, reflow_gen)) = self
            .focused_grid()
            .map(|g| ((g.viewport_to_abs(cur.0), cur.1), g.reflow_gen))
        else {
            return;
        };
        self.mark = Some(MarkState { cur, anchor });
        self.selection =
            Some(Sel { pane, start: anchor.unwrap_or(cur_abs), end: cur_abs, block: false, reflow_gen });
        self.redraw();
    }

    /// focus the pane that most needs eyes: failed command beats a bell beats
    /// finished beats still-running; ties go to the nearest tab ahead of the
    /// current one, so repeated presses walk the whole set (viewing a done
    /// pane retires its badge, which naturally drains the queue)
    fn jump_attention(&mut self) {
        let n = self.pw.tabs.len();
        if n == 0 {
            return;
        }
        let cur_pane = self.active_focused_id();
        // (severity, tab distance, tab index, pane id)
        let mut best: Option<(u8, usize, usize, usize)> = None;
        for off in 0..n {
            let ti = (self.pw.active_tab + off) % n;
            let Some(tab) = self.pw.tabs.get(ti) else {
                continue;
            };
            let Some(root) = tab.root.as_ref() else {
                continue;
            };
            each_pane(root, &mut |p| {
                let mut rank = p.status.rank();
                // the tab's bell counts for its focused pane
                if tab.attention && p.id == tab.focused {
                    rank = rank.max(3);
                }
                if rank == 0 || (off == 0 && Some(p.id) == cur_pane) {
                    return;
                }
                let better = match best {
                    None => true,
                    Some((br, bd, ..)) => rank > br || (rank == br && off < bd),
                };
                if better {
                    best = Some((rank, off, ti, p.id));
                }
            });
        }
        let Some((_, _, ti, id)) = best else {
            self.show_notice("nothing needs attention");
            return;
        };
        let before = self.focus_identity();
        self.switch_tab(ti);
        if let Some(tab) = self.pw.tabs.get_mut(ti)
            && tab.focused != id
        {
            tab.focused = id;
            self.focus_anim = Some(Instant::now());
            self.sync_tabs();
            self.after_focus_context_change(before);
        }
        self.redraw();
    }

    /// open the new-tab profile menu as a dropdown anchored under the '+' button
    fn open_newtab_menu(&mut self) {
        let Some((nx, ny, _nw, nh)) = self.pw.renderer.as_ref().map(|r| r.newtab_rect()) else {
            return;
        };
        self.pw.pane_menu =
            Some(PaneMenu { x: nx, y: ny + nh, hovered: None, target: MenuTarget::NewTab });
        self.redraw();
    }

    /// run a context-menu item. pane items index render::PANE_MENU_ITEMS
    /// (0 copy, 1 split vertical, 2 split horizontal, 3 pop out, 4 close pane,
    /// 5 paste; copy no-ops with no selection); tab items index TAB_MENU_ITEMS
    /// (0 rename, 1 duplicate, 2 move left, 3 move right, 4 close, 5 close others)
    fn pane_menu_action(&mut self, target: MenuTarget, idx: usize, at: (f32, f32), event_loop: &ActiveEventLoop) {
        match target {
            MenuTarget::Pane => match idx {
                0 => {
                    self.copy_selection();
                    self.selection = None;
                }
                1 => self.split_focused(Dir::Vertical),
                2 => self.split_focused(Dir::Horizontal),
                3 => self.pop_out_focused(event_loop),
                4 => self.close_focused_pane(event_loop),
                5 => self.paste(),
                _ => {}
            },
            MenuTarget::Tab(i) => match idx {
                0 => {
                    if let Some(tab) = self.pw.tabs.get(i) {
                        let buf = tab.title.clone().unwrap_or_default();
                        self.pw.rename = Some(RenameState { tab: i, buf });
                        self.redraw();
                    }
                }
                1 => {
                    // duplicate: clone the clicked tab's focused pane cwd + shell
                    let ctx = self.pw.tabs.get(i).and_then(|t| {
                        let p = find_pane(t.root.as_ref()?, t.focused)?;
                        Some((cwd_path(p.term.cwd.as_deref()), p.shell))
                    });
                    let (cwd, shell) = ctx.map_or((None, None), |(c, s)| (c, Some(s)));
                    self.new_tab_cwd(cwd, shell);
                }
                // swap the menu for the swatch list, anchored where it was
                2 => {
                    self.pw.pane_menu =
                        Some(PaneMenu { x: at.0, y: at.1, hovered: None, target: MenuTarget::TabColor(i) });
                    self.redraw();
                }
                3 => self.move_tab(i, i.saturating_sub(1)),
                4 => self.move_tab(i, i + 1),
                5 => self.close_tab(i, event_loop),
                6 => self.close_others(i),
                _ => {}
            },
            MenuTarget::TabColor(i) => {
                if idx < render::TAB_COLOR_ITEMS.len()
                    && let Some(tab) = self.pw.tabs.get_mut(i)
                {
                    tab.color = (idx > 0).then_some(idx as u8);
                    self.sync_tabs();
                    self.redraw();
                }
            }
            MenuTarget::NewTab => {
                if let Some((_, shell)) = new_tab_menu_entries().into_iter().nth(idx) {
                    let cwd = self.focused_cwd();
                    self.new_tab_cwd(cwd, Some(shell));
                }
            }
        }
    }

    /// run one synthetic --drive key through the exact path a hardware press
    /// takes: the shortcut/overlay layer first, then the pty encoding
    fn inject_key(&mut self, mods: ModifiersState, logical: &Key, text: Option<&str>, event_loop: &ActiveEventLoop) {
        let saved = self.mods;
        self.mods = mods;
        if !self.handle_shortcut(logical, text, ElementState::Pressed, event_loop)
            && let Some(id) = self.active_focused_id()
        {
            let (app_cursor, kbd) = self
                .pw.tabs
                .get(self.pw.active_tab)
                .and_then(|t| t.root.as_ref())
                .and_then(|r| find_pane(r, id))
                .map(|p| (p.term.app_cursor_keys, p.term.kbd_flags()))
                .unwrap_or((false, 0));
            if let Some(bytes) = input::key_to_bytes(
                logical,
                text,
                None,
                ElementState::Pressed,
                false,
                self.mods,
                winit::keyboard::KeyLocation::Standard,
                app_cursor,
                kbd,
            ) {
                self.selection = None;
                self.write_to_focused(&bytes);
            }
        }
        self.mods = saved;
        self.redraw();
    }

    /// fire the --drive steps that have come due since the clock was armed
    fn drive_tick(&mut self, event_loop: &ActiveEventLoop) {
        // collect due steps first: injecting them re-borrows self
        let mut due = Vec::new();
        if let Some(d) = self.drive.as_mut()
            && let Some(started) = d.started
        {
            while d.next < d.steps.len() && started.elapsed() >= d.steps[d.next].0 {
                due.push(d.steps[d.next].1.clone());
                d.next += 1;
            }
        }
        for step in due {
            match step {
                DriveStep::Key(mods, key) => {
                    // the text a real press of this chord would carry
                    let txt = match &key {
                        Key::Character(s) if !mods.control_key() && !mods.alt_key() => {
                            Some(if mods.shift_key() { s.to_uppercase() } else { s.to_string() })
                        }
                        Key::Named(NamedKey::Space) => Some(" ".to_string()),
                        _ => None,
                    };
                    self.inject_key(mods, &key, txt.as_deref(), event_loop);
                }
                DriveStep::Type(text) => {
                    for ch in text.chars() {
                        let s = ch.to_string();
                        self.inject_key(ModifiersState::empty(), &Key::Character(s.as_str().into()), Some(&s), event_loop);
                    }
                }
                DriveStep::Pointer(position) => {
                    let inside = self.pw.window.as_ref().is_some_and(|window| {
                        let size = window.inner_size();
                        position.x >= 0.0
                            && position.y >= 0.0
                            && position.x < size.width as f64
                            && position.y < size.height as f64
                    });
                    if inside {
                        self.on_cursor_entered();
                    } else {
                        self.on_cursor_left();
                    }
                    self.on_cursor_moved(position);
                }
                DriveStep::Mouse(state) => {
                    self.on_mouse_input(state, MouseButton::Left, event_loop);
                }
            }
        }
    }

    /// grow/shrink the focused pane along `dir` (pane-mode keyboard resize)
    fn resize_focused(&mut self, dir: Dir, grow: bool) {
        let Some(fid) = self.pw.tabs.get(self.pw.active_tab).map(|t| t.focused) else {
            return;
        };
        let mut done = false;
        if let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut()) {
            grow_focused(root, fid, dir, grow, 0.04, &mut done);
        }
        if done {
            self.relayout_all();
            self.redraw();
        }
    }

    /// tear the focused pane off into its own OS window (multi-window)
    fn pop_out_focused(&mut self, event_loop: &ActiveEventLoop) {
        let Some(fid) = self.pw.tabs.get(self.pw.active_tab).map(|t| t.focused) else {
            return;
        };
        let count = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .map(|r| {
                let mut n = 0;
                each_pane(r, &mut |_| n += 1);
                n
            })
            .unwrap_or(0);
        if count < 2 {
            return; // don't strip a tab's only pane
        }
        let before = self.focus_identity();
        let mut popped: Option<Pane> = None;
        if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
            && let Some(root) = tab.root.take()
        {
            tab.root = extract_pane(root, fid, &mut popped);
            if let Some(r) = tab.root.as_ref() {
                tab.focused = first_leaf(r);
            }
        }
        let Some(pane) = popped else {
            return;
        };
        let mut pw = match self.create_satellite_window(event_loop, None) {
            Ok(pw) => pw,
            Err(_) => {
                // dock as a new tab (find-follow is inside dock_loose_pane)
                self.dock_loose_pane(pane);
                return;
            }
        };
        pw.tabs.push(tab_from_pane(pane));
        self.satellites.push(pw);
        // relayout + paint the new satellite via the swap-into-pw technique, then
        // relayout + repaint the main window (which just lost a pane)
        let idx = self.satellites.len() - 1;
        self.with_window(idx, |app| {
            // arm the quiet-settle reveal at the moment of first paint, mirroring
            // the main window's boot so a torn-off window eases in too
            if let Some(r) = app.pw.renderer.as_mut() {
                r.begin_reveal();
            }
            app.relayout_all();
            app.paint();
        });
        if let Some(window) = self.satellites[idx].window.as_ref() {
            window.set_visible(true);
        }
        self.relayout_all();
        self.sync_tabs();
        // surviving pane is now focused; find must leave the torn-off grid
        self.after_focus_context_change(before);
    }

    /// run `f` with satellite `idx` swapped into `self.pw`, so every self.pw-based
    /// method (relayout/paint/key handling) operates on that window, then swap
    /// back. the swap-back always runs because `f` returns normally
    fn with_window(&mut self, idx: usize, f: impl FnOnce(&mut Self)) {
        if idx >= self.satellites.len() {
            return;
        }
        // save + restore cur_sat so a pop-out triggered from inside a torn-off
        // window (already swapped in) leaves cur_sat consistent on return
        let prev = self.cur_sat;
        self.cur_sat = Some(idx);
        std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
        f(self);
        std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
        self.cur_sat = prev;
    }

    /// the main window's PaneWindow, even while a satellite is swapped into pw —
    /// session/config persistence and the quit/close logic must always target it
    fn main_pw(&self) -> &PaneWindow {
        match self.cur_sat {
            Some(i) => &self.satellites[i],
            None => &self.pw,
        }
    }

    /// the MAIN window's renderer even while a satellite is swapped into
    /// self.pw (update chips live on the main status bar only)
    fn main_pw_renderer(&mut self) -> Option<&mut Renderer> {
        match self.cur_sat {
            Some(i) => self.satellites[i].renderer.as_mut(),
            None => self.pw.renderer.as_mut(),
        }
    }

    /// handle a window event for satellite `idx`. close removes just that window;
    /// everything else is dispatched by swapping the satellite into self.pw and
    /// reusing the main key/shortcut/paint/relayout logic, so torn-off windows
    /// get the full tab/split behavior
    fn satellite_event(&mut self, idx: usize, event: WindowEvent, event_loop: &ActiveEventLoop) {
        if matches!(event, WindowEvent::CloseRequested) {
            if idx < self.satellites.len() {
                // more than one pane gets the count-gated confirm (shown on the
                // satellite's own window via the swap); a single pane closes
                let mut panes = 0;
                for tab in &self.satellites[idx].tabs {
                    if let Some(root) = tab.root.as_ref() {
                        each_pane(root, &mut |_| panes += 1);
                    }
                }
                if panes > 1 {
                    self.cur_sat = Some(idx);
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    self.pw.confirm = Some(ConfirmState {
                        prompt: format!("close this window's {panes} panes?"),
                        hint: "enter: close \u{b7} esc: cancel".to_string(),
                        action: ConfirmAction::CloseWindow,
                    });
                    self.redraw();
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    self.cur_sat = None;
                    return;
                }
                let mut sat = self.satellites.remove(idx);
                for tab in sat.tabs.iter_mut() {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
            }
            return;
        }
        if idx >= self.satellites.len() {
            return;
        }
        self.cur_sat = Some(idx);
        std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
        if let (Some(adapter), Some(window)) = (self.pw.a11y.as_mut(), self.pw.window.as_ref()) {
            adapter.process_event(window, &event);
        }
        match event {
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(size) => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
                // reconfigure the satellite's GPU surface before relayout — this is
                // the only place config.width/height + surface.configure() update
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.relayout_all();
                self.paint();
            }
            WindowEvent::Moved(_) => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_scale(scale_factor as f32);
                }
                self.relayout_all();
                self.paint();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event: ke, .. } => {
                // input-to-photon: stamp the first keypress not yet reflected on
                // screen; paint() clears it once the resulting frame presents
                if ke.state == ElementState::Pressed && self.input_at.is_none() {
                    self.input_at = Some(Instant::now());
                }
                // while composing, the IME owns text input (mirrors the main
                // window's swallow; expressed without `return` so the pw
                // swap-back below always runs)
                let ime_owns = self.pw.ime_composing && ke.state == ElementState::Pressed;
                // releases flow through too: key_to_bytes reports them when the
                // pane's kitty flags ask for event types, exactly like the main
                // window's path
                if !ime_owns && !self.handle_shortcut(&ke.logical_key, ke.text.as_deref(), ke.state, event_loop) {
                    let id = self.active_focused_id();
                    let (app_cursor, kbd) = id
                        .and_then(|id| {
                            self.pw
                                .tabs
                                .get(self.pw.active_tab)
                                .and_then(|t| t.root.as_ref())
                                .and_then(|r| find_pane(r, id))
                        })
                        .map(|p| (p.term.app_cursor_keys, p.term.kbd_flags()))
                        .unwrap_or((false, 0));
                    if let Some(bytes) = input::key_to_bytes(
                        &ke.logical_key,
                        ke.text.as_deref(),
                        unshifted_char(&ke),
                        ke.state,
                        ke.repeat,
                        self.mods,
                        ke.location,
                        app_cursor,
                        kbd,
                    ) {
                        self.write_to_focused(&bytes);
                    }
                }
                self.paint();
            }
            WindowEvent::ThemeChanged(_) => {
                // the windows light/dark setting flipped; under theme=auto
                // every window re-resolves and repaints
                if self.persisted.theme_auto {
                    self.apply_os_theme();
                }
            }
            WindowEvent::Focused(f) => {
                if f && let Some(window) = self.pw.window.as_ref() {
                    win::clear_attention(window);
                }
                // a --drive window paints focused no matter the real focus
                // (it is WS_EX_NOACTIVATE, so real focus never arrives)
                self.pw.focused = f || self.drive.is_some();
                if !f {
                    self.pw.ime_composing = false;
                    self.pw.ime_preedit.clear();
                    self.pw.ime_preedit_caret = None;
                    self.release_held_input();
                }
                // coming back acknowledges the active tab's bell dot
                if f && self.pw.tabs.get(self.pw.active_tab).is_some_and(|t| t.attention) {
                    self.sync_tabs();
                }
                // report focus in/out to a pane that enabled mode 1004, the
                // same as the main window does
                if let Some(id) = self.active_focused_id()
                    && let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut())
                        && let Some(p) = find_pane_mut(root, id)
                            && p.term.focus_events {
                                p.pty.write(if f { b"\x1b[I" } else { b"\x1b[O" });
                            }
                self.paint();
            }
            WindowEvent::Ime(ime) => self.on_ime(ime),
            // torn-off windows get the same mouse handling as the main window —
            // hover/selection/scroll/context-menu/title-bar — via the swapped-in
            // self.pw (close button + close-tab stay window-scoped through cur_sat)
            WindowEvent::CursorMoved { position, .. } => self.on_cursor_moved(position),
            WindowEvent::CursorEntered { .. } => self.on_cursor_entered(),
            WindowEvent::CursorLeft { .. } => self.on_cursor_left(),
            WindowEvent::MouseWheel { delta, .. } => self.on_mouse_wheel(delta),
            WindowEvent::MouseInput {
                state, button, ..
            } => self.on_mouse_input(state, button, event_loop),
            // the swapped-in self.pw routes the drop to this window's strip
            // and panes, same as the main window
            WindowEvent::DroppedFile(path) => self.on_dropped_file(&path),
            _ => {}
        }
        std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
        self.cur_sat = None;
        // a pane-mode close ('x') that emptied this satellite closes its window
        // (do_close_tab declined to exit the app because cur_sat was set)
        if self.satellites.get(idx).is_some_and(|s| s.tabs.is_empty()) {
            self.satellites.remove(idx);
        }
        self.cleanup_empty_windows();
    }

    /// re-attach a loose pane as a new tab (used if a satellite window won't open)
    fn dock_loose_pane(&mut self, pane: Pane) {
        let before = self.focus_identity();
        let fid = pane.id;
        self.pw.tabs.push(Tab {
            focused: fid,
            root: Some(Node::Leaf(pane)),
            zoom: None,
            title: None,
            attention: false,
            color: None,
        });
        self.pw.active_tab = self.pw.tabs.len() - 1;
        self.relayout_all();
        self.sync_tabs();
        self.after_focus_context_change(before);
    }

    /// the index of the satellite owning window `id`, if any
    fn satellite_for(&self, id: WindowId) -> Option<usize> {
        self.satellites
            .iter()
            .position(|s| s.window.as_ref().map(|w| w.id()) == Some(id))
    }

    /// the index of the satellite whose tree contains pane `pid`, if any
    fn satellite_with_pane(&self, pid: usize) -> Option<usize> {
        self.satellites
            .iter()
            .position(|s| s.tabs.iter().any(|t| t.root.as_ref().is_some_and(|r| find_pane(r, pid).is_some())))
    }

    /// toggle the quake drop-down: summon the window to the top of the active
    /// monitor (full width, ~45% height, always-on-top, focused), or hide it.
    /// only ever reached via the global hotkey or the palette action
    #[cfg(any(windows, target_os = "linux"))]
    fn toggle_quake(&mut self) {
        let Some(win) = self.pw.window.clone() else {
            return;
        };
        if self.quake_shown {
            #[cfg(target_os = "linux")]
            let hidden = win::hide_quake_window(&win);
            #[cfg(not(target_os = "linux"))]
            let hidden = false;
            if !hidden {
                win.set_visible(false);
            }
            // a user-toggled always-on-top survives quake dismissal
            if !self.pw.on_top {
                win.set_window_level(WindowLevel::Normal);
                win::set_window_above(&win, false);
            }
            self.quake_shown = false;
            return;
        }
        let mon = win
            .current_monitor()
            .or_else(|| win.primary_monitor())
            .or_else(|| win.available_monitors().next());
        #[cfg(target_os = "linux")]
        {
            if !win::show_quake_window(&win) {
                win.set_visible(true);
                win.set_minimized(false);
            }
        }
        if let Some(mon) = mon {
            let pos = mon.position();
            let size = mon.size();
            let h = ((size.height as f64 * 0.45).round() as u32).max(120);
            win.set_outer_position(PhysicalPosition::new(pos.x, pos.y));
            let _ = win.request_inner_size(PhysicalSize::new(size.width, h));
        }
        win.set_window_level(WindowLevel::AlwaysOnTop);
        #[cfg(not(target_os = "linux"))]
        win.set_visible(true);
        win.focus_window();
        self.quake_shown = true;
        self.redraw();
    }

    /// borderless fullscreen on the window's current monitor, F11 to toggle —
    /// the custom chrome stays, so tabs and the status bar remain reachable
    fn toggle_fullscreen(&mut self) {
        if let Some(win) = self.pw.window.as_ref() {
            let on = win.fullscreen().is_some();
            win.set_fullscreen(if on {
                None
            } else {
                Some(winit::window::Fullscreen::Borderless(None))
            });
            self.redraw();
        }
    }

    /// report a mouse event to the pane under the cursor if it has mouse mode on;
    /// returns true if forwarded (caller should skip local selection/scroll)
    fn mouse_report(&mut self, btn: u8, pressed: bool, motion: bool) -> bool {
        let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
        let Some(id) = self.pane_at(cx, cy) else {
            return false;
        };
        self.report_to_pane(id, btn, pressed, motion)
    }

    /// report a mouse event to a specific pane (coords clamped to its rect);
    /// used to keep a drag locked to the pane that received the press
    fn report_to_pane(&mut self, id: usize, btn: u8, pressed: bool, motion: bool) -> bool {
        let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
        let Some(rect) = self.pw.layout_cache.iter().find(|(i, _)| *i == id).map(|(_, r)| *r) else {
            return false;
        };
        let Some((col, row)) = self.pw.renderer.as_ref().map(|r| r.cell_at(rect, cx, cy)) else {
            return false;
        };
        // xterm modifier bitfield (shift 4, alt 8, ctrl 16) for the mouse report
        let mmods = (if self.mods.shift_key() { 4u8 } else { 0 })
            | (if self.mods.alt_key() { 8 } else { 0 })
            | (if self.mods.control_key() { 16 } else { 0 });
        // drop identical motion reports: winit fires CursorMoved at the OS
        // sample rate, often many times per cell. flooding any-event mode
        // fills a TUI's input buffer with the same CSI and the leftover bytes
        // show up as garbage in the composer when the parser falls behind
        let key = (id, btn, pressed, motion, col, row, mmods);
        if motion && self.last_mouse_report == Some(key) {
            return true;
        }
        let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut()) else {
            return false;
        };
        let Some(p) = find_pane_mut(root, id) else {
            return false;
        };
        if let Some(bytes) = p.term.encode_mouse(btn, pressed, motion, col, row, mmods) {
            p.pty.write(&bytes);
            self.last_mouse_report = Some(key);
            true
        } else {
            false
        }
    }

    /// does the pane under the cursor want motion events right now?
    fn pane_wants_motion(&self) -> bool {
        let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
        let Some(id) = self.pane_at(cx, cy) else {
            return false;
        };
        self.pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, id)))
            .map(|p| p.term.wants_motion(self.mouse_down.is_some()))
            .unwrap_or(false)
    }

    /// set the OS pointer icon, skipping the call when it hasn't changed
    fn set_pointer(&mut self, icon: CursorIcon) {
        if self.pw.cursor_icon != icon {
            self.pw.cursor_icon = icon;
            if let Some(w) = &self.pw.window {
                w.set_cursor(icon);
            }
        }
    }

    /// pointer motion: hover/link/divider feedback, selection drag, mouse-report
    /// motion, divider drag. operates on self.pw so the swap reuses it for any
    /// window
    // clear transient input state on focus loss: a stuck modifier (winit can
    // miss the release on windows), and drop any hover-link underline so it
    // doesn't linger over the window while it's unfocused
    fn release_held_input(&mut self) {
        self.mods = ModifiersState::empty();
        // scroll-thumb state is window-local; tab and pane drags stay alive so
        // they can cross into another termie window
        self.sb_drag = None;
        if self.link.take().is_some() {
            self.set_pointer(CursorIcon::Default);
        }
    }

    fn on_cursor_left(&mut self) {
        let Some(current) = self.pw.window.as_ref().map(|window| window.id()) else {
            return;
        };
        let clear_tabs = self.tab_drag.as_mut().is_some_and(|drag| drag.window_left(current));
        let clear_panes = self.pane_drag.as_mut().is_some_and(|drag| drag.window_left(current));
        if clear_tabs {
            self.show_tab_drop(None);
            self.show_drag_preview(None);
        }
        if clear_panes {
            self.show_pane_drop(None);
            self.show_tab_drop(None);
            self.show_drag_preview(None);
        }
    }

    fn on_cursor_entered(&mut self) {
        let Some(current) = self.pw.window.as_ref().map(|window| window.id()) else {
            return;
        };
        if let Some(drag) = self.tab_drag.as_mut()
            && drag.source == current
        {
            drag.left_window = false;
        }
        if let Some(drag) = self.pane_drag.as_mut()
            && drag.source_window == current
        {
            drag.left_window = false;
        }
    }

    fn on_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        self.pw.cursor = position;
        let (px, py) = (position.x as f32, position.y as f32);
        // hovering the open palette moves its selection with the pointer
        if self.palette.is_some() {
            if let Some(i) = self.pw.renderer.as_ref().and_then(|r| r.palette_row_at(px, py))
                && self.palette.as_ref().map(|p| p.selected) != Some(i)
            {
                if let Some(p) = self.palette.as_mut() {
                    p.selected = i;
                }
                self.redraw();
            }
            return;
        }
        // hovering the font picker previews the font under the pointer
        if self.font_pick.is_some() {
            if let Some(i) = self.pw.renderer.as_ref().and_then(|r| r.palette_row_at(px, py))
                && self.font_pick.as_ref().map(|p| p.selected) != Some(i)
            {
                if let Some(p) = self.font_pick.as_mut() {
                    p.selected = i;
                }
                self.font_pick_preview();
            }
            return;
        }
        // while the pane menu is open, only track which item is hovered
        if self.pw.pane_menu.is_some() {
            let h = self.pw.renderer.as_ref().and_then(|r| r.pane_menu_item_at(px, py));
            if let Some(m) = self.pw.pane_menu.as_mut()
                && m.hovered != h
            {
                m.hovered = h;
                self.redraw();
            }
            return;
        }
        // dragging the scroll thumb: map pointer-y to a scroll offset and skip
        // every other motion handler while the thumb is held
        if let Some((id, _)) = self.sb_drag {
            self.apply_scrollbar_drag(id, py);
            self.redraw();
            return;
        }
        // drag-reordering a tab: the held tab follows the pointer along the
        // strip, swapping places live as it crosses its neighbors (equal tab
        // widths keep the pointer inside the moved tab, so this can't oscillate)
        if let Some(mut drag) = self.tab_drag.take() {
            let current = self.pw.window.as_ref().map(|w| w.id());
            let inside = self.pw.window.as_ref().is_some_and(|window| {
                let size = window.inner_size();
                position.x >= 0.0
                    && position.y >= 0.0
                    && position.x < size.width as f64
                    && position.y < size.height as f64
            });
            if current == Some(drag.source) && inside {
                drag.left_window = false;
            }
            if let Some(window) = self.pw.window.as_ref() {
                drag.screen = window.inner_position().ok().map(|origin| {
                    PhysicalPosition::new(
                        origin.x + position.x.round() as i32,
                        origin.y + position.y.round() as i32,
                    )
                });
            }
            let travelled = (position.x - drag.start.x).abs() + (position.y - drag.start.y).abs();
            if let Some(current) = current
                && current != drag.source
            {
                let index = self
                    .pw
                    .renderer
                    .as_ref()
                    .and_then(|r| match r.hit_test(px, py) {
                        Hit::Button(Hot::Tab(i) | Hot::TabClose(i)) => Some(i),
                        _ => None,
                    })
                    .unwrap_or(self.pw.tabs.len());
                drag.target = Some((current, index));
            } else if current == Some(drag.source) {
                drag.target = drag
                    .screen
                    .and_then(|point| self.window_at_screen(point))
                    .filter(|(window, _)| *window != drag.source);
                #[cfg(target_os = "linux")]
                if drag.target.is_none() {
                    drag.target = self
                        .kwin_drag_point(drag.source, position)
                        .and_then(|point| self.kwin_tab_drop_at(point))
                        .filter(|(window, _)| *window != drag.source);
                }
            }
            if let Some(Hit::Button(Hot::Tab(j) | Hot::TabClose(j))) =
                self.pw.renderer.as_ref().map(|r| r.hit_test(px, py))
                && current == Some(drag.source)
                && j != drag.index
            {
                self.move_tab(drag.index, j);
                drag.index = j;
            } else if travelled > 12.0 {
                drag.left_strip = true;
            }
            self.show_tab_drop(drag.target);
            if let Some(current) = current {
                self.show_drag_preview(Some((current, px, py, drag.label.clone(), false)));
            }
            self.set_pointer(CursorIcon::Grabbing);
            self.tab_drag = Some(drag);
            return;
        }
        if let Some(mut drag) = self.pane_drag.take() {
            let current = self.pw.window.as_ref().map(|window| window.id());
            let inside = self.pw.window.as_ref().is_some_and(|window| {
                let size = window.inner_size();
                position.x >= 0.0
                    && position.y >= 0.0
                    && position.x < size.width as f64
                    && position.y < size.height as f64
            });
            if current == Some(drag.source_window) && inside {
                drag.left_window = false;
            }
            if current != Some(drag.source_window)
                || (position.x - drag.start.x).abs() + (position.y - drag.start.y).abs() > 8.0
            {
                drag.moved = true;
            }
            if let Some(window) = self.pw.window.as_ref() {
                drag.screen = window.inner_position().ok().map(|origin| {
                    PhysicalPosition::new(
                        origin.x + position.x.round() as i32,
                        origin.y + position.y.round() as i32,
                    )
                });
            }
            drag.target = Self::pane_drop_at(&self.pw, px, py)
                .map(PaneDropDestination::Dock)
                .or_else(|| {
                    Self::pane_tab_drop_at(&self.pw, px, py)
                        .map(|(window, index)| PaneDropDestination::Tab(window, index))
                });
            if drag.target.is_none() {
                drag.target = drag.screen.and_then(|point| self.pane_drop_at_screen(point));
                #[cfg(target_os = "linux")]
                if drag.target.is_none() {
                    drag.target = self
                        .kwin_drag_point(drag.source_window, position)
                        .and_then(|point| self.kwin_pane_drop_at(point));
                }
            }
            let pane_target = drag.target.and_then(|target| match target {
                PaneDropDestination::Dock(target)
                    if target.window != drag.source_window
                        || target.tab != drag.source_tab
                        || target.pane != drag.pane =>
                {
                    Some(target)
                }
                _ => None,
            });
            let tab_target = drag.target.and_then(|target| match target {
                PaneDropDestination::Tab(window, index) => Some((window, index)),
                _ => None,
            });
            self.show_pane_drop(pane_target);
            self.show_tab_drop(tab_target);
            if let Some(current) = current {
                self.show_drag_preview(Some((current, px, py, drag.label.clone(), true)));
            }
            self.set_pointer(CursorIcon::Grabbing);
            self.pane_drag = Some(drag);
            return;
        }
        // mouse-tracking motion (1002 drag / 1003 any-motion)
        if self.drag_divider.is_none() && !self.pw.settings_open && !self.mods.shift_key() {
            if let Some((btn, id)) = self.mouse_down {
                // a forwarded press is held: lock motion to the press-pane
                // (even off its rect) and don't fall through to selection
                let wants = self
                    .pw.tabs
                    .get(self.pw.active_tab)
                    .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, id)))
                    .map(|p| p.term.wants_motion(true))
                    .unwrap_or(false);
                if wants {
                    self.report_to_pane(id, btn, true, true);
                }
                return;
            } else if self.pane_wants_motion() {
                // 1003 any-motion with no button held
                self.mouse_report(3, true, true);
                return;
            }
        }
        if let Some(path) = self.drag_divider.clone() {
            // pane-mode: drag a divider to resize the split
            if let Some(content) = self.pw.renderer.as_ref().map(|r| r.content_rect()) {
                if let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut()) {
                    set_divider_ratio(root, content, &path, px, py);
                }
                self.relayout_all();
                self.redraw();
            }
        } else if self.selecting {
            if let (Some(mut sel), Some((row, col))) =
                (self.selection, self.cell_in_focused(px, py))
                && let Some((abs, reflow_gen)) =
                    self.focused_grid().map(|g| (g.viewport_to_abs(row), g.reflow_gen))
                && sel.reflow_gen == reflow_gen
            {
                sel.end = (abs, col);
                self.selection = Some(sel);
                // dragging past the pane's top/bottom edge arms the autoscroll
                // tick in about_to_wait, which keeps extending while held there
                self.sel_autoscroll = self.sel_edge_speed(py);
                self.redraw();
            }
        } else {
            if let Some(r) = self.pw.renderer.as_mut() {
                let hovered = match r.hit_test(px, py) {
                    Hit::Button(c) => Some(c),
                    _ => None,
                };
                if r.set_hovered(hovered) {
                    self.redraw();
                }
            }
            // hover a url: underline it and show a hand so links read as
            // clickable without holding a modifier. opening still needs
            // ctrl+click (checked against tracked modifiers at click time); a plain
            // click here still starts a selection
            let new_link = if !self.pw.settings_open {
                self.focused_url_at(px, py).map(|(r, a, b, _)| (r, a, b))
            } else {
                None
            };
            if new_link != self.link {
                self.link = new_link;
                self.redraw();
            }
            let icon = if new_link.is_some() {
                CursorIcon::Pointer
            } else {
                // otherwise show a resize pointer over a split divider
                let dir = if self.pw.settings_open || self.mods.shift_key() {
                    None
                } else if let (Some(content), Some(root)) = (
                    self.pw.renderer.as_ref().map(|r| r.content_rect()),
                    self.pw.tabs.get(self.pw.active_tab).and_then(|t| t.root.as_ref()),
                ) {
                    divider_dir(root, content, px, py, 6.0)
                } else {
                    None
                };
                match dir {
                    Some(Dir::Vertical) => CursorIcon::EwResize,
                    Some(Dir::Horizontal) => CursorIcon::NsResize,
                    None if self.pw.pane_mode && self.pane_at(px, py).is_some() => CursorIcon::Grab,
                    None => CursorIcon::Default,
                }
            };
            self.set_pointer(icon);
        }
    }

    /// wheel: settings-panel scroll, mouse-report wheel buttons, or local scrollback
    fn on_mouse_wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        use winit::event::MouseScrollDelta;
        // ctrl+wheel zooms the content font, like every other windows terminal
        if self.mods.control_key() {
            let y = match delta {
                MouseScrollDelta::LineDelta(_, y) => y,
                MouseScrollDelta::PixelDelta(p) => p.y as f32,
            };
            if y != 0.0 {
                self.nudge_font(if y > 0.0 { 1.0 } else { -1.0 });
            }
            return;
        }
        let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
        // the open settings panel grabs the wheel when hovered
        if self.pw.settings_open {
            let over = self.pw.renderer.as_ref().map(|r| r.in_settings_panel(cx, cy)).unwrap_or(false);
            if over {
                let amt = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * 40.0,
                    MouseScrollDelta::PixelDelta(p) => -(p.y as f32),
                };
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.scroll_settings(amt);
                }
                self.redraw();
                return;
            }
        }
        let up = match delta {
            MouseScrollDelta::LineDelta(_, y) => y > 0.0,
            MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
        };
        // a TUI with mouse reporting gets wheel events (button 64/65)
        if self.mouse_report(if up { 64 } else { 65 }, true, false) {
            return;
        }
        if let Some(id) = self.pane_at(cx, cy)
            && let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
                && let Some(root) = tab.root.as_mut()
                    && let Some(p) = find_pane_mut(root, id) {
                        let lines = match delta {
                            // honor the windows wheel-lines setting per notch;
                            // the page sentinel scrolls a pane height
                            MouseScrollDelta::LineDelta(_, y) => match win::wheel_scroll_lines() {
                                u32::MAX => y * p.term.grid.rows.saturating_sub(1).max(1) as f32,
                                n => y * n.max(1) as f32,
                            },
                            MouseScrollDelta::PixelDelta(px) => (px.y / 20.0) as f32,
                        };
                        // fractional accumulator: a slow precision-touchpad
                        // scroll delivers a few px per event, which used to
                        // round to zero lines and go nowhere
                        self.wheel_accum += lines;
                        let step = self.wheel_accum.trunc();
                        if step == 0.0 {
                            return;
                        }
                        self.wheel_accum -= step;
                        if !p.term.using_alt {
                            p.term.grid.scroll_view(step as isize);
                            self.redraw();
                        } else if p.term.alt_scroll {
                            // the alt screen has no scrollback: translate the
                            // wheel to arrow keys (the default-on "alternate
                            // scroll" other terminals ship) so pagers — less,
                            // man, git log — scroll under the wheel
                            let seq: &[u8] = if step > 0.0 {
                                if p.term.app_cursor_keys { b"\x1bOA" } else { b"\x1b[A" }
                            } else if p.term.app_cursor_keys {
                                b"\x1bOB"
                            } else {
                                b"\x1b[B"
                            };
                            let n = step.abs() as usize;
                            let mut buf = Vec::with_capacity(seq.len() * n);
                            for _ in 0..n {
                                buf.extend_from_slice(seq);
                            }
                            p.pty.write(&buf);
                        }
                    }
    }

    /// left/right/middle button press+release: context menu, tab close, selection,
    /// click-to-focus, divider/pane drag, link open, widget click, title-bar buttons
    fn on_mouse_input(&mut self, state: ElementState, button: MouseButton, event_loop: &ActiveEventLoop) {
        match button {
            MouseButton::Back | MouseButton::Forward => {
                let logical = if button == MouseButton::Back {
                    Key::Named(NamedKey::F23)
                } else {
                    Key::Named(NamedKey::F24)
                };
                self.handle_shortcut(&logical, None, state, event_loop);
            }
            MouseButton::Right if state == ElementState::Pressed => {
                // right-click opens a context menu at the cursor: over a pane it
                // targets that pane (focus it first so the actions land there);
                // over a tab it targets that tab (rename / duplicate / move / close)
                let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
                match self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy)) {
                    Some(Hit::Content) => {
                        self.focus_pane_at(cx, cy);
                        // right_click=paste (WT muscle memory): copy an active
                        // selection then clear it, else paste. shift bypasses to
                        // the context menu, which carries splits / pop-out
                        if self.config.right_click == RightClick::Paste && !self.mods.shift_key() {
                            if self.selection.is_some() {
                                self.copy_selection();
                                self.selection = None;
                                self.redraw();
                            } else {
                                self.paste();
                            }
                        } else {
                            self.pw.pane_menu =
                                Some(PaneMenu { x: cx, y: cy, hovered: None, target: MenuTarget::Pane });
                            self.redraw();
                        }
                    }
                    Some(Hit::Button(Hot::Tab(i) | Hot::TabClose(i))) => {
                        self.pw.pane_menu = Some(PaneMenu {
                            x: cx,
                            y: cy,
                            hovered: None,
                            target: MenuTarget::Tab(i),
                        });
                        self.redraw();
                    }
                    // right-clicking the '+' (or its chevron) opens the same
                    // profile dropdown the chevron does
                    Some(Hit::Button(Hot::NewTab | Hot::NewTabMenu)) => self.open_newtab_menu(),
                    _ => {}
                }
            }
            MouseButton::Middle if state == ElementState::Pressed => {
                let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
                if let Some(Hit::Button(Hot::Tab(i) | Hot::TabClose(i))) =
                    self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy))
                {
                    self.close_tab(i, event_loop);
                }
            }
            MouseButton::Left => {
                let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
                if state == ElementState::Released && self.tab_drag.is_some() {
                    self.finish_tab_drag(event_loop);
                    self.pressed = None;
                    return;
                }
                if state == ElementState::Released && self.pane_drag.is_some() {
                    self.finish_pane_drag(event_loop);
                    self.pressed = None;
                    return;
                }
                // the marketplace is a full-page overlay: handle its clicks and
                // consume the press so nothing falls through to the panes
                if self.market.is_some() {
                    if state == ElementState::Pressed {
                        self.market_click(cx, cy);
                    }
                    return;
                }
                // the font picker owns every click: a row commits that font,
                // anywhere else cancels back to the original
                if self.font_pick.is_some() {
                    if state == ElementState::Pressed {
                        let row = self.pw.renderer.as_ref().and_then(|r| r.palette_row_at(cx, cy));
                        let inside =
                            self.pw.renderer.as_ref().is_some_and(|r| r.palette_contains(cx, cy));
                        if let Some(i) = row {
                            if let Some(p) = self.font_pick.as_mut() {
                                p.selected = i;
                            }
                            self.close_font_picker(true);
                        } else if !inside {
                            self.close_font_picker(false);
                        }
                    }
                    return;
                }
                // the open palette owns every click: an entry runs, anywhere
                // else dismisses — and nothing falls through to the panes
                if self.palette.is_some() {
                    if state == ElementState::Pressed {
                        let row = self.pw.renderer.as_ref().and_then(|r| r.palette_row_at(cx, cy));
                        let inside =
                            self.pw.renderer.as_ref().is_some_and(|r| r.palette_contains(cx, cy));
                        if let Some(i) = row {
                            let (mode, q) = self
                                .palette
                                .as_ref()
                                .map(|p| (p.mode, p.query.clone()))
                                .unwrap_or((PaletteMode::Commands, String::new()));
                            self.palette = None;
                            self.run_palette_choice(mode, &q, i, event_loop);
                            self.redraw();
                        } else if !inside {
                            self.palette = None;
                            self.redraw();
                        }
                    }
                    return;
                }
                // find-bar buttons: regex toggle / prev / next / close (misses
                // fall through so text can still be selected while find is open)
                if self.find.is_some()
                    && state == ElementState::Pressed
                    && let Some(btn) = self.pw.renderer.as_ref().and_then(|r| r.find_btn_at(cx, cy))
                {
                    match btn {
                        0 => {
                            self.find_regex = !self.find_regex;
                            self.find_recompute();
                        }
                        1 => self.find_step(false),
                        2 => self.find_step(true),
                        _ => {
                            self.find = None;
                            self.redraw();
                        }
                    }
                    return;
                }
                // the status-bar UPDATE chip opens the install confirm
                if state == ElementState::Pressed
                    && self.update.is_some()
                    && self.pw.renderer.as_ref().is_some_and(|r| r.update_chip_at(cx, cy))
                {
                    self.run_action(PaletteAction::InstallUpdate, event_loop);
                    return;
                }
                // scroll-thumb drag: a press on the thumb (or its track) grabs it
                // and takes priority over selection / TUI mouse forwarding; the
                // matching release just ends the drag
                if state == ElementState::Released && self.sb_drag.take().is_some() {
                    self.redraw();
                    return;
                }
                if state == ElementState::Pressed
                    && let Some((id, t)) = self.scrollbar_hit(cx, cy)
                {
                    // grab the thumb where pressed; a press on the bare track jumps
                    // the thumb so its centre lands under the pointer, then drags
                    let grab_dy = if cy >= t.thumb_y && cy <= t.thumb_y + t.thumb_h {
                        cy - t.thumb_y
                    } else {
                        t.thumb_h / 2.0
                    };
                    self.sb_drag = Some((id, grab_dy));
                    self.apply_scrollbar_drag(id, cy);
                    self.redraw();
                    return;
                }
                let hit = self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy));
                // a left-press while the pane menu is open runs the clicked item
                // (or dismisses it when the click lands elsewhere)
                if let Some(menu) = self.pw.pane_menu.as_ref()
                    && state == ElementState::Pressed
                {
                    let target = menu.target;
                    let at = (menu.x, menu.y);
                    let item = self.pw.renderer.as_ref().and_then(|r| r.pane_menu_item_at(cx, cy));
                    self.pw.pane_menu = None;
                    if let Some(i) = item {
                        self.pane_menu_action(target, i, at, event_loop);
                    }
                    self.redraw();
                    return;
                }
                // always finalize a forwarded press with a release report, even if
                // the cursor left the pane (else the TUI sees a stuck drag)
                if state == ElementState::Released
                    && let Some((btn, id)) = self.mouse_down.take() {
                        self.report_to_pane(id, btn, false, false);
                        return;
                    }
                // while the settings panel is open, a press outside it dismisses it
                // (and is consumed); presses inside fall through to its controls
                if self.pw.settings_open && state == ElementState::Pressed {
                    let in_panel = self
                        .pw.renderer
                        .as_ref()
                        .map(|r| r.in_settings_panel(cx, cy))
                        .unwrap_or(false);
                    if !in_panel {
                        self.close_settings();
                        return;
                    }
                    if let Some(Hit::Button(h)) = hit {
                        self.pressed = Some(h);
                    }
                    return;
                }
                // reaching for the mouse leaves mark mode; the click then acts
                // on the pane normally (focus, selection, buttons)
                if self.mark.is_some() && state == ElementState::Pressed {
                    self.set_mark_mode(false);
                }
                // pane mode: drag a divider to resize or place a pane beside another
                if self.pw.pane_mode && !matches!(hit, Some(Hit::Button(_)) | Some(Hit::TitleBar) | Some(Hit::Resize(_))) {
                    match state {
                        ElementState::Pressed => {
                            let found = if let (Some(content), Some(root)) = (
                                self.pw.renderer.as_ref().map(|r| r.content_rect()),
                                self.pw.tabs.get(self.pw.active_tab).and_then(|t| t.root.as_ref()),
                            ) {
                                let mut path = Vec::new();
                                find_divider(root, content, cx, cy, 8.0, &mut path)
                            } else {
                                None
                            };
                            if let Some(p) = found {
                                self.drag_divider = Some(p);
                            } else if let Some(id) = self.pane_at(cx, cy) {
                                let label = self
                                    .pw
                                    .tabs
                                    .get(self.pw.active_tab)
                                    .and_then(|tab| tab.root.as_ref())
                                    .and_then(|root| find_pane(root, id))
                                    .map(pane_label)
                                    .unwrap_or_else(|| FALLBACK_LABEL.to_string());
                                if let Some(source_window) = self.pw.window.as_ref().map(|window| window.id()) {
                                    self.pane_drag = Some(PaneDrag {
                                        source_window,
                                        source_tab: self.pw.active_tab,
                                        pane: id,
                                        start: self.pw.cursor,
                                        screen: None,
                                        target: None,
                                        moved: false,
                                        left_window: false,
                                        label,
                                    });
                                    #[cfg(target_os = "linux")]
                                    self.begin_kwin_drag_probe();
                                }
                                self.focus_pane_at(cx, cy);
                            }
                        }
                        ElementState::Released => {
                            self.drag_divider = None;
                        }
                    }
                    return;
                }
                // ctrl+click opens a web link under the cursor (before any TUI
                // forwarding, so it works inside mouse-reporting apps too)
                if state == ElementState::Pressed && self.mods.control_key()
                    && let Some((_, _, _, url)) = self.focused_url_at(cx, cy) {
                        win::open_url(&url);
                        return;
                    }
                // a click on a plugin dock widget notifies the owning plugin
                if state == ElementState::Pressed {
                    let di = self.pw.renderer.as_ref().and_then(|r| r.widget_at(cx, cy));
                    if let Some(di) = di
                        && let Some((pidx, wid, _)) = self.plugin_widgets.get(di)
                    {
                        let (pidx, id) = (*pidx, wid.clone());
                        if let Some(p) = self.plugins.get_mut(pidx) {
                            p.send(&plugin::HostEvent::WidgetClicked { id });
                        }
                        return;
                    }
                }
                // forward a press to a TUI with mouse reporting on (Shift bypasses
                // for manual selection); release is finalized at the top of the arm
                if matches!(hit, Some(Hit::Content))
                    && !self.mods.shift_key()
                    && state == ElementState::Pressed
                    && let Some(id) = self.pane_at(cx, cy)
                        && self.report_to_pane(id, 0, true, false) {
                            self.focus_pane_at(cx, cy);
                            self.mouse_down = Some((0, id));
                            return;
                        }
                // drag a split divider directly to resize it (no pane mode needed)
                if matches!(hit, Some(Hit::Content)) && !self.mods.shift_key() {
                    match state {
                        ElementState::Pressed => {
                            let found = if let (Some(content), Some(root)) = (
                                self.pw.renderer.as_ref().map(|r| r.content_rect()),
                                self.pw.tabs.get(self.pw.active_tab).and_then(|t| t.root.as_ref()),
                            ) {
                                let mut path = Vec::new();
                                find_divider(root, content, cx, cy, 6.0, &mut path)
                            } else {
                                None
                            };
                            if let Some(p) = found {
                                self.drag_divider = Some(p);
                                return;
                            }
                        }
                        ElementState::Released => {
                            if self.drag_divider.take().is_some() {
                                return;
                            }
                        }
                    }
                }
                match state {
                    ElementState::Pressed => match hit {
                        Some(Hit::Button(h)) => {
                            self.pressed = Some(h);
                            // a tab activates on press (like every tab strip) and
                            // can then be dragged along the strip to reorder
                            if let Hot::Tab(i) = h {
                                let label = self.pw.tabs.get(i).map(tab_label).unwrap_or_default();
                                self.switch_tab(i);
                                if let Some(window) = self.pw.window.as_ref() {
                                    let cursor = self.pw.cursor;
                                    self.tab_drag = Some(TabDrag {
                                        source: window.id(),
                                        index: i,
                                        start: cursor,
                                        screen: window.inner_position().ok().map(|origin| {
                                            PhysicalPosition::new(
                                                origin.x + cursor.x.round() as i32,
                                                origin.y + cursor.y.round() as i32,
                                            )
                                        }),
                                        target: None,
                                        left_strip: false,
                                        left_window: false,
                                        label,
                                    });
                                    #[cfg(target_os = "linux")]
                                    self.begin_kwin_drag_probe();
                                }
                            }
                        }
                        Some(Hit::Content) => {
                            self.focus_pane_at(cx, cy);
                            // shift-click extends an existing selection in the
                            // clicked pane to that cell (the usual anchor-extend)
                            if self.mods.shift_key()
                                && let Some((row, col)) = self.cell_in_focused(cx, cy)
                                && let Some((abs, reflow_gen)) =
                                    self.focused_grid().map(|g| (g.viewport_to_abs(row), g.reflow_gen))
                                && let Some(sel) = self
                                    .selection
                                    .as_mut()
                                    .filter(|s| {
                                        Some(s.pane)
                                            == self.pw.tabs.get(self.pw.active_tab).map(|t| t.focused)
                                            && s.reflow_gen == reflow_gen
                                    })
                            {
                                sel.end = (abs, col);
                                self.selecting = true;
                                self.redraw();
                                return;
                            }
                            let now = Instant::now();
                            // cycle 1=char, 2=word, 3=line on rapid clicks in place
                            let consecutive = self
                                .last_click
                                .map(|(t, lx, ly)| {
                                    now.duration_since(t) < Duration::from_millis(400)
                                        && (lx - cx as f64).abs() < 6.0
                                        && (ly - cy as f64).abs() < 6.0
                                })
                                .unwrap_or(false);
                            self.click_seq = if consecutive { (self.click_seq % 3) + 1 } else { 1 };
                            self.last_click = Some((now, cx as f64, cy as f64));
                            if let (Some(pane), Some((row, col))) =
                                (self.active_focused_id(), self.cell_in_focused(cx, cy))
                                && let Some(grid) = self
                                    .pw.tabs
                                    .get(self.pw.active_tab)
                                    .and_then(|t| t.root.as_ref())
                                    .and_then(|r| find_pane(r, pane))
                                    .map(|p| &p.term.grid)
                            {
                                let abs = grid.viewport_to_abs(row);
                                let reflow_gen = grid.reflow_gen;
                                match self.click_seq {
                                    2 => {
                                        let (lo, hi) = grid.word_bounds(row, col);
                                        self.selection = Some(Sel {
                                            pane,
                                            start: (abs, lo),
                                            end: (abs, hi),
                                            block: false,
                                            reflow_gen,
                                        });
                                        self.selecting = false;
                                        if self.config.copy_on_select {
                                            self.copy_selection();
                                        }
                                    }
                                    3 => {
                                        let hi = grid.line_last_col(row);
                                        self.selection = Some(Sel {
                                            pane,
                                            start: (abs, 0),
                                            end: (abs, hi),
                                            block: false,
                                            reflow_gen,
                                        });
                                        self.selecting = false;
                                        if self.config.copy_on_select {
                                            self.copy_selection();
                                        }
                                    }
                                    _ => {
                                        self.selection = Some(Sel {
                                            pane,
                                            start: (abs, col),
                                            end: (abs, col),
                                            // alt+drag selects a rectangle
                                            block: self.mods.alt_key(),
                                            reflow_gen,
                                        });
                                        self.selecting = true;
                                    }
                                }
                            }
                            self.redraw();
                        }
                        Some(Hit::TitleBar) => {
                            // double-click the empty title bar opens a new tab
                            let now = Instant::now();
                            let dbl = self
                                .last_click
                                .map(|(t, lx, ly)| {
                                    now.duration_since(t) < Duration::from_millis(400)
                                        && (lx - cx as f64).abs() < 6.0
                                        && (ly - cy as f64).abs() < 6.0
                                })
                                .unwrap_or(false);
                            if dbl {
                                self.last_click = None;
                                self.new_tab();
                            } else {
                                self.last_click = Some((now, cx as f64, cy as f64));
                                if let Some(w) = &self.pw.window {
                                    let _ = w.drag_window();
                                }
                            }
                        }
                        Some(Hit::Resize(dir)) => {
                            if let Some(w) = &self.pw.window {
                                let _ = w.drag_resize_window(dir);
                            }
                        }
                        _ => {}
                    },
                    ElementState::Released => {
                        self.selecting = false;
                        self.sel_autoscroll = None;
                        // a plain click (no drag) clears the selection; a real drag
                        // auto-copies when copy-on-select is enabled
                        if let Some(sel) = self.selection {
                            if sel.start == sel.end {
                                self.selection = None;
                                self.redraw();
                            } else if self.config.copy_on_select {
                                self.copy_selection();
                            }
                        }
                        // tabs already switched on press, so a tab release is a no-op
                        if let Some(h) = self.pressed.take()
                            && matches!(hit, Some(Hit::Button(hh)) if hh == h)
                            && !matches!(h, Hot::Tab(_)) {
                                self.button_action(event_loop, h);
                            }
                    }
                }
            }
            _ => {}
        }
    }

    /// put a program notification's text on the status bar for a few seconds;
    /// lands on whichever window's renderer is swapped in
    fn show_notice(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_notice(Some(text.to_string()));
        }
        self.notice_until = Some(Instant::now() + Duration::from_secs(5));
        self.redraw();
    }

    /// push the merged OSC 9;4 progress of every pane in this window onto the
    /// taskbar button; the platform call is skipped while the value is unchanged
    fn sync_taskbar_progress(&mut self) {
        let mut agg = (0u8, 0u8);
        for tab in &self.pw.tabs {
            if let Some(root) = tab.root.as_ref() {
                each_pane(root, &mut |p| {
                    agg = merge_progress(agg, p.term.progress.unwrap_or((0, 0)));
                });
            }
        }
        if agg == self.taskbar_sent {
            return;
        }
        self.taskbar_sent = agg;
        if let Some(w) = self.pw.window.as_ref() {
            win::set_taskbar_progress(w, agg.0, agg.1);
        }
    }

    /// change the content font size: d>0 bigger, d<0 smaller, d==0 reset to default
    fn nudge_font(&mut self, d: f32) {
        if let Some(r) = self.pw.renderer.as_mut() {
            let pt = if d == 0.0 { CONTENT_PT } else { r.content_pt() + d };
            r.set_content_pt(pt);
        }
        self.relayout_all();
        self.redraw();
        self.save_config();
    }

    /// move focus to the nearest pane in direction (dx, dy)
    fn focus_dir(&mut self, dx: i32, dy: i32) {
        let Some(cur) = self.active_focused_id() else {
            return;
        };
        let Some((_, (cx0, cy0, cw, ch))) = self.pw.layout_cache.iter().find(|(id, _)| *id == cur)
        else {
            return;
        };
        let (cx, cy) = (cx0 + cw / 2.0, cy0 + ch / 2.0);
        let mut best: Option<(usize, f32)> = None;
        for (id, (x, y, w, h)) in &self.pw.layout_cache {
            if *id == cur {
                continue;
            }
            let (px, py) = (x + w / 2.0, y + h / 2.0);
            let in_dir = (dx > 0 && px > cx)
                || (dx < 0 && px < cx)
                || (dy > 0 && py > cy)
                || (dy < 0 && py < cy);
            if !in_dir {
                continue;
            }
            let dist = (px - cx).abs() + (py - cy).abs();
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((*id, dist));
            }
        }
        if let Some((id, _)) = best {
            let before = self.focus_identity();
            if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) {
                tab.focused = id;
            }
            // ease the accent border in on the newly focused pane
            self.focus_anim = Some(Instant::now());
            self.sync_tabs();
            self.after_focus_context_change(before);
        }
    }

    /// intercept chrome shortcuts; returns true if consumed
    // takes the key event's fields rather than winit's KeyEvent (which can't
    // be constructed) so synthetic --drive keys walk the identical path
    fn handle_shortcut(&mut self, logical: &Key, text: Option<&str>, state: ElementState, event_loop: &ActiveEventLoop) -> bool {
        if state != ElementState::Pressed {
            return false;
        }
        if *logical == Key::Named(NamedKey::Escape)
            && (self.tab_drag.take().is_some() || self.pane_drag.take().is_some())
        {
            self.show_tab_drop(None);
            self.show_pane_drop(None);
            self.show_drag_preview(None);
            self.reset_drag_cursors();
            #[cfg(target_os = "linux")]
            self.cancel_kwin_drag_probe();
            self.pressed = None;
            return true;
        }
        // Esc closes an open pane context menu before anything else sees it
        if self.pw.pane_menu.is_some() && *logical == Key::Named(NamedKey::Escape) {
            self.pw.pane_menu = None;
            self.redraw();
            return true;
        }
        // a modal confirm prompt captures every key while open: enter runs the
        // held action, esc cancels, anything else is swallowed (no accidental
        // dismissal and no leakage to the pane underneath)
        if self.pw.confirm.is_some() {
            match logical {
                Key::Named(NamedKey::Enter) => {
                    if let Some(c) = self.pw.confirm.take() {
                        self.run_confirm(c.action, event_loop);
                    }
                }
                Key::Named(NamedKey::Escape) => self.pw.confirm = None,
                _ => {}
            }
            self.redraw();
            return true;
        }
        // tab rename text field: enter commits (an empty name clears back to the
        // cwd label), esc cancels, the rest edits the buffer
        if self.pw.rename.is_some() {
            match logical {
                Key::Named(NamedKey::Enter) => {
                    if let Some(rs) = self.pw.rename.take() {
                        let name = rs.buf.trim().to_string();
                        if let Some(tab) = self.pw.tabs.get_mut(rs.tab) {
                            tab.title = (!name.is_empty()).then_some(name);
                        }
                        self.sync_tabs();
                    }
                }
                Key::Named(NamedKey::Escape) => self.pw.rename = None,
                Key::Named(NamedKey::Backspace) => {
                    if let Some(rs) = self.pw.rename.as_mut() {
                        rs.buf.pop();
                    }
                }
                _ => {
                    if !self.mods.control_key()
                        && let Some(t) = text
                        && !t.is_empty()
                        && !t.chars().any(|c| c.is_control())
                    {
                        let t = t.to_string();
                        if let Some(rs) = self.pw.rename.as_mut() {
                            rs.buf.push_str(&t);
                        }
                    }
                }
            }
            self.redraw();
            return true;
        }
        // user keybindings take precedence over the built-in shortcuts, but
        // never over an open overlay or pane mode
        if !self.keybindings.is_empty()
            && self.market.is_none()
            && self.find.is_none()
            && self.palette.is_none()
            && self.font_pick.is_none()
            && !self.pw.settings_open
            && !self.pw.pane_mode
            && self.mark.is_none()
        {
            let mods = self.mods;
            let act = self
                .keybindings
                .iter()
                .find(|(m, k, _)| *m == mods && key_matches(logical, k))
                .map(|(_, _, a)| *a);
            if let Some(a) = act {
                // run_action returns false only for prompt-jump with no marks, so
                // that key falls through to the program unchanged
                return self.run_action(a, event_loop);
            }
        }
        // ctrl+c with an active selection copies it (and clears the selection) the
        // way windows terminal does; with no selection it falls through so the
        // shell still receives the interrupt. ctrl+shift+c stays the unconditional
        // copy chord
        if self.market.is_none()
            && self.find.is_none()
            && self.palette.is_none()
            && self.font_pick.is_none()
            && !self.pw.settings_open
            && !self.pw.pane_mode
            && self.mark.is_none()
            && self.mods.control_key()
            && !self.mods.shift_key()
            && !self.mods.alt_key()
            && self.selection.is_some()
            && key_matches(logical, &Key::Character("c".into()))
        {
            self.copy_selection();
            self.selection = None;
            self.redraw();
            return true;
        }
        // the plugins marketplace overlay captures keys while open
        if self.market.is_some() && self.market_input(logical) {
            return true;
        }
        // find-in-scrollback overlay captures every key while open
        if self.find.is_some() {
            match logical {
                Key::Named(NamedKey::Escape) => {
                    self.find = None;
                    self.redraw();
                }
                Key::Named(NamedKey::Enter) => {
                    self.find_step(!self.mods.shift_key());
                }
                Key::Named(NamedKey::ArrowDown) => self.find_step(true),
                Key::Named(NamedKey::ArrowUp) => self.find_step(false),
                Key::Named(NamedKey::Backspace) => {
                    if let Some(f) = self.find.as_mut() {
                        f.query.pop();
                    }
                    self.find_recompute();
                }
                // Alt+R mirrors the .* button (alt keeps plain 'r' typeable)
                Key::Character(c) if self.mods.alt_key() && c.eq_ignore_ascii_case("r") => {
                    self.find_regex = !self.find_regex;
                    self.find_recompute();
                }
                _ => {
                    if !self.mods.control_key()
                        && !self.mods.alt_key()
                        && let Some(t) = text
                            && !t.is_empty() && !t.chars().any(|c| c.is_control()) {
                                let t = t.to_string();
                                if let Some(f) = self.find.as_mut() {
                                    f.query.push_str(&t);
                                }
                                self.find_recompute();
                            }
                }
            }
            return true;
        }
        // the font picker captures every key while open (preview live, commit
        // on Enter, cancel on Esc — reuses the palette overlay box)
        if self.font_pick.is_some() {
            match logical {
                Key::Named(NamedKey::Escape) => self.close_font_picker(false),
                Key::Named(NamedKey::Enter) => self.close_font_picker(true),
                Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::ArrowUp) => {
                    let down = matches!(logical, Key::Named(NamedKey::ArrowDown));
                    let len = self
                        .font_pick
                        .as_ref()
                        .map(|p| self.font_pick_filter(&p.query).len())
                        .unwrap_or(0);
                    if let Some(p) = self.font_pick.as_mut()
                        && len > 0
                    {
                        p.selected = if down {
                            (p.selected + 1) % len
                        } else {
                            (p.selected + len - 1) % len
                        };
                    }
                    self.font_pick_preview();
                }
                Key::Named(NamedKey::Backspace) => {
                    if let Some(p) = self.font_pick.as_mut() {
                        p.query.pop();
                        p.selected = 0;
                    }
                    self.font_pick_preview();
                }
                _ => {
                    if !self.mods.control_key()
                        && !self.mods.alt_key()
                        && let Some(t) = text
                        && !t.is_empty()
                        && !t.chars().any(|c| c.is_control())
                    {
                        let t = t.to_string();
                        if let Some(p) = self.font_pick.as_mut() {
                            p.query.push_str(&t);
                            p.selected = 0;
                        }
                        self.font_pick_preview();
                    }
                }
            }
            return true;
        }
        // command palette captures every key while open
        if self.palette.is_some() {
            match logical {
                Key::Named(NamedKey::Escape) => {
                    self.palette = None;
                    self.redraw();
                }
                Key::Named(NamedKey::Enter) => {
                    let (mode, q, sel) = self
                        .palette
                        .as_ref()
                        .map(|p| (p.mode, p.query.clone(), p.selected))
                        .unwrap_or((PaletteMode::Commands, String::new(), 0));
                    self.palette = None;
                    self.run_palette_choice(mode, &q, sel, event_loop);
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let (mode, query) = self
                        .palette
                        .as_ref()
                        .map(|p| (p.mode, p.query.clone()))
                        .unwrap_or((PaletteMode::Commands, String::new()));
                    let len = self.palette_choices(mode, &query).len();
                    if let Some(p) = self.palette.as_mut()
                        && len > 0 {
                            p.selected = (p.selected + 1) % len;
                        }
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    let (mode, query) = self
                        .palette
                        .as_ref()
                        .map(|p| (p.mode, p.query.clone()))
                        .unwrap_or((PaletteMode::Commands, String::new()));
                    let len = self.palette_choices(mode, &query).len();
                    if let Some(p) = self.palette.as_mut()
                        && len > 0 {
                            p.selected = (p.selected + len - 1) % len;
                        }
                    self.redraw();
                }
                Key::Named(NamedKey::Backspace) => {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.pop();
                        p.selected = 0;
                    }
                    self.redraw();
                }
                _ => {
                    if !self.mods.control_key()
                        && let Some(t) = text
                            && !t.is_empty() && !t.chars().any(|c| c.is_control()) {
                                let t = t.to_string();
                                if let Some(p) = self.palette.as_mut() {
                                    p.query.push_str(&t);
                                    p.selected = 0;
                                }
                                self.redraw();
                            }
                }
            }
            return true;
        }
        // pane control mode is a toggle: the same chord (Ctrl+Shift+P) enters
        // and exits it, so it stays on until you deliberately turn it off
        if self.mods.control_key()
            && self.mods.shift_key()
            && self.mark.is_none()
            && matches!(logical, Key::Character(c) if c.eq_ignore_ascii_case("p"))
        {
            self.set_pane_mode(!self.pw.pane_mode);
            return true;
        }
        // pane control mode captures every key until exited
        if self.pw.pane_mode {
            match logical {
                Key::Named(NamedKey::Escape) => self.set_pane_mode(false),
                // shift+arrows resize the focused pane; plain arrows move focus
                Key::Named(NamedKey::ArrowLeft) if self.mods.shift_key() => self.resize_focused(Dir::Vertical, false),
                Key::Named(NamedKey::ArrowRight) if self.mods.shift_key() => self.resize_focused(Dir::Vertical, true),
                Key::Named(NamedKey::ArrowUp) if self.mods.shift_key() => self.resize_focused(Dir::Horizontal, false),
                Key::Named(NamedKey::ArrowDown) if self.mods.shift_key() => self.resize_focused(Dir::Horizontal, true),
                Key::Named(NamedKey::ArrowLeft) => self.focus_dir(-1, 0),
                Key::Named(NamedKey::ArrowRight) => self.focus_dir(1, 0),
                Key::Named(NamedKey::ArrowUp) => self.focus_dir(0, -1),
                Key::Named(NamedKey::ArrowDown) => self.focus_dir(0, 1),
                Key::Character(c) => match c.to_ascii_lowercase().as_str() {
                    "h" => self.focus_dir(-1, 0),
                    "l" => self.focus_dir(1, 0),
                    "k" => self.focus_dir(0, -1),
                    "j" => self.focus_dir(0, 1),
                    "v" => self.split_focused(Dir::Vertical),
                    "s" => self.split_focused(Dir::Horizontal),
                    "x" => self.close_focused_pane(event_loop),
                    "o" => self.pop_out_focused(event_loop),
                    "z" => self.toggle_zoom(),
                    "n" => self.new_tab(),
                    "q" => self.set_pane_mode(false),
                    _ => {}
                },
                _ => {}
            }
            return true;
        }
        // mark mode captures every key until exited: keyboard selection over
        // screen + scrollback without touching the mouse
        if self.mark.is_some() {
            self.mark_key(logical);
            return true;
        }

        // the settings panel captures keys while open (Esc or Ctrl+, closes it)
        if self.pw.settings_open {
            let esc = *logical == Key::Named(NamedKey::Escape);
            let ctrl_comma = self.mods.control_key()
                && matches!(logical, Key::Character(c) if c.as_str() == ",");
            if esc || ctrl_comma {
                self.close_settings();
            }
            return true;
        }
        // every built-in chord now lives in the keybindings table (seeded by
        // default_keybindings and dispatched by the gate above), so anything
        // reaching here is unbound and falls through to the focused program
        false
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.pw.window.is_some() {
            return;
        }
        if let Err(e) = self.boot(event_loop) {
            log::error!("failed to start termie: {e:#}");
            // show why instead of vanishing — boot failure is almost always gpu
            // init, and a window that silently never appears looks like a hang
            win::show_fatal_error(&format!("{e:#}"));
            win::clipboard_shutdown();
            self.release_window_resources();
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Pty { id, bytes } => {
                let mut responses: Option<Vec<u8>> = None;
                let mut clip: Option<String> = None;
                let mut color_reqs: Vec<term::ColorReq> = Vec::new();
                let mut found = false;
                let mut in_sync = false;
                let mut rang = false;
                let mut rang_tab: Option<usize> = None;
                let mut note: Option<String> = None;
                let mut newly_ready = false;
                let mut cwd_changed = false;
                let mut status_changed = false;
                let mut state_event: Option<plugin::HostEvent> = None;
                for (ti, tab) in self.pw.tabs.iter_mut().enumerate() {
                    if let Some(root) = tab.root.as_mut()
                        && let Some(p) = find_pane_mut(root, id) {
                            pump_bytes(p, &bytes);
                            // first output means the shell has settled past its
                            // PSReadLine startup, so it's now safe to resize
                            if !p.ready {
                                p.ready = true;
                                newly_ready = true;
                                // a --drive clock starts at the first output so
                                // scripts never race a cold shell
                                if let Some(d) = self.drive.as_mut()
                                    && d.started.is_none()
                                {
                                    d.started = Some(Instant::now());
                                }
                            }
                            if p.term.cwd_dirty {
                                p.term.cwd_dirty = false;
                                cwd_changed = true;
                            }
                            if p.term.title_dirty {
                                p.term.title_dirty = false;
                                cwd_changed = true;
                            }
                            // derive the pane's command badge from OSC 133; a
                            // command finishing in the viewed pane is already
                            // seen, so it never earns a done badge
                            let viewed =
                                self.pw.focused && ti == self.pw.active_tab && tab.focused == id;
                            let status = if p.term.cmd_running {
                                p.term.cmd_done = None;
                                PaneStatus::Running
                            } else if let Some(code) = p.term.cmd_done.take() {
                                if viewed { PaneStatus::Idle } else { PaneStatus::Done(code) }
                            } else if p.status == PaneStatus::Running {
                                // the C..D window closed without a done event
                                PaneStatus::Idle
                            } else {
                                p.status
                            };
                            if status != p.status {
                                p.status = status;
                                status_changed = true;
                                state_event = Some(pane_state_event(id, status, &p.term.title));
                            }
                            in_sync = p.term.sync_output;
                            if !p.term.responses.is_empty() {
                                responses = Some(std::mem::take(&mut p.term.responses));
                            }
                            if let Some(text) = p.term.clipboard.take() {
                                clip = Some(text);
                            }
                            if !p.term.color_queries.is_empty() {
                                color_reqs = std::mem::take(&mut p.term.color_queries);
                            }
                            if p.term.bell {
                                p.term.bell = false;
                                p.flash = Some(Instant::now());
                                rang = true;
                                rang_tab = Some(ti);
                            }
                            if let Some(text) = p.term.notify.take() {
                                note = Some(text);
                            }
                            found = true;
                            break;
                        }
                }
                if !found
                    && let Some(idx) = self.satellite_with_pane(id)
                {
                    found = true;
                    // run the full pump path against the torn-off window: same
                    // color-query answering, cwd relabel, deferred-resize and bell
                    // handling the main window gets — just scoped to this window's
                    // own renderer/palette/tabs via the swap
                    self.cur_sat = Some(idx);
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    let mut sat_responses: Option<Vec<u8>> = None;
                    let mut sat_color: Vec<term::ColorReq> = Vec::new();
                    let mut sat_ready = false;
                    let mut sat_cwd = false;
                    let mut sat_status = false;
                    let mut sat_state_event: Option<plugin::HostEvent> = None;
                    let mut sat_rang = false;
                    let mut sat_rang_tab: Option<usize> = None;
                    let mut sat_note: Option<String> = None;
                    for (ti, tab) in self.pw.tabs.iter_mut().enumerate() {
                        if let Some(root) = tab.root.as_mut()
                            && let Some(p) = find_pane_mut(root, id)
                        {
                            pump_bytes(p, &bytes);
                            if !p.ready {
                                p.ready = true;
                                sat_ready = true;
                            }
                            if p.term.cwd_dirty {
                                p.term.cwd_dirty = false;
                                sat_cwd = true;
                            }
                            if p.term.title_dirty {
                                p.term.title_dirty = false;
                                sat_cwd = true;
                            }
                            // same badge derivation as the main window, scoped
                            // to the swapped-in satellite's focus state
                            let viewed =
                                self.pw.focused && ti == self.pw.active_tab && tab.focused == id;
                            let status = if p.term.cmd_running {
                                p.term.cmd_done = None;
                                PaneStatus::Running
                            } else if let Some(code) = p.term.cmd_done.take() {
                                if viewed { PaneStatus::Idle } else { PaneStatus::Done(code) }
                            } else if p.status == PaneStatus::Running {
                                PaneStatus::Idle
                            } else {
                                p.status
                            };
                            if status != p.status {
                                p.status = status;
                                sat_status = true;
                                sat_state_event = Some(pane_state_event(id, status, &p.term.title));
                            }
                            if !p.term.responses.is_empty() {
                                sat_responses = Some(std::mem::take(&mut p.term.responses));
                            }
                            if let Some(text) = p.term.clipboard.take() {
                                clip = Some(text);
                            }
                            if !p.term.color_queries.is_empty() {
                                sat_color = std::mem::take(&mut p.term.color_queries);
                            }
                            if p.term.bell {
                                p.term.bell = false;
                                p.flash = Some(Instant::now());
                                sat_rang = true;
                                sat_rang_tab = Some(ti);
                            }
                            if let Some(text) = p.term.notify.take() {
                                sat_note = Some(text);
                            }
                            break;
                        }
                    }
                    if !sat_color.is_empty()
                        && let Some(rend) = self.pw.renderer.as_ref()
                    {
                        let pal = rend.palette();
                        let mut buf = sat_responses.take().unwrap_or_default();
                        for q in &sat_color {
                            buf.extend_from_slice(&term::format_color_reply(*q, pal));
                        }
                        sat_responses = Some(buf);
                    }
                    if let Some(r) = sat_responses {
                        for tab in self.pw.tabs.iter_mut() {
                            if let Some(root) = tab.root.as_mut()
                                && let Some(p) = find_pane_mut(root, id)
                            {
                                p.pty.write(&r);
                                break;
                            }
                        }
                    }
                    if sat_ready {
                        self.relayout_all();
                    }
                    if sat_cwd || sat_status {
                        self.sync_tabs();
                    }
                    // the swapped-in renderer is the satellite's own, so the
                    // notice lands on that window's status bar
                    if let Some(text) = sat_note {
                        self.show_notice(&text);
                    }
                    // same bell routing as the main window, scoped to this
                    // satellite's tabs and taskbar button via the swap
                    if let Some(ti) = sat_rang_tab {
                        if ti != self.pw.active_tab
                            && let Some(t) = self.pw.tabs.get_mut(ti)
                            && !t.attention
                        {
                            t.attention = true;
                            self.sync_tabs();
                        }
                        if !self.pw.focused
                            && let Some(w) = self.pw.window.as_ref()
                        {
                            win::request_attention(w);
                        }
                    }
                    if let Some(w) = self.pw.window.as_ref() {
                        w.request_redraw();
                    }
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    self.cur_sat = None;
                    if sat_rang && !self.plugins.is_empty() {
                        self.plugins_broadcast(&plugin::HostEvent::Bell { pane: id as u64 });
                    }
                    if let Some(ev) = sat_state_event
                        && !self.plugins.is_empty()
                    {
                        self.plugins_broadcast_gated("read_output", &ev);
                    }
                }
                // a pane that just became ready may need its deferred resize
                if newly_ready {
                    self.relayout_all();
                    // --kitty-demo injects AFTER the deferred resize: the
                    // column-change reflow clears image placements, so a
                    // gradient placed on the raw first output wouldn't survive
                    if self.kitty_demo_pending {
                        self.kitty_demo_pending = false;
                        let demo = kitty_demo_bytes();
                        if let Some(p) = self
                            .pw.tabs
                            .get_mut(self.pw.active_tab)
                            .and_then(|t| t.root.as_mut())
                            .and_then(|r| find_pane_mut(r, id))
                        {
                            pump_bytes(p, &demo);
                        }
                        self.redraw();
                    }
                }
                // let plugins react to the bell (host -> plugin event direction)
                if rang && !self.plugins.is_empty() {
                    self.plugins_broadcast(&plugin::HostEvent::Bell { pane: id as u64 });
                }
                // pane badge flips go to plugins that hold read_output — the
                // feed a status-panel plugin builds on
                if let Some(ev) = state_event
                    && !self.plugins.is_empty()
                {
                    self.plugins_broadcast_gated("read_output", &ev);
                }
                // a notification's text shows in the status bar for a few seconds
                if let Some(text) = note {
                    self.show_notice(&text);
                }
                // route the bell to the user: dot a background tab, flash the
                // taskbar of an unfocused window (viewing the tab clears both)
                if let Some(ti) = rang_tab {
                    if ti != self.pw.active_tab
                        && let Some(t) = self.pw.tabs.get_mut(ti)
                        && !t.attention
                    {
                        t.attention = true;
                        self.sync_tabs();
                        self.redraw();
                    }
                    if !self.pw.focused
                        && let Some(w) = self.pw.window.as_ref()
                    {
                        win::request_attention(w);
                    }
                }
                if !found {
                    // route to a warm pool shell; first output means it's started,
                    // so it's now safe to size it to a full content pane
                    let (ccols, crows) = self.content_pane_size();
                    if let Some(sp) = self.pool.iter_mut().find(|sp| sp.id == id) {
                        pump_bytes(sp, &bytes);
                        sp.ready = true;
                        if sp.term.grid.cols != ccols || sp.term.grid.rows != crows {
                            sp.resize(crows, ccols);
                        }
                        if !sp.term.responses.is_empty() {
                            responses = Some(std::mem::take(&mut sp.term.responses));
                        }
                        if let Some(text) = sp.term.clipboard.take() {
                            clip = Some(text);
                        }
                        if !sp.term.color_queries.is_empty() {
                            color_reqs = std::mem::take(&mut sp.term.color_queries);
                        }
                    }
                }
                if let Some(t) = clip {
                    win::clipboard_set(&t);
                    // OSC 52 is useful (nvim/tmux yank) but it is a program
                    // writing the user's clipboard — say so on the status bar
                    self.show_notice("clipboard set by program");
                }
                // answer OSC 4/10/11/12 color queries from the active palette
                if !color_reqs.is_empty()
                    && let Some(rend) = self.pw.renderer.as_ref()
                {
                    let pal = rend.palette();
                    let mut buf = responses.take().unwrap_or_default();
                    for q in &color_reqs {
                        buf.extend_from_slice(&term::format_color_reply(*q, pal));
                    }
                    responses = Some(buf);
                }
                if let Some(r) = responses {
                    let mut wrote = false;
                    for tab in &mut self.pw.tabs {
                        if let Some(root) = tab.root.as_mut()
                            && let Some(p) = find_pane_mut(root, id) {
                                p.pty.write(&r);
                                wrote = true;
                                break;
                            }
                    }
                    if !wrote
                        && let Some(sp) = self.pool.iter_mut().find(|sp| sp.id == id) {
                            sp.pty.write(&r);
                        }
                }
                // relabel tabs only when a tab pane's cwd or title actually
                // changed; a status flip also refreshes the strip's badges
                if cwd_changed || status_changed {
                    self.sync_tabs();
                }
                if status_changed {
                    self.redraw();
                }
                if self.pw.layout_cache.iter().any(|(pid, _)| *pid == id) {
                    if in_sync {
                        // mid synchronized-output frame: defer the paint so the
                        // screen isn't shown torn (cursor stranded mid-redraw)
                        if self.sync_redraw_pending.is_none() {
                            self.sync_redraw_pending = Some(Instant::now());
                        }
                    } else {
                        // mark dirty and let about_to_wait paint once per loop
                        // turn, so a flood of pty chunks collapses to one frame
                        self.sync_redraw_pending = None;
                        self.pty_dirty = true;
                    }
                }
            }
            UserEvent::Exited { id } => {
                // a warm pool shell that died — drop it so warm_pool respawns
                self.pool.retain(|p| p.id != id);
                // a torn-off pane whose shell exited — close its satellite window
                if let Some(idx) = self.satellite_with_pane(id) {
                    // close only the exited pane (collapse the split / close its
                    // tab), not the whole torn-off window; remove the window only
                    // when it ends up empty
                    self.cur_sat = Some(idx);
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    // restore the previously-viewed tab after close so a
                    // background-tab exit doesn't yank the visible tab or
                    // poison find against the temporary owner
                    self.close_pane_keeping_viewer(id, event_loop);
                    std::mem::swap(&mut self.pw, &mut self.satellites[idx]);
                    self.cur_sat = None;
                    if self.satellites.get(idx).is_some_and(|s| s.tabs.is_empty()) {
                        self.satellites.remove(idx);
                    }
                    return;
                }
                // main window: same keep-viewer path so find stays on the
                // tab the user is looking at while a background shell dies
                self.close_pane_keeping_viewer(id, event_loop);
            }
            UserEvent::PaneReady(pane) => {
                self.pending_warm = self.pending_warm.saturating_sub(1);
                if let Some(mut pane) = pane {
                    // a shell spawned successfully: clear the failure backoff
                    self.warm_fails = 0;
                    self.warm_backoff_until = None;
                    // start reading now that we own it; refresh scrollback in case
                    // it changed while the shell was spawning
                    self.start_reader(&mut pane);
                    pane.term.grid.set_scrollback_limit(self.config.scrollback);
                    if self.pw.tabs.is_empty() {
                        // first shell of an async startup -> becomes tab one
                        self.install_first_tab(*pane);
                        timing("first shell on screen");
                    } else {
                        self.pool.push(*pane);
                    }
                } else {
                    // spawn failed: back off (growing) so a broken shell can't
                    // hot-loop, and give up after MAX_WARM_FAILS
                    self.warm_fails += 1;
                    let ms = (200 * self.warm_fails as u64).min(3000);
                    self.warm_backoff_until = Some(Instant::now() + Duration::from_millis(ms));
                    if self.warm_fails == MAX_WARM_FAILS {
                        log::error!(
                            "shell failed to start {MAX_WARM_FAILS} times; stopping spawn retries"
                        );
                    }
                }
            }
            UserEvent::Plugin { id, msg } => match msg {
                plugin::PluginMsg::Cmd(cmd) => self.handle_plugin_cmd(id, cmd),
                plugin::PluginMsg::Exited => {
                    // drop this plugin's widgets + bus subscriptions so a dead
                    // plugin doesn't linger. the slot in `plugins`/`plugin_ids`
                    // stays (indices must stay stable as bus/widget keys)
                    let before = self.plugin_widgets.len();
                    self.plugin_widgets.retain(|(p, _, _)| *p != id);
                    self.plugin_subs.retain(|(p, _)| *p != id);
                    if self.plugin_widgets.len() != before {
                        self.rebuild_dock();
                    }
                    log::info!("plugin {id} exited");
                }
            },
            UserEvent::Launch(request) => self.open_launch_window(event_loop, request),
            #[cfg(target_os = "linux")]
            UserEvent::KwinDragGeometry(snapshot) => self.accept_kwin_drag_geometry(snapshot),
            UserEvent::Market(result) => {
                // the remote catalog arrived (or failed): merge into the open overlay
                if self.market.is_some() {
                    match result {
                        Ok(catalog) => {
                            let empty = catalog.is_empty();
                            let rows = self.market_rows(&catalog);
                            if let Some(m) = self.market.as_mut() {
                                m.selected = m.selected.min(rows.len().saturating_sub(1));
                                m.rows = rows;
                                m.loading = false;
                                m.fetch_failed = false;
                                m.status = if empty {
                                    "the catalog has no plugins yet".to_string()
                                } else {
                                    String::new()
                                };
                            }
                        }
                        Err(e) => {
                            if let Some(m) = self.market.as_mut() {
                                m.loading = false;
                                m.fetch_failed = true;
                                m.status = e;
                            }
                        }
                    }
                    self.redraw();
                }
            }
            #[cfg(any(windows, target_os = "linux"))]
            UserEvent::ToggleQuake => self.toggle_quake(),
            UserEvent::UserConfChanged => {
                let (kb, sends, ignored) = load_keybindings();
                self.keybindings = kb;
                self.send_inputs = sends;
                // a live edit that introduces an error gets the same notice as boot
                if ignored > 0 {
                    let noun = if ignored == 1 { "line" } else { "lines" };
                    let msg = format!("keybindings.conf: {ignored} {noun} ignored");
                    self.show_notice(&msg);
                }
                let overrides = load_color_overrides();
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_color_overrides(overrides.clone());
                }
                for s in &mut self.satellites {
                    if let Some(r) = s.renderer.as_mut() {
                        r.set_color_overrides(overrides.clone());
                    }
                    if let Some(w) = s.window.as_ref() {
                        w.request_redraw();
                    }
                }
                self.redraw();
            }
            #[cfg(not(windows))]
            UserEvent::SystemThemeChanged(dark) => {
                self.system_dark = dark;
                if self.persisted.theme_auto {
                    self.apply_os_theme();
                }
            }
            #[cfg(windows)]
            UserEvent::Handoff(h) => self.handoff_tab(h),
            UserEvent::UpdateCheckDone(found, manual) => {
                match found {
                    Some(u) => {
                        // chip on the status bar; installing stays user-driven
                        if let Some(r) = self.main_pw_renderer() {
                            r.set_update(Some(u.version.clone()));
                        }
                        if manual {
                            let install = update::can_install();
                            self.pw.confirm = Some(ConfirmState {
                                prompt: if install {
                                    format!("install termie {} and restart?", u.version)
                                } else {
                                    format!("open the termie {} release page?", u.version)
                                },
                                hint: if install {
                                    "enter: update \u{b7} esc: not now".to_string()
                                } else {
                                    "enter: open \u{b7} esc: not now".to_string()
                                },
                                action: ConfirmAction::InstallUpdate,
                            });
                        } else {
                            let action = if update::can_install() { "update termie" } else { "open release page" };
                            self.show_notice(&format!("update {} available \u{2014} palette: {action}", u.version));
                        }
                        self.update = Some(u);
                        self.redraw();
                    }
                    None if manual => {
                        self.show_notice("termie is up to date");
                    }
                    None => {}
                }
            }
            UserEvent::UpdateDownloaded(result) => match result {
                Ok(path) => match update::run_setup(&path) {
                    // the installer takes over: update, relaunch, session restore
                    Ok(()) => self.quit_app(event_loop),
                    Err(e) => self.show_notice(&format!("update failed to start: {e}")),
                },
                Err(e) => self.show_notice(&format!("update download failed: {e}")),
            },
            UserEvent::Accessibility(e) => match e.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => self.update_window_a11y(e.window_id),
                // read-only v1: the screen reader can't drive actions
                accesskit_winit::WindowEvent::ActionRequested(_) => {}
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // events for a torn-off window go to its satellite handler
        if self.pw.window.as_ref().map(|w| w.id()) != Some(id) {
            if let Some(idx) = self.satellite_for(id) {
                self.satellite_event(idx, event, event_loop);
            }
            return;
        }
        // feed the main window's events to the accesskit adapter (focus/bounds)
        if let (Some(a), Some(w)) = (self.pw.a11y.as_mut(), self.pw.window.as_ref()) {
            a.process_event(w, &event);
        }
        match event {
            WindowEvent::CloseRequested => self.request_quit(event_loop),
            WindowEvent::ThemeChanged(_) => {
                // the windows light/dark setting flipped; under theme=auto
                // every window re-resolves and repaints
                if self.persisted.theme_auto {
                    self.apply_os_theme();
                }
            }
            WindowEvent::Focused(f) => {
                if f && let Some(window) = self.pw.window.as_ref() {
                    win::clear_attention(window);
                }
                // a --drive window paints focused no matter the real focus
                // (it is WS_EX_NOACTIVATE, so real focus never arrives)
                self.pw.focused = f || self.drive.is_some();
                // losing focus mid-composition must clear the IME flag, or a
                // missing Disabled/Commit (a real winit-on-Windows gap) would
                // leave every keystroke swallowed with no in-app recovery
                if !f {
                    self.pw.ime_composing = false;
                    self.pw.ime_preedit.clear();
                    self.pw.ime_preedit_caret = None;
                    self.release_held_input();
                }
                // coming back acknowledges the active tab's bell dot
                if f && self.pw.tabs.get(self.pw.active_tab).is_some_and(|t| t.attention) {
                    self.sync_tabs();
                }
                // a held drag can't survive losing focus: release it so the TUI
                // doesn't see a stuck button
                if !f
                    && let Some((btn, id)) = self.mouse_down.take() {
                        self.report_to_pane(id, btn, false, false);
                    }
                // report focus in/out to a pane that enabled mode 1004
                if let Some(id) = self.active_focused_id()
                    && let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut())
                        && let Some(p) = find_pane_mut(root, id)
                            && p.term.focus_events {
                                p.pty.write(if f { b"\x1b[I" } else { b"\x1b[O" });
                            }
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::CursorMoved { position, .. } => self.on_cursor_moved(position),
            WindowEvent::CursorEntered { .. } => self.on_cursor_entered(),
            WindowEvent::CursorLeft { .. } => self.on_cursor_left(),
            WindowEvent::MouseWheel { delta, .. } => self.on_mouse_wheel(delta),
            WindowEvent::MouseInput {
                state, button, ..
            } => self.on_mouse_input(state, button, event_loop),
            WindowEvent::Resized(size) => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
                // reflow on a width change moves cell coordinates, so a stale
                // selection would highlight the wrong cells
                self.selection = None;
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                // keep the surface crisp now, but defer the grid/pty reflow until
                // the drag settles so a live resize doesn't rebuild all scrollback
                // per pixel-step; about_to_wait fires relayout once it stops
                self.resize_settle = Some(Instant::now());
                // persist the new size for next launch (debounced like the layout)
                self.mark_session_dirty();
                self.redraw();
            }
            // moving the window persists its new position on the same debounce
            WindowEvent::Moved(_) => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
                self.mark_session_dirty();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(window) = self.pw.window.as_ref() {
                    constrain_window_to_monitor(window);
                }
                // monitor/dpi change: re-raster the atlas at the new scale so text
                // stays crisp. winit applies the os-suggested size and a Resized
                // follows (which arms the resize-settle reflow at the new size)
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_scale(scale_factor as f32);
                }
                self.relayout_all();
                self.redraw();
            }
            WindowEvent::Ime(ime) => self.on_ime(ime),
            WindowEvent::KeyboardInput { event, .. } => {
                // while composing, the IME owns text input; swallow only key
                // presses (releases must pass so kitty release-reporting + held
                // modifiers don't get stuck)
                if self.pw.ime_composing && event.state == ElementState::Pressed {
                    return;
                }
                if self.handle_shortcut(&event.logical_key, event.text.as_deref(), event.state, event_loop) {
                    return;
                }
                let id = match self.active_focused_id() {
                    Some(id) => id,
                    None => return,
                };
                let (app_cursor, kbd_flags) = self
                    .pw.tabs
                    .get(self.pw.active_tab)
                    .and_then(|t| t.root.as_ref())
                    .and_then(|r| find_pane(r, id))
                    .map(|p| (p.term.app_cursor_keys, p.term.kbd_flags()))
                    .unwrap_or((false, 0));
                if let Some(bytes) = input::key_to_bytes(
                    &event.logical_key,
                    event.text.as_deref(),
                    unshifted_char(&event),
                    event.state,
                    event.repeat,
                    self.mods,
                    event.location,
                    app_cursor,
                    kbd_flags,
                ) {
                    self.selection = None; // typing clears the selection
                    self.write_to_focused(&bytes);
                }
            }
            WindowEvent::DroppedFile(path) => self.on_dropped_file(&path),
            WindowEvent::RedrawRequested => self.paint(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // refresh the screen-reader tree (no-op unless an assistive tech is on)
        self.update_all_a11y();
        // top up the warm pool once the window is up (one per tick, no spawn burst)
        if self.shown {
            self.warm_pool();
            // warm the printable-ASCII glyph cache as soon as the window is up, so
            // the first shell output paints from a warm atlas instead of shaping
            // ~95 glyphs on the first content frames. the bundled content font is
            // already loaded, so this needs no system-font scan
            if !self.ascii_warmed {
                self.ascii_warmed = true;
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.prewarm_glyphs();
                }
            }
            // scan system fonts once, deferred until the first shell is on screen
            // so the prompt appears before this (event-loop-blocking) scan rather
            // than after it; enables the font picker + non-Latin fallbacks
            if self.system_fonts_pending && !self.pw.tabs.is_empty() {
                self.system_fonts_pending = false;
                let want = self.persisted.font.clone();
                let scanned = if let Some(r) = self.pw.renderer.as_mut() {
                    let s = r.ensure_system_fonts();
                    // a persisted system font couldn't resolve before the scan;
                    // apply it now that the db has it
                    if s
                        && let Some(f) = want.as_deref() {
                            r.set_font_by_name(f);
                            // the switch cleared the cache; re-warm for the new font
                            r.prewarm_glyphs();
                        }
                    s
                } else {
                    false
                };
                if scanned {
                    // font may have changed the grid size; repaint so the picker,
                    // re-rasterized fallbacks, and any font switch all show
                    self.relayout_all();
                    self.redraw();
                }
                timing("system fonts scanned");
            }
            // spawn enabled plugins once, deferred off the boot path so a window
            // with no/disabled plugins pays nothing at startup
            if !self.plugins_started {
                self.plugins_started = true;
                self.start_plugins();
            }
            // one deferred daily update check, entirely off-thread; failures
            // are silent (an update check must never bother anyone)
            if !self.update_checked && !self.pw.tabs.is_empty() {
                self.update_checked = true;
                if self.persisted.update_check && update::due() {
                    update::mark_checked();
                    let proxy = self.proxy.clone();
                    update::check(move |found| {
                        let _ = proxy.send_event(UserEvent::UpdateCheckDone(found, false));
                    });
                }
            }
        }
        // debug-only: drive TERMIE_BENCH auto-opens once the first shell + pool
        // are up, so warm-pool tab-open latency lands in the TERMIE_TIMING log
        #[cfg(debug_assertions)]
        if self.bench_left > 0 && !self.pw.tabs.is_empty() {
            match self.bench_next {
                None => self.bench_next = Some(Instant::now() + Duration::from_millis(600)),
                Some(t) if Instant::now() >= t => {
                    self.new_tab();
                    self.bench_left -= 1;
                    self.bench_next = Some(Instant::now() + Duration::from_millis(600));
                }
                _ => {}
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50)));
            return;
        }
        // fire any --drive steps that have come due (the flow tail below
        // schedules the wakeup for the next one)
        if self.drive.is_some() {
            self.drive_tick(event_loop);
        }
        // first shell hasn't spawned yet after a failure: wake at the backoff
        // deadline to retry, rather than hot-looping or sleeping indefinitely
        if self.pw.tabs.is_empty() && self.warm_fails > 0 && self.warm_fails < MAX_WARM_FAILS
            && let Some(t) = self.warm_backoff_until {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t));
                return;
            }
        // a synchronized-output frame is open: hold the paint until it closes,
        // but force one if it stalls (~100ms) so a crash mid-frame can't freeze us
        // flush the debounced session write once its deadline passes, so a crash
        // leaves session.json close to the live layout (clean exits flush directly)
        if let Some(t) = self.session_flush_at
            && Instant::now() >= t
        {
            self.session_flush_at = None;
            if self.session_dirty {
                self.session_dirty = false;
                self.write_session();
            }
        }
        if let Some(t) = self.sync_redraw_pending {
            if t.elapsed() >= Duration::from_millis(100) {
                self.sync_redraw_pending = None;
                self.redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t + Duration::from_millis(100)));
                return;
            }
        }
        // taskbar progress runs every turn (cheap, deduped) so a closed pane
        // clears its bar, and stays live while minimized — minimized is exactly
        // when the taskbar button is what the user sees
        if self.cur_sat.is_none() {
            self.sync_taskbar_progress();
        }
        // coalesced pty-output paint: one frame per loop turn no matter how many
        // pty chunks arrived since the last frame. inline_paint (experimental)
        // paints here directly, skipping the request_redraw sub-vsync hop; either
        // way it stays one present per loop turn
        if self.pty_dirty {
            // a minimized window has no pixels to update: skip the gpu frame
            // but leave the flag set, so the first turn after restore paints
            // the latest grid (restore always delivers focus/resize events)
            let minimized =
                self.pw.window.as_ref().and_then(|w| w.is_minimized()).unwrap_or(false);
            if !minimized {
                self.pty_dirty = false;
                if self.persisted.inline_paint {
                    self.paint();
                } else {
                    self.redraw();
                }
            }
        }
        // a resize drag is in flight: hold the grid/pty reflow until it settles
        // (~90ms of quiet), then reflow once instead of per pixel-step. this
        // branch also guarantees the loop wakes after the drag's last event
        if let Some(t) = self.resize_settle {
            if t.elapsed() >= Duration::from_millis(90) {
                self.resize_settle = None;
                self.relayout_all();
                self.redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t + Duration::from_millis(90)));
                return;
            }
        }
        // keep torn-off windows' animations (reveal/hover/tab-slide/overlay)
        // ticking — about_to_wait only drives self.pw, so a satellite animation
        // would otherwise stall until the next incidental event. their redraws
        // are requested here; the deadline is folded in at the idle block below
        // so the soonest of (main, satellite) wins even when the main is idle
        let sat_anim = !self.satellites.is_empty() && self.pump_satellite_redraws();
        // drag-selection autoscroll: while the pointer is held past the pane
        // edge, step the view and stretch the selection to the pointer ~33x/s
        if self.selecting
            && let Some(speed) = self.sel_autoscroll
        {
            if self.sel_scroll_at.map(|t| Instant::now() >= t).unwrap_or(true) {
                self.sel_scroll_at = Some(Instant::now() + Duration::from_millis(30));
                if let Some(g) = self.focused_grid_mut() {
                    g.scroll_view(speed);
                }
                let (cx, cy) = (self.pw.cursor.x as f32, self.pw.cursor.y as f32);
                if let (Some(mut sel), Some((row, col))) =
                    (self.selection, self.cell_in_focused(cx, cy))
                    && let Some((abs, reflow_gen)) =
                        self.focused_grid().map(|g| (g.viewport_to_abs(row), g.reflow_gen))
                    && sel.reflow_gen == reflow_gen
                {
                    sel.end = (abs, col);
                    self.selection = Some(sel);
                }
                self.redraw();
            }
            if let Some(t) = self.sel_scroll_at {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t));
            }
            return;
        } else {
            self.sel_scroll_at = None;
        }
        // startup reveal fade: drive it at ~60fps until it settles
        if self.pw.renderer.as_ref().map(|r| r.startup_fading()).unwrap_or(false) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            return;
        }
        // settings slide: redraw at ~60fps until the transition settles
        if let Some(t) = self.settings_anim {
            if t.elapsed().as_secs_f32() >= self.settings_anim_dur() {
                self.settings_anim = None;
            } else {
                self.redraw();
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(16),
                ));
                return;
            }
        }
        // focused-pane accent border ease: drive ~60fps until it settles, then
        // fall back to the idle (event-driven) cadence
        if let Some(t) = self.focus_anim {
            if t.elapsed().as_secs_f32() >= 0.16 {
                self.focus_anim = None;
            } else {
                self.redraw();
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(16),
                ));
                return;
            }
        }
        // chrome-button hover fade-in: drive ~60fps only while it's in flight
        if self.pw.renderer.as_ref().is_some_and(|r| r.hover_animating()) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            return;
        }
        // active-tab accent rail slide: drive ~60fps only while it's in flight
        if self.pw.renderer.as_ref().is_some_and(|r| r.tab_animating()) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            return;
        }
        // overlay (palette/find/market/menu) bloom-in: drive while in flight
        if self.pw.renderer.as_ref().is_some_and(|r| r.overlay_animating()) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            return;
        }
        // a status-bar notification readout expires after a few seconds; clear
        // it everywhere (a satellite's pump may have set it on its own bar)
        if let Some(t) = self.notice_until
            && Instant::now() >= t
        {
            self.notice_until = None;
            if let Some(r) = self.pw.renderer.as_mut() {
                r.set_notice(None);
            }
            for sat in &mut self.satellites {
                if let Some(r) = sat.renderer.as_mut() {
                    r.set_notice(None);
                }
                if let Some(w) = sat.window.as_ref() {
                    w.request_redraw();
                }
            }
            self.redraw();
        }
        #[cfg(target_os = "linux")]
        self.poll_kwin_drag_probe();
        // only tick (~2 redraws/sec) when a blinking cursor is actually on screen;
        // otherwise stay event-driven so idle panes cost nothing. content changes
        // already request redraws from their own events (pty output, keys, resize)
        // the main window's idle wakeup need (ms), or None when it can fully sleep
        let main_ms: Option<u64> = if self.any_flash() {
            // fade the bell flash out quickly
            self.redraw();
            Some(60)
        } else if self.pw.focused && self.blinking_cursor_on_screen() {
            self.redraw();
            Some(530)
        } else if self.pw.focused {
            // keep the status-bar clock current without busy-redrawing: wake
            // coarsely and only repaint when the displayed minute actually rolls
            let now_hm = win::local_hm();
            let stale = self
                .pw.renderer
                .as_ref()
                .map(|r| r.status_clock() != now_hm)
                .unwrap_or(false);
            if stale {
                self.redraw();
            }
            Some(5000)
        } else {
            None
        };
        // a torn-off window mid-animation needs ~60fps wakeups; take the soonest
        // of the main window's need and the satellite tick so neither is starved
        let sat_ms = sat_anim.then_some(16u64);
        // a live notice must wake the loop at its expiry even when otherwise idle
        let notice_ms: Option<u64> = self
            .notice_until
            .map(|t| t.saturating_duration_since(Instant::now()).as_millis() as u64 + 10);
        // a pending --drive step wakes the loop at its due time
        let drive_ms: Option<u64> = self.drive.as_ref().and_then(|d| {
            let started = d.started?;
            (d.next < d.steps.len())
                .then(|| d.steps[d.next].0.saturating_sub(started.elapsed()).as_millis() as u64 + 5)
        });
        #[cfg(target_os = "linux")]
        let kwin_ms = self.kwin_drag_probe.as_ref().map(|probe| {
            let deadline = if probe.script.is_some() {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(20)
            };
            deadline.saturating_sub(probe.started.elapsed()).as_millis() as u64 + 1
        });
        #[cfg(not(target_os = "linux"))]
        let kwin_ms: Option<u64> = None;
        match main_ms
            .into_iter()
            .chain(sat_ms)
            .chain(notice_ms)
            .chain(drive_ms)
            .chain(kwin_ms)
            .min()
        {
            Some(ms) => event_loop
                .set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(ms))),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl App {
    fn close_focused_pane_by_id(&mut self, id: usize, event_loop: &ActiveEventLoop) {
        let before = self.focus_identity();
        let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) else {
            return;
        };
        let Some(root) = tab.root.take() else {
            return;
        };
        match close_pane(root, id) {
            Some(node) => {
                if tab.focused == id {
                    tab.focused = first_leaf(&node);
                }
                tab.root = Some(node);
                self.relayout_all();
                self.sync_tabs();
                self.after_focus_context_change(before);
            }
            None => {
                let idx = self.pw.active_tab;
                self.close_tab(idx, event_loop);
            }
        }
    }
}

/// opt-in startup timing: when TERMIE_TIMING names a file, append "<ms> label"
/// lines at key milestones so startup latency can be measured. release is a
/// windowed subsystem with no console, so this writes to a file, not stderr
pub(crate) fn timing(label: &str) {
    use std::io::Write;
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    static SINK: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    let sink = SINK.get_or_init(|| std::env::var_os("TERMIE_TIMING").map(Into::into));
    if let Some(path) = sink
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(f, "{:>8.1} ms  {label}", start.elapsed().as_secs_f64() * 1000.0);
    }
}

fn main() -> Result<()> {
    // dev-only headless screen dumper; never compiled into release
    #[cfg(debug_assertions)]
    if termview::maybe_run() {
        return Ok(());
    }
    // dev-only headless chrome capture (full window to PNG)
    #[cfg(debug_assertions)]
    if uiview::maybe_run() {
        return Ok(());
    }
    // private cpu microbenchmarks (feature-gated; absent from the shipped binary)
    #[cfg(feature = "microbench")]
    if microbench::maybe_run() {
        return Ok(());
    }
    timing("process start");
    // give the log facade a sink (file, since release has no console); the
    // harness early-returns above stay log-free
    install_file_log();
    // stop child shells (esp. pool shells racing exit) from popping OS error dialogs
    win::suppress_child_error_dialogs();
    // a COM `-Embedding` launch is the OS handing over a console session
    // (default-terminal handoff); receive it before building any UI, and exit
    // quietly if the activation never delivers one
    #[cfg(windows)]
    let is_embedding = std::env::args()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case("-embedding") || a.eq_ignore_ascii_case("/embedding"));
    #[cfg(windows)]
    let handoff = if is_embedding {
        match defterm::serve_embedding() {
            Some(h) => Some(h),
            None => return Ok(()),
        }
    } else {
        None
    };
    // when serving as the default terminal, lock in permissive COM security
    // before winit initializes OLE — after that it's too late and OpenConsole
    // can't reach our handoff class object
    #[cfg(windows)]
    if !is_embedding && win::defterm_registered() {
        defterm::init_process_security();
    }
    let launch_args: Vec<String> = std::env::args().skip(1).collect();
    let launch_server = if launch_can_join(&launch_args) && !win::is_elevated() {
        let request = instance::LaunchRequest {
            args: launch_args,
            process_cwd: std::env::current_dir()
                .ok()
                .map(|cwd| cwd.to_string_lossy().into_owned()),
            launch_cwd: launch_cwd(win::foreground_window()),
        };
        match instance::claim(&request) {
            instance::Claim::Primary(server) => Some(server),
            instance::Claim::Forwarded => return Ok(()),
            instance::Claim::Standalone => None,
        }
    } else {
        None
    };
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    if let Some(server) = launch_server {
        let launch_proxy = proxy.clone();
        server.start(move |request| launch_proxy.send_event(UserEvent::Launch(request)).is_ok());
    }
    let mut app = App::new(proxy);
    #[cfg(windows)]
    {
        app.handoff = handoff;
    }
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_a_tab_drop_window_clears_the_cached_target() {
        let source = WindowId::from(1);
        let target = WindowId::from(2);
        let mut drag = TabDrag {
            source,
            index: 0,
            start: PhysicalPosition::new(20.0, 12.0),
            screen: Some(PhysicalPosition::new(800, 400)),
            target: Some((target, 1)),
            left_strip: true,
            left_window: true,
            label: "source".into(),
        };

        assert!(drag.window_left(target));
        assert!(drag.target.is_none());
        assert!(drag.left_window);
    }

    #[test]
    fn leaving_a_pane_drop_window_clears_the_cached_target() {
        let source = WindowId::from(1);
        let target = WindowId::from(2);
        let mut drag = PaneDrag {
            source_window: source,
            source_tab: 0,
            pane: 7,
            start: PhysicalPosition::new(20.0, 80.0),
            screen: Some(PhysicalPosition::new(800, 400)),
            target: Some(PaneDropDestination::Dock(PaneDropTarget {
                window: target,
                tab: 0,
                pane: 9,
                side: PaneDropSide::Right,
                rect: (100.0, 60.0, 300.0, 240.0),
            })),
            moved: true,
            left_window: true,
            label: "source".into(),
        };

        assert!(drag.window_left(target));
        assert!(drag.target.is_none());
        assert!(drag.left_window);
    }

    #[test]
    fn pane_drag_accepts_tabs_and_open_tab_strip_space() {
        assert_eq!(pane_tab_drop_index(Hit::Button(Hot::Tab(1)), 3), Some(1));
        assert_eq!(pane_tab_drop_index(Hit::Button(Hot::TabClose(2)), 3), Some(2));
        assert_eq!(pane_tab_drop_index(Hit::TitleBar, 3), Some(3));
        assert_eq!(pane_tab_drop_index(Hit::Button(Hot::NewTab), 3), Some(3));
        assert_eq!(pane_tab_drop_index(Hit::Button(Hot::Close), 3), None);
        assert_eq!(pane_tab_drop_index(Hit::Content, 3), None);
    }

    #[test]
    fn leaving_a_pane_tab_target_clears_the_insertion_marker() {
        let source = WindowId::from(1);
        let target = WindowId::from(2);
        let mut drag = PaneDrag {
            source_window: source,
            source_tab: 0,
            pane: 7,
            start: PhysicalPosition::new(20.0, 80.0),
            screen: Some(PhysicalPosition::new(800, 400)),
            target: Some(PaneDropDestination::Tab(target, 2)),
            moved: true,
            left_window: true,
            label: "source".into(),
        };

        assert!(drag.window_left(target));
        assert!(drag.target.is_none());
    }

    #[test]
    fn satellite_windows_start_hidden_with_custom_chrome() {
        let attrs = satellite_window_attrs(None);
        assert_eq!(attrs.title, "termie");
        assert!(!attrs.decorations);
        assert!(!attrs.visible);
        assert!(attrs.window_icon.is_none());
        assert!(attrs.inner_size.is_some());
        assert!(attrs.min_inner_size.is_some());
    }

    #[test]
    fn font_weight_labels_and_numbers_parse() {
        assert_eq!(font_weight_from_label("semibold"), Some(600));
        assert_eq!(font_weight_from_label("Regular"), Some(400));
        assert_eq!(font_weight_from_label("350"), Some(350));
        // out-of-range numbers clamp, garbage leaves the setting untouched
        assert_eq!(font_weight_from_label("1000"), Some(900));
        assert_eq!(font_weight_from_label("55"), Some(100));
        assert_eq!(font_weight_from_label("heavyish"), None);
    }

    #[test]
    fn drive_scripts_parse_combos_and_text() {
        let steps = parse_drive_script(
            "# comment\n\n500 key ctrl+shift+m\n100 type line 1\n50 key enter\nbogus line\n50 nope x\n",
        );
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].0, Duration::from_millis(500));
        assert!(matches!(&steps[0].1, DriveStep::Key(m, _) if m.control_key() && m.shift_key()));
        // delays accumulate; type keeps its full text
        assert_eq!(steps[1].0, Duration::from_millis(600));
        assert!(matches!(&steps[1].1, DriveStep::Type(t) if t == "line 1"));
        assert_eq!(steps[2].0, Duration::from_millis(650));
        assert!(matches!(&steps[2].1, DriveStep::Key(_, Key::Named(NamedKey::Enter))));
    }

    #[test]
    fn drive_scripts_parse_pointer_and_left_mouse_steps() {
        let steps = parse_drive_script(
            "10 pointer 12.5 -4\n20 mouse down\n30 pointer NaN 2\n40 pointer 1 nope\n50 mouse up\n",
        );
        assert_eq!(steps.len(), 3);
        assert!(matches!(
            steps[0],
            (at, DriveStep::Pointer(position))
                if at == Duration::from_millis(10)
                    && position == PhysicalPosition::new(12.5, -4.0)
        ));
        assert!(matches!(
            steps[1],
            (at, DriveStep::Mouse(ElementState::Pressed)) if at == Duration::from_millis(30)
        ));
        assert!(matches!(
            steps[2],
            (at, DriveStep::Mouse(ElementState::Released)) if at == Duration::from_millis(150)
        ));
    }

    #[test]
    fn layout_verbs_build_tabs_and_splits() {
        let args: Vec<String> =
            ["new-tab", "-d", "C:/a", ";", "split-pane", "-H", "--shell", "cmd", ";", "nt", "--shell=wsl"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let cli = parse_args(args.into_iter());
        assert!(!cli.is_bare());
        assert_eq!(cli.tabs.len(), 2);
        match &cli.tabs[0].root {
            session::NodeSnap::Split { vertical, a, b, .. } => {
                assert!(!vertical, "-H stacks the new pane below");
                assert!(matches!(&**a, session::NodeSnap::Leaf { cwd: Some(c), .. } if c == "C:/a"));
                assert!(matches!(&**b, session::NodeSnap::Leaf { shell, .. } if shell == "cmd"));
            }
            _ => panic!("expected a split"),
        }
        // the newest pane keeps focus
        assert_eq!(cli.tabs[0].focused_leaf, 1);
        assert!(matches!(&cli.tabs[1].root, session::NodeSnap::Leaf { shell, .. } if shell == "wsl"));
    }

    #[test]
    fn layout_verbs_leading_split_implies_a_tab_and_legacy_args_stay_legacy() {
        let cli = parse_args(["split-pane".to_string()].into_iter());
        assert_eq!(cli.tabs.len(), 1);
        let cli = parse_args(["-d".to_string(), "C:/x".to_string()].into_iter());
        assert!(cli.tabs.is_empty());
        assert_eq!(cli.cwd.as_deref(), Some("C:/x"));
    }

    #[test]
    fn sel_view_span_clamps_offscreen_endpoints() {
        let mut g = grid::Grid::new(2, 4);
        g.set_scrollback_limit(100);
        for line in ["aa", "bb", "cc", "dd", "ee"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        let base = g.abs_base();
        let sel =
            Sel { pane: 0, start: (base, 1), end: (base + 4, 0), block: false, reflow_gen: g.reflow_gen };
        // at the live bottom only the selection's tail is on screen; the start
        // clamps to the viewport's top-left
        let span = sel_view_span(&g, &sel).unwrap();
        assert_eq!(span.0, (0, 0));
        assert_eq!(span.1, (0, 0));
        // scrolled to the top the start shows at its real column and the end
        // clamps to the bottom-right
        g.scroll_view(g.scrollback.len() as isize);
        let span = sel_view_span(&g, &sel).unwrap();
        assert_eq!(span.0, (0, 1));
        assert_eq!(span.1, (1, 3));
        // a stale reflow generation hides the selection instead of lying
        let stale = Sel { reflow_gen: sel.reflow_gen.wrapping_add(1), ..sel };
        assert!(sel_view_span(&g, &stale).is_none());
    }

    #[test]
    fn font_pick_filter_ranks_and_matches() {
        let mut app_families =
            vec!["Cascadia Code".to_string(), "Cascadia Mono".to_string(), "Consolas".to_string(), "JetBrains Mono".to_string()];
        app_families.sort();
        // empty query returns the whole list; a query fuzzy-filters + ranks
        let full = filter_fonts(&app_families, "");
        assert_eq!(full.len(), 4);
        // fuzzy (subsequence) matching, so every result must contain the query
        // chars in order; the two Cascadia families both prefix-match "casc"
        let casc = filter_fonts(&app_families, "casc");
        assert_eq!(casc.len(), 2);
        assert!(casc.iter().all(|f| f.starts_with("Cascadia")));
        // "jbm" scattered-matches "JetBrains Mono" only
        assert_eq!(filter_fonts(&app_families, "jbm"), vec!["JetBrains Mono".to_string()]);
        assert!(filter_fonts(&app_families, "zzz").is_empty());
    }

    // mirror of App::font_pick_filter without the renderer, for the unit test
    fn filter_fonts(families: &[String], query: &str) -> Vec<String> {
        let q = query.trim();
        if q.is_empty() {
            return families.to_vec();
        }
        let mut scored: Vec<(i32, &String)> = families
            .iter()
            .filter_map(|f| fuzzy_score(q, &f.to_ascii_lowercase()).map(|s| (s, f)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, f)| f.clone()).collect()
    }

    #[test]
    fn kitty_demo_bytes_store_an_image_and_place_it() {
        let demo = kitty_demo_bytes();
        // stage 1: the scanner must split out every chunk of the transfer
        let mut sc = apc::ApcScanner::default();
        let (_, imgs) = sc.feed(&demo);
        assert_eq!(imgs.len(), 9, "scanner should split out all 9 APC chunks");
        // stage 2: every chunk must parse, the first carrying the display action
        let cmds: Vec<apc::KittyCmd> =
            imgs.iter().filter_map(|raw| apc::KittyCmd::parse(raw)).collect();
        assert_eq!(cmds.len(), 9, "every chunk should parse");
        assert_eq!(cmds[0].action, b'T');
        assert!(cmds[0].more);
        assert!(!cmds[8].more);
        assert_eq!(cmds.iter().map(|c| c.payload.len()).sum::<usize>(), 96 * 96 * 3);
        // stage 3: the full pump stores and places the image
        let mut p = tp(1);
        pump_bytes(&mut p, &demo);
        assert_eq!(p.term.grid.placements().len(), 1, "demo image should be placed");
        let pl = p.term.grid.placements()[0];
        assert!(p.term.images.get(pl.image_id).is_some(), "demo image should be stored");
    }

    #[test]
    fn kitty_demo_base64_round_trips() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"the quick brown fox \x00\xff\x10"] {
            let enc = base64_encode(data);
            let dec = term::base64_decode(&enc).expect("valid base64");
            assert_eq!(dec, data);
        }
        // the demo stream is well-formed: caption, then the APC intro, and
        // every chunk terminates with ST
        let demo = kitty_demo_bytes();
        assert!(demo.starts_with(b"kitty demo:"));
        let intro = b"\x1b_Ga=T,f=24,s=96,v=96,m=1;";
        assert!(demo.windows(intro.len()).any(|w| w == intro));
        assert!(demo.windows(2).filter(|w| w == b"\x1b\\").count() >= 2);
    }

    #[test]
    fn boring_titles_yield_to_the_cwd_label() {
        // conpty announces the spawned exe; shells announce themselves
        for t in [
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
            "Administrator: C:\\Windows\\system32\\cmd.exe",
            "c:/windows/system32/cmd.exe",
            "\\\\server\\share\\tool.exe",
            "Windows PowerShell",
            "pwsh",
            "Command Prompt",
        ] {
            assert!(boring_title(t), "{t:?} should be boring");
        }
        // titles an app set on purpose survive
        for t in ["✳ fixing tests", "vim readme.md", "3/5 done", "user@host: ~/src"] {
            assert!(!boring_title(t), "{t:?} should win the label");
        }
    }

    #[test]
    fn merge_progress_picks_the_taskbar_value() {
        // none + none stays none; a single normal wins over none
        assert_eq!(merge_progress((0, 0), (0, 0)), (0, 0));
        assert_eq!(merge_progress((0, 0), (1, 40)), (1, 40));
        // error beats paused beats normal beats indeterminate
        assert_eq!(merge_progress((1, 90), (2, 10)), (2, 10));
        assert_eq!(merge_progress((4, 5), (1, 99)), (4, 5));
        assert_eq!(merge_progress((3, 0), (1, 1)), (1, 1));
        // equal severity takes the larger percentage, either order
        assert_eq!(merge_progress((1, 30), (1, 70)), (1, 70));
        assert_eq!(merge_progress((1, 70), (1, 30)), (1, 70));
    }

    #[test]
    fn latency_percentiles_and_hud() {
        let mut m = LatencyMeter::default();
        assert_eq!(m.hud(), None);
        for v in [10.0, 1.0, 5.0, 2.0, 8.0, 3.0, 4.0, 6.0, 7.0, 9.0] {
            m.record_input(v);
        }
        let (p50, p95) = percentiles(&m.input_ms).unwrap();
        assert_eq!((p50, p95), (6.0, 10.0));
        // the cap keeps the meter bounded no matter how much is recorded
        for _ in 0..500 {
            m.record_frame(16.6);
        }
        assert!(m.frame_ms.len() <= 120);
        assert!(m.hud().unwrap().contains("in->photon"));
    }

    #[test]
    fn latency_hud_segments_match_recorded_data() {
        // nothing recorded yet -> nothing to show
        assert_eq!(LatencyMeter::default().hud(), None);
        // frame samples only -> a frame segment, no input-to-photon segment
        let mut f = LatencyMeter::default();
        f.record_frame(16.6);
        let line = f.hud().unwrap();
        assert!(line.contains("frame") && !line.contains("in->photon"));
        // input samples only -> an input-to-photon segment, no frame segment
        let mut i = LatencyMeter::default();
        i.record_input(4.0);
        let line = i.hud().unwrap();
        assert!(line.contains("in->photon") && !line.contains("frame"));
        // both -> one combined line carrying both segments
        let mut b = LatencyMeter::default();
        b.record_input(4.0);
        b.record_frame(16.6);
        let line = b.hud().unwrap();
        assert!(line.contains("in->photon") && line.contains("frame"));
    }

    #[test]
    fn latency_percentiles_edges() {
        use std::collections::VecDeque;
        // a single sample is both p50 and p95
        let one: VecDeque<f32> = [7.0].into_iter().collect();
        assert_eq!(percentiles(&one), Some((7.0, 7.0)));
        // an empty set has no percentiles
        assert_eq!(percentiles(&VecDeque::new()), None);
        // percentiles sort first, so insertion order can't change the result
        let shuffled: VecDeque<f32> = [5.0, 1.0, 4.0, 2.0, 3.0].into_iter().collect();
        let sorted: VecDeque<f32> = [1.0, 2.0, 3.0, 4.0, 5.0].into_iter().collect();
        assert_eq!(percentiles(&shuffled), percentiles(&sorted));
    }

    #[test]
    fn latency_meter_is_capped_at_120_samples() {
        let mut m = LatencyMeter::default();
        for _ in 0..1000 {
            m.record_input(1.0);
            m.record_frame(2.0);
        }
        assert_eq!(m.input_ms.len(), 120);
        assert_eq!(m.frame_ms.len(), 120);
    }

    #[test]
    fn config_parses_feature_flags_and_aliases() {
        // the opt-in sandbox accepts each of its documented spellings
        assert!(parse_persisted("plugin_sandbox=appcontainer").plugin_sandbox);
        assert!(parse_persisted("plugin_sandbox=on").plugin_sandbox);
        assert!(parse_persisted("plugin_sandbox=true").plugin_sandbox);
        assert!(!parse_persisted("plugin_sandbox=off").plugin_sandbox);
        assert!(!Persisted::default().plugin_sandbox);

        assert!(parse_persisted("inline_paint=true").inline_paint);
        assert!(parse_persisted("inline_paint=on").inline_paint);
        assert!(!parse_persisted("inline_paint=false").inline_paint);

        assert!(parse_persisted("latency_hud=true").latency_hud);
        assert!(parse_persisted("latency_hud=on").latency_hud);
        assert!(!parse_persisted("latency_hud=nope").latency_hud);

        assert_eq!(parse_persisted("wsl_distro=Ubuntu").wsl_distro.as_deref(), Some("Ubuntu"));
        // an empty value leaves the default (no distro pinned)
        assert_eq!(parse_persisted("wsl_distro=").wsl_distro, None);

        // the system backdrop accepts both spellings, off by default
        assert!(parse_persisted("acrylic=true").acrylic);
        assert!(parse_persisted("mica=on").acrylic);
        assert!(!parse_persisted("acrylic=false").acrylic);
        assert!(!Persisted::default().acrylic);
    }

    #[test]
    fn config_parses_right_click() {
        assert_eq!(parse_persisted("right_click=paste").right_click, RightClick::Paste);
        assert_eq!(parse_persisted("right_click=menu").right_click, RightClick::Menu);
        // an unknown value warns and keeps the default (menu)
        assert_eq!(parse_persisted("right_click=bogus").right_click, RightClick::Menu);
        assert_eq!(Persisted::default().right_click, RightClick::Menu);
    }

    #[test]
    fn window_bounds_clamp_to_a_visible_monitor() {
        let one = [(0, 0, 1920u32, 1080u32)];
        // fully inside: untouched
        assert_eq!(clamp_window_bounds(&one, (100, 100, 800, 600)), (100, 100, 800, 600));
        // every edge stays contained, even when the saved title bar was reachable
        assert_eq!(clamp_window_bounds(&one, (1600, 100, 800, 600)), (1120, 100, 800, 600));
        assert_eq!(clamp_window_bounds(&one, (-100, -80, 800, 600)), (0, 0, 800, 600));
        assert_eq!(clamp_window_bounds(&one, (100, 900, 800, 600)), (100, 480, 800, 600));
        // larger than the monitor is capped to its size
        assert_eq!(clamp_window_bounds(&one, (0, 0, 5000, 5000)), (0, 0, 1920, 1080));
        // a window off every monitor (its display is gone) is centered on the primary
        let two = [(0, 0, 1920u32, 1080u32), (1920, 0, 1280, 1024)];
        assert_eq!(clamp_window_bounds(&two, (-4000, -4000, 1000, 700)), (460, 190, 1000, 700));
    }

    #[test]
    fn monitor_limit_includes_window_frame() {
        let monitor = PhysicalSize::new(1920, 1080);
        assert_eq!(
            monitor_inner_limit(monitor, PhysicalSize::new(1000, 640), PhysicalSize::new(1016, 679)),
            PhysicalSize::new(1904, 1041)
        );
        assert_eq!(monitor_inner_limit(monitor, monitor, monitor), monitor);
    }

    #[test]
    fn split_cmdline_respects_quotes() {
        assert_eq!(split_cmdline("nu.exe"), ["nu.exe"]);
        assert_eq!(
            split_cmdline(r#""C:\Program Files\Git\bin\bash.exe" -i -l"#),
            [r"C:\Program Files\Git\bin\bash.exe", "-i", "-l"]
        );
        assert_eq!(split_cmdline("  a   b  "), ["a", "b"]);
        assert!(split_cmdline("   ").is_empty());
    }

    fn prof(name: &str, argv: &[&str]) -> pty::Profile {
        pty::Profile {
            name: name.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: Vec::new(),
        }
    }

    #[test]
    fn config_parses_custom_profiles() {
        let p = parse_persisted(
            "profile.git-bash=\"C:\\Git\\bin\\bash.exe\" -i -l\nprofile.nu=nu.exe\nprofile.=broken\nprofile.empty=\n",
        );
        assert_eq!(p.profiles.len(), 2);
        assert_eq!(p.profiles[0].name, "git-bash");
        assert_eq!(p.profiles[0].argv, ["C:\\Git\\bin\\bash.exe", "-i", "-l"]);
        assert_eq!(p.profiles[1].name, "nu");
        assert_eq!(p.profiles[1].argv, ["nu.exe"]);
        // the raw lines round-trip for save_config
        assert_eq!(p.profiles_raw[1], ("nu".to_string(), "nu.exe".to_string()));
    }

    #[test]
    fn config_parses_profile_cwd_and_env() {
        let p = parse_persisted(
            "profile.dev=pwsh.exe\nprofile.dev.cwd=C:\\repo\nprofile.dev.env.RUST_LOG=debug\nprofile.dev.env.API=1\n",
        );
        assert_eq!(p.profiles.len(), 1);
        let dev = &p.profiles[0];
        assert_eq!(dev.argv, ["pwsh.exe"]);
        assert_eq!(dev.cwd.as_deref(), Some("C:\\repo"));
        assert_eq!(
            dev.env,
            [("RUST_LOG".to_string(), "debug".to_string()), ("API".to_string(), "1".to_string())]
        );
        // sub-keys attach in any line order; an orphaned sub-key (no command
        // line) leaves no profile but the raw line still round-trips
        let q = parse_persisted("profile.x.cwd=C:\\a\nprofile.x=cmd.exe\nprofile.orphan.env.FOO=bar\n");
        assert_eq!(q.profiles.len(), 1);
        assert_eq!(q.profiles[0].name, "x");
        assert_eq!(q.profiles[0].cwd.as_deref(), Some("C:\\a"));
        assert!(q.profiles_raw.iter().any(|(k, v)| k == "x.cwd" && v == "C:\\a"));
        assert!(q.profiles_raw.iter().any(|(k, v)| k == "orphan.env.FOO" && v == "bar"));
    }

    #[test]
    fn wsl_distros_become_synthetic_profiles() {
        let base = vec![prof("nu", &["nu.exe"])];
        let merged = with_wsl_profiles(
            base,
            vec!["Ubuntu".to_string(), "Arch".to_string(), "Ubuntu 22.04 LTS".to_string()],
        );
        // config profiles keep their slot; every distro appends unconditionally
        // as wsl.exe -d <name>, and a name with spaces round-trips as one argv
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].name, "nu");
        assert_eq!(merged[1].name, "wsl: Ubuntu");
        assert_eq!(merged[1].argv, ["wsl.exe", "-d", "Ubuntu"]);
        assert_eq!(merged[2].name, "wsl: Arch");
        assert_eq!(merged[2].argv, ["wsl.exe", "-d", "Arch"]);
        assert_eq!(merged[3].name, "wsl: Ubuntu 22.04 LTS");
        assert_eq!(merged[3].argv, ["wsl.exe", "-d", "Ubuntu 22.04 LTS"]);
    }

    #[test]
    fn wsl_synthetic_profile_yields_to_a_user_profile_of_the_same_name() {
        let base = vec![prof("wsl: Ubuntu", &["custom.exe"])];
        let merged = with_wsl_profiles(base, vec!["Ubuntu".to_string()]);
        // the user's own definition wins; no duplicate synthetic entry is added
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].argv, ["custom.exe"]);
    }

    #[test]
    fn reopen_stack_is_bounded_and_pops_lifo() {
        let mk = |tag: usize| ClosedTab { shell: ShellKind::Pwsh, cwd: Some(tag.to_string()), title: None };
        let mut stack: Vec<ClosedTab> = Vec::new();
        let n = CLOSED_TAB_CAP + 3;
        for i in 0..n {
            push_closed_tab(&mut stack, mk(i));
        }
        // the cap holds and the three oldest (0, 1, 2) fell off the bottom
        assert_eq!(stack.len(), CLOSED_TAB_CAP);
        assert_eq!(stack.first().unwrap().cwd.as_deref(), Some("3"));
        // pop is last-in-first-out: the most recently closed comes back first
        assert_eq!(stack.pop().unwrap().cwd.as_deref(), Some((n - 1).to_string().as_str()));
    }

    #[test]
    fn config_parses_per_profile_themes() {
        let p = parse_persisted("theme=paper\ntheme.cmd=nord\ntheme.git-bash=gruvbox\ntheme.=broken\ntheme.wsl=\n");
        // the global theme key is untouched by the per-profile lines
        assert_eq!(p.theme, color::ThemeId::Paper);
        assert_eq!(
            p.shell_themes,
            [
                ("cmd".to_string(), color::ThemeId::Nord),
                ("git-bash".to_string(), color::ThemeId::Gruvbox)
            ]
        );
    }

    #[test]
    fn config_parses_theme_auto() {
        let p = parse_persisted("theme=auto\ntheme_dark=nord\ntheme_light=paper\n");
        assert!(p.theme_auto);
        assert_eq!(p.theme_dark, color::ThemeId::Nord);
        assert_eq!(p.theme_light, color::ThemeId::Paper);
        // a concrete theme after auto wins and turns auto off (last line rules)
        let p = parse_persisted("theme=auto\ntheme=koi\n");
        assert!(!p.theme_auto);
        assert_eq!(p.theme, color::ThemeId::Koi);
    }

    #[test]
    fn config_round_trips_the_serialized_flag_lines() {
        // exactly what the settings writer emits for the opt-in features
        let text = "plugin_sandbox=appcontainer\ninline_paint=true\nlatency_hud=true\nwsl_distro=Arch\n";
        let p = parse_persisted(text);
        assert!(p.plugin_sandbox && p.inline_paint && p.latency_hud);
        assert_eq!(p.wsl_distro.as_deref(), Some("Arch"));
    }

    #[test]
    fn config_ignores_unknown_keys_and_malformed_lines() {
        let text = "\nfrobnicate=yes\nlatency_hud=true\n   \nplugin_sandbox\n";
        let p = parse_persisted(text);
        // the one real key still takes effect
        assert!(p.latency_hud);
        // a bare key with no '=' is skipped, not read as enabling the sandbox
        assert!(!p.plugin_sandbox);
        // unknown keys leave everything else at its default
        assert_eq!(p.scrollback, Persisted::default().scrollback);
    }

    #[test]
    fn cwd_path_parses_osc7_uris() {
        assert_eq!(cwd_path(Some("file:///C:/Users/dev")).as_deref(), Some("C:/Users/dev"));
        assert_eq!(cwd_path(Some("file://host/C:/dev")).as_deref(), Some("C:/dev"));
        assert_eq!(cwd_path(Some("file:///C:/a%20b")).as_deref(), Some("C:/a b"));
        assert_eq!(cwd_path(None), None);
    }

    #[test]
    fn parse_args_forms() {
        let p = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert!(p(&[]).is_bare());
        assert_eq!(p(&["--cwd", "C:/x"]).cwd.as_deref(), Some("C:/x"));
        assert_eq!(p(&["-d", "C:/y"]).cwd.as_deref(), Some("C:/y"));
        assert_eq!(p(&["--cwd=C:/z"]).cwd.as_deref(), Some("C:/z"));
        assert!(!p(&["--cwd", "C:/x"]).is_bare());
        assert_eq!(p(&["--shell", "cmd"]).shell.as_deref(), Some("cmd"));
        assert_eq!(p(&["--shell=wsl"]).shell.as_deref(), Some("wsl"));
        assert!(!p(&["--shell", "cmd"]).is_bare());
        let cmd = p(&["--", "vim", "a.txt"]);
        assert_eq!(
            cmd.command.as_deref(),
            Some(&["vim".to_string(), "a.txt".to_string()][..])
        );
        assert!(!cmd.is_bare());
        let admin = p(&["--admin-shell", "--", "sudo", "-s"]);
        assert!(admin.admin_shell);
        assert_eq!(admin.command.as_deref(), Some(&["sudo".to_string(), "-s".to_string()][..]));
        // unknown flags are ignored, not misread as a cwd or command
        assert!(p(&["--frobnicate"]).is_bare());
    }

    #[test]
    fn only_interactive_launches_join_the_running_app() {
        let args = |values: &[&str]| values.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert!(launch_can_join(&args(&[])));
        assert!(launch_can_join(&args(&["--cwd", "work", "--shell", "bash"])));
        assert!(!launch_can_join(&args(&["--drive", "steps.txt"])));
        assert!(!launch_can_join(&args(&["--drive=steps.txt"])));
        assert!(!launch_can_join(&args(&["--admin-shell"])));
        assert!(!launch_can_join(&args(&["-Embedding"])));
        assert!(!launch_can_join(&args(&["/EMBEDDING"])));
    }

    #[test]
    fn forwarded_layout_dirs_resolve_from_the_launching_process() {
        let base = std::path::Path::new("workspace").join("project");
        let expected = base.join("child").to_string_lossy().into_owned();
        let inherited = base.to_string_lossy().into_owned();
        let mut root = session::NodeSnap::Split {
            vertical: true,
            ratio: 0.5,
            a: Box::new(session::NodeSnap::Leaf {
                cwd: Some("child".into()),
                shell: "default".into(),
            }),
            b: Box::new(session::NodeSnap::Leaf {
                cwd: None,
                shell: "default".into(),
            }),
        };
        resolve_layout_dirs(&mut root, Some(&inherited));
        let session::NodeSnap::Split { a, b, .. } = root else {
            panic!("expected split layout");
        };
        assert!(matches!(*a, session::NodeSnap::Leaf { cwd: Some(ref cwd), .. } if cwd == &expected));
        assert!(matches!(*b, session::NodeSnap::Leaf { cwd: Some(ref cwd), .. } if cwd == &inherited));
    }

    #[test]
    fn torn_out_window_keeps_the_pointer_at_its_grab_point() {
        assert_eq!(
            drag_window_origin(
                PhysicalPosition::new(840, 460),
                PhysicalPosition::new(238.4, 19.7),
            ),
            PhysicalPosition::new(602, 440),
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_admin_window_prefers_polkit_and_falls_back_to_sudo() {
        assert_eq!(linux_admin_command(true, true), Some(&["pkexec", "--keep-cwd"][..]));
        assert_eq!(linux_admin_command(false, true), Some(&["sudo", "-s"][..]));
        assert_eq!(linux_admin_command(false, false), None);
    }

    #[test]
    fn same_dir_ignores_case_separators_and_trailing_slash() {
        use std::path::Path;
        // non-existent paths exercise the literal-normalization fallback.
        // windows filesystems fold case; unix ones are case-sensitive
        #[cfg(windows)]
        {
            assert!(same_dir(Path::new("C:\\Users\\Me"), Path::new("c:/users/me/")));
            assert!(same_dir(Path::new("C:\\Users\\Me\\"), Path::new("C:\\users\\me")));
            assert!(!same_dir(Path::new("C:\\Users\\Me"), Path::new("C:\\Users\\Other")));
        }
        #[cfg(not(windows))]
        {
            assert!(same_dir(Path::new("/home/me"), Path::new("/home/me/")));
            assert!(!same_dir(Path::new("/home/me"), Path::new("/home/Me")));
            assert!(!same_dir(Path::new("/home/me"), Path::new("/home/other")));
        }
    }

    #[test]
    fn parse_color_forms() {
        assert_eq!(parse_color("#ff8800"), Some(color::Rgb::new(255, 136, 0)));
        assert_eq!(parse_color("#f80"), Some(color::Rgb::new(255, 136, 0)));
        assert_eq!(parse_color("255, 136, 0"), Some(color::Rgb::new(255, 136, 0)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn parse_combo_forms() {
        let (m, k) = parse_combo("ctrl+shift+t").unwrap();
        assert_eq!(m, ModifiersState::CONTROL | ModifiersState::SHIFT);
        // matching is case-insensitive (shift uppercases the char)
        assert!(key_matches(&Key::Character("T".into()), &k));
        let (m2, k2) = parse_combo("alt+enter").unwrap();
        assert_eq!(m2, ModifiersState::ALT);
        assert_eq!(k2, Key::Named(NamedKey::Enter));
        // nav keys parse with their aliases; a bare key carries empty modifiers
        let (m3, k3) = parse_combo("shift+ins").unwrap();
        assert_eq!(m3, ModifiersState::SHIFT);
        assert_eq!(k3, Key::Named(NamedKey::Insert));
        let (m4, k4) = parse_combo("f11").unwrap();
        assert_eq!(m4, ModifiersState::empty());
        assert_eq!(k4, Key::Named(NamedKey::F11));
        assert_eq!(parse_combo("pgdn").unwrap().1, Key::Named(NamedKey::PageDown));
        assert_eq!(parse_combo("mouse4").unwrap().1, Key::Named(NamedKey::F23));
        assert_eq!(parse_combo("xbutton2").unwrap().1, Key::Named(NamedKey::F24));
        assert_eq!(key_label(&Key::Named(NamedKey::F23)).as_deref(), Some("mouse4"));
        assert_eq!(key_label(&Key::Named(NamedKey::F24)).as_deref(), Some("mouse5"));
        assert!(parse_combo("ctrl+").is_none());
    }

    #[test]
    fn keybinding_defaults_and_labels() {
        let d = default_keybindings();
        let ctrl = ModifiersState::CONTROL;
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let has = |m: ModifiersState, key: &str, a: PaletteAction| {
            d.iter().any(|(mm, k, aa)| *mm == m && key_matches(&Key::Character(key.into()), k) && *aa == a)
        };
        assert!(has(ctrl, "t", PaletteAction::NewTab));
        // Ctrl+Shift+T reopens the last closed tab (chrome/vscode muscle memory);
        // Ctrl+T stays new-tab for WT switchers
        assert!(has(cs, "t", PaletteAction::ReopenTab));
        assert!(has(cs, "n", PaletteAction::NewWindow));
        assert!(has(cs, "d", PaletteAction::DuplicateTab));
        assert!(has(cs, "e", PaletteAction::SplitV));
        assert!(has(ctrl, "1", PaletteAction::SelectTab(0)));
        assert!(has(ctrl, "9", PaletteAction::SelectTab(8)));
        // '+' and '_' are typed with shift, so they must be bound Ctrl+Shift to
        // match the modifiers that actually arrive (regression guard)
        assert!(has(cs, "+", PaletteAction::FontInc));
        assert!(has(cs, "_", PaletteAction::FontDec));
        // Ctrl+Shift+P is deliberately NOT a default (dedicated pane-mode handler)
        assert!(!d.iter().any(|(m, k, _)| *m == cs && key_matches(&Key::Character("p".into()), k)));
        // the classic conhost chords and bare-F11 fullscreen ship as defaults
        let named = |m: ModifiersState, k: NamedKey, a: PaletteAction| {
            d.iter().any(|(bm, bk, ba)| *bm == m && *bk == Key::Named(k) && *ba == a)
        };
        assert!(named(ctrl, NamedKey::Insert, PaletteAction::Copy));
        assert!(named(ModifiersState::SHIFT, NamedKey::Insert, PaletteAction::Paste));
        assert!(named(ModifiersState::empty(), NamedKey::F11, PaletteAction::ToggleFullscreen));
        assert!(named(cs, NamedKey::PageUp, PaletteAction::MoveTabLeft));
        assert!(named(cs, NamedKey::PageDown, PaletteAction::MoveTabRight));
        // mark mode ships on the windows terminal chord and resolves by label
        assert!(has(cs, "m", PaletteAction::MarkMode));
        assert_eq!(action_from_label("mark mode"), Some(PaletteAction::MarkMode));
        assert!(has(cs, "a", PaletteAction::SelectAll));
        assert_eq!(action_from_label("select all"), Some(PaletteAction::SelectAll));
        assert!(has(ctrl | ModifiersState::ALT, "a", PaletteAction::JumpAttention));
        // label resolution covers palette + keybinding-only + select-tab
        assert_eq!(action_from_label("new tab"), Some(PaletteAction::NewTab));
        assert_eq!(action_from_label("new window"), Some(PaletteAction::NewWindow));
        assert_eq!(action_from_label("tab search"), Some(PaletteAction::TabSearch));
        assert_eq!(action_from_label("reopen closed tab"), Some(PaletteAction::ReopenTab));
        assert_eq!(action_from_label("copy"), Some(PaletteAction::Copy));
        assert_eq!(action_from_label("select tab 3"), Some(PaletteAction::SelectTab(2)));
        assert_eq!(action_from_label("bogus action"), None);
    }

    #[test]
    fn tab_color_palette_labels_track_the_menu_swatches() {
        // the palette rows and the tab menu's swatch list must name the same
        // colors in the same order, or the two paths drift apart
        for (i, name) in render::TAB_COLOR_ITEMS.iter().enumerate() {
            let label = format!("tab color: {name}");
            assert_eq!(action_from_label(&label), Some(PaletteAction::SetTabColor(i)), "{label}");
        }
        let n = PALETTE_ACTIONS
            .iter()
            .filter(|(_, a)| matches!(a, PaletteAction::SetTabColor(_)))
            .count();
        assert_eq!(n, render::TAB_COLOR_ITEMS.len());
    }

    #[test]
    fn keybindings_template_lists_every_action_commented() {
        let t = keybindings_template();
        // every palette action label appears (bound in the defaults block or in
        // the no-default block), so the template can't silently drop an action
        for (label, _) in PALETTE_ACTIONS {
            assert!(t.contains(label), "template missing action label: {label}");
        }
        // keybinding-only actions with defaults show up with their alias label
        assert!(t.contains("# ctrl+, = toggle settings"));
        assert!(t.contains("# ctrl+1 = select tab 1"));
        // nothing is active: every non-blank line is a comment
        assert!(t.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')));
    }

    #[test]
    fn keybindings_conf_counts_ignored_lines() {
        let mut out = default_keybindings();
        let mut sends = Vec::new();
        let ignored = apply_keybindings_conf(
            "# a comment, not counted\n\
             ctrl+shift+q = copy\n\
             this line has no equals\n\
             ctrl+nope = paste\n\
             ctrl+j = not a real action\n\
             ctrl+k = none\n",
            &mut out,
            &mut sends,
        );
        // missing '=', unparseable combo, and unknown action each count once; the
        // comment, the valid override, and the `none` unbind do not
        assert_eq!(ignored, 3);
        assert!(sends.is_empty());
    }

    // `send <text>` binds a typed payload: escapes unescape, \n normalizes to
    // the enter byte, and an empty payload is refused rather than bound
    #[test]
    fn keybindings_conf_send_action_interns_payloads() {
        let mut out = Vec::new();
        let mut sends = Vec::new();
        let ignored = apply_keybindings_conf(
            "ctrl+alt+g = send git status\\r\n\
             ctrl+alt+l = SEND ls\\n\n\
             ctrl+alt+e = send \n",
            &mut out,
            &mut sends,
        );
        assert_eq!(ignored, 1, "the empty payload is refused");
        assert_eq!(sends, vec!["git status\r".to_string(), "ls\r".to_string()]);
        let actions: Vec<PaletteAction> = out.iter().map(|(_, _, a)| *a).collect();
        assert_eq!(
            actions,
            vec![PaletteAction::SendInput(0), PaletteAction::SendInput(1)]
        );
    }

    #[test]
    fn active_tab_index_tracks_reorder() {
        // the active tab itself follows the move
        assert_eq!(active_after_move(2, 2, 0), 0);
        assert_eq!(active_after_move(0, 0, 3), 3);
        // a tab dragged across the active one shifts it by a single slot
        assert_eq!(active_after_move(1, 0, 2), 0); // from left, past it, to its right
        assert_eq!(active_after_move(1, 3, 0), 2); // from right, past it, to its left
        assert_eq!(active_after_move(1, 3, 1), 2); // landing exactly on it from the right
        assert_eq!(active_after_move(1, 0, 1), 0); // landing exactly on it from the left
        // moves entirely on one side leave it alone
        assert_eq!(active_after_move(0, 1, 3), 0);
        assert_eq!(active_after_move(3, 0, 2), 3);
    }

    #[test]
    fn focus_view_changed_keys_on_pane_id_not_tab_index() {
        // same pane after leftward tab removal: tab 2→1, pane unchanged
        assert!(!focus_view_changed(Some((2, 5)), Some((1, 5))));
        // same pane, same tab
        assert!(!focus_view_changed(Some((0, 7)), Some((0, 7))));
        // both absent
        assert!(!focus_view_changed(None, None));
        // pane retarget inside one tab (split / close / focus_dir / click)
        assert!(focus_view_changed(Some((0, 1)), Some((0, 2))));
        // different tab AND different pane (real tab switch)
        assert!(focus_view_changed(Some((0, 1)), Some((1, 3))));
        // same tab index but different pane after reorder of other tabs still counts
        assert!(focus_view_changed(Some((1, 4)), Some((1, 9))));
        // focus gained / lost
        assert!(focus_view_changed(None, Some((0, 1))));
        assert!(focus_view_changed(Some((0, 1)), None));
        // tab switch that lands on a different pane id (usual case: each tab's leaf)
        // even when the numbers look like a reindex of the same slot
        assert!(focus_view_changed(Some((0, 10)), Some((1, 11))));
    }

    #[test]
    fn find_must_follow_focus_matrix() {
        // closed find never recomputes
        assert!(!find_must_follow_focus(false, Some((0, 1)), Some((1, 1)), false));
        assert!(!find_must_follow_focus(false, Some((0, 1)), Some((0, 2)), false));
        // same pane id at same or shifted tab index: no recompute
        assert!(!find_must_follow_focus(true, Some((0, 1)), Some((0, 1)), false));
        // skeptic: leftward owner-tab removal leaves the same pane at a lower index
        assert!(!find_must_follow_focus(true, Some((2, 5)), Some((1, 5)), false));
        assert!(!find_must_follow_focus(true, Some((3, 9)), Some((0, 9)), false));
        // real pane retarget → recompute
        assert!(find_must_follow_focus(true, Some((0, 1)), Some((0, 2)), false)); // split/close/focus_dir
        assert!(find_must_follow_focus(true, Some((0, 1)), Some((1, 3)), false)); // tab switch
        assert!(find_must_follow_focus(true, Some((0, 1)), None, false));         // focus lost
        assert!(find_must_follow_focus(true, None, Some((0, 1)), false));         // focus gained
        // hold suppresses even a real pane change mid-flight
        assert!(!find_must_follow_focus(true, Some((0, 1)), Some((1, 9)), true));
        assert!(!find_must_follow_focus(true, Some((0, 1)), Some((0, 2)), true));
        // after hold clears, a real pane change still recomputes
        assert!(find_must_follow_focus(true, Some((0, 1)), Some((0, 2)), false));
    }

    #[test]
    fn close_tab_capture_order_drives_find_follow() {
        // pane ids per tab slot before close — do_close_tab must capture
        // identity *before* remove; these cases assert the pure after-close
        // identity against a pre-remove before, through find_must_follow_focus
        let panes = [10usize, 20, 30];

        // close active middle tab: before (1,20) → after lands on old tab 2's pane
        let before = Some((1, panes[1]));
        let after = focus_identity_after_tab_close(&panes, 1, 1);
        assert_eq!(after, Some((1, 30)));
        assert!(find_must_follow_focus(true, before, after, false));

        // close active first tab: before (0,10) → after (0,20)
        let before = Some((0, panes[0]));
        let after = focus_identity_after_tab_close(&panes, 0, 0);
        assert_eq!(after, Some((0, 20)));
        assert!(find_must_follow_focus(true, before, after, false));

        // close active last tab: before (2,30) → after (1,20)
        let before = Some((2, panes[2]));
        let after = focus_identity_after_tab_close(&panes, 2, 2);
        assert_eq!(after, Some((1, 20)));
        assert!(find_must_follow_focus(true, before, after, false));

        // close a tab left of the viewer: same pane id, only tab index shifts
        // — find must NOT recompute (cursor stays put on the live grid)
        let before = Some((2, panes[2]));
        let after = focus_identity_after_tab_close(&panes, 2, 0);
        assert_eq!(after, Some((1, 30)));
        assert!(!find_must_follow_focus(true, before, after, false));

        // regression: if before were captured *after* remove while still
        // pointing at the new occupant of the active slot, middle-active close
        // would compare (1,30) to (1,30) and skip recompute — the bug 09a609e
        // introduced. prove that same-after path does not force follow
        assert!(!find_must_follow_focus(true, Some((1, 30)), Some((1, 30)), false));
    }

    #[test]
    fn background_close_restores_viewer_active_tab() {
        // viewer left of owner, owner tab removed: index unchanged
        assert_eq!(restore_viewer_tab(0, 2, 2, true), Some(0));
        // viewer right of owner, owner tab removed: shift left
        assert_eq!(restore_viewer_tab(2, 0, 2, true), Some(1));
        // viewer right of owner, only a pane closed (tab stays): no shift
        assert_eq!(restore_viewer_tab(2, 0, 3, false), Some(2));
        // viewer was the owner: leave close's active alone
        assert_eq!(restore_viewer_tab(1, 1, 2, true), None);
        // empty window
        assert_eq!(restore_viewer_tab(0, 0, 0, true), None);
    }

    #[test]
    fn find_after_grid_change_snaps_to_first_match() {
        let hits = vec![(3, 0), (5, 2), (9, 1)];
        let (m, cur) = find_after_grid_change(hits.clone());
        assert_eq!(m, hits);
        assert_eq!(cur, 0);
        // empty result still resets the cursor so next/prev do not wrap a stale index
        let (m2, cur2) = find_after_grid_change(Vec::<(usize, usize)>::new());
        assert!(m2.is_empty());
        assert_eq!(cur2, 0);
    }

    #[test]
    fn fuzzy_palette_matches_and_ranks() {
        assert!(fuzzy_score("xyz", "new tab").is_none());
        assert!(fuzzy_score("nt", "new tab").is_some());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        // a word-boundary/contiguous match outranks a scattered subsequence
        let prefix = fuzzy_score("set", "settings").unwrap();
        let scattered = fuzzy_score("set", "split vertical").unwrap();
        assert!(prefix > scattered);
        // filter returns only subsequence matches, best-first, and a clean prefix wins
        let r = palette_filter("split");
        assert!(r.iter().all(|(l, _)| fuzzy_score("split", l).is_some()));
        assert!(r.first().map(|(l, _)| l.starts_with("split")).unwrap_or(false));
        assert_eq!(palette_filter("").len(), all_palette_actions().len());
    }

    #[test]
    fn tab_search_numbers_duplicates_and_filters_fuzzily() {
        let labels = vec!["repo".to_string(), "repo".to_string(), "logs".to_string()];
        assert_eq!(
            tab_filter("", &labels),
            vec![
                ("1  repo".to_string(), 0),
                ("2  repo".to_string(), 1),
                ("3  logs".to_string(), 2),
            ]
        );
        assert_eq!(tab_filter("2", &labels), vec![("2  repo".to_string(), 1)]);
        assert_eq!(tab_filter("lg", &labels), vec![("3  logs".to_string(), 2)]);
    }

    #[test]
    #[cfg(windows)]
    fn parse_quake_key_forms() {
        // MOD_NOREPEAT(0x4000) is always set; ctrl=0x2, shift=0x4, alt=0x1
        // ctrl+grave -> VK_OEM_3 (0xC0)
        assert_eq!(parse_quake_key("ctrl+grave"), Some((0x4002, 0xC0)));
        // letters map to their uppercase ascii == virtual-key
        assert_eq!(parse_quake_key("ctrl+shift+t"), Some((0x4006, 0x54)));
        // f-keys -> 0x70.. ; alt only
        assert_eq!(parse_quake_key("alt+f12"), Some((0x4001, 0x7B)));
        // a bare key (no real modifier) is rejected so it can't swallow the key
        assert_eq!(parse_quake_key("grave"), None);
        assert_eq!(parse_quake_key(""), None);
        assert_eq!(parse_quake_key("ctrl+nonsense"), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn parse_quake_portal_triggers() {
        assert_eq!(quake_portal_trigger("ctrl+grave").as_deref(), Some("CTRL+grave"));
        assert_eq!(quake_portal_trigger("ctrl+shift+t").as_deref(), Some("CTRL+SHIFT+t"));
        assert_eq!(quake_portal_trigger("alt+f12").as_deref(), Some("ALT+F12"));
        assert_eq!(quake_portal_trigger("super+enter").as_deref(), Some("LOGO+Return"));
        assert_eq!(quake_portal_trigger("grave"), None);
        assert_eq!(quake_portal_trigger("ctrl+nonsense"), None);
        assert_eq!(quake_portal_trigger("ctrl+t+x"), None);
    }

    // ---- headless pane-tree harness ----
    // build real Panes (with a no-op pty) so the split/close/swap/layout logic
    // can be exercised without a window or a shell

    fn tp(id: usize) -> Pane {
        Pane {
            id,
            term: Terminal::new(24, 80),
            parser: Parser::new(),
            pty: pty::Pty::null(),
            shell: ShellKind::Auto,
            ready: true,
            flash: None,
            apc: apc::ApcScanner::default(),
            status: PaneStatus::Idle,
        }
    }
    fn leaf(id: usize) -> Node {
        Node::Leaf(tp(id))
    }
    fn split(dir: Dir, ratio: f32, a: Node, b: Node) -> Node {
        Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }
    }
    fn ids(node: &Node) -> Vec<usize> {
        let mut v = Vec::new();
        each_pane(node, &mut |p| v.push(p.id));
        v
    }

    #[test]
    fn split_rects_partitions_without_gaps_or_overlap() {
        let rect = (0.0, 0.0, 100.0, 60.0);
        let (a, b) = split_rects(Dir::Vertical, rect, 0.5);
        // left + right tile the width exactly, share full height, no overlap
        assert_eq!(a, (0.0, 0.0, 50.0, 60.0));
        assert_eq!(b, (50.0, 0.0, 50.0, 60.0));
        assert_eq!(a.2 + b.2, rect.2);
        let (t, btm) = split_rects(Dir::Horizontal, rect, 0.25);
        assert_eq!(t, (0.0, 0.0, 100.0, 15.0));
        assert_eq!(btm, (0.0, 15.0, 100.0, 45.0));
        assert_eq!(t.3 + btm.3, rect.3);
        // an extreme ratio is clamped so neither side collapses to zero
        let (a2, b2) = split_rects(Dir::Vertical, rect, 0.0);
        assert!(a2.2 >= 1.0 && b2.2 >= 1.0);
    }

    #[test]
    fn layout_covers_every_leaf_once_and_tiles_the_rect() {
        // a vertical split whose right child is itself split horizontally
        let tree = split(
            Dir::Vertical,
            0.5,
            leaf(1),
            split(Dir::Horizontal, 0.5, leaf(2), leaf(3)),
        );
        let mut out = Vec::new();
        layout(&tree, (0.0, 0.0, 100.0, 80.0), &mut out);
        let got: Vec<usize> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(got, vec![1, 2, 3]);
        // total leaf area equals the parent rect area (a tiling, no overlap/gap)
        let area: f32 = out.iter().map(|(_, r)| r.2 * r.3).sum();
        assert!((area - 100.0 * 80.0).abs() < 1.0, "covered area {area}");
        // pane 1 owns the left half; 2 and 3 share the right half stacked
        assert_eq!(out[0].1, (0.0, 0.0, 50.0, 80.0));
        assert_eq!(out[1].1.0, 50.0);
        assert_eq!(out[2].1.0, 50.0);
    }

    #[test]
    fn close_pane_promotes_the_sibling() {
        let tree = split(Dir::Vertical, 0.5, leaf(1), split(Dir::Horizontal, 0.5, leaf(2), leaf(3)));
        // closing pane 2 collapses its split so 3 takes the whole right side
        let after = close_pane(tree, 2).expect("tree not empty");
        assert_eq!(ids(&after), vec![1, 3]);
        // the surviving structure is a single vertical split (1 | 3)
        match &after {
            Node::Split { a, b, .. } => {
                assert!(matches!(**a, Node::Leaf(ref p) if p.id == 1));
                assert!(matches!(**b, Node::Leaf(ref p) if p.id == 3));
            }
            _ => panic!("expected a split, got a leaf"),
        }
        // closing the last pane yields an empty tree
        assert!(close_pane(leaf(7), 7).is_none());
        // closing a missing id leaves the tree intact
        let keep = close_pane(leaf(7), 99).expect("kept");
        assert_eq!(ids(&keep), vec![7]);
    }

    #[test]
    fn extract_pane_hands_back_the_pane_and_collapses_the_tree() {
        // pop pane 2 out: the remaining tree collapses like a close, but the
        // pane comes back alive (its pty isn't killed) for the new window
        let tree = split(Dir::Vertical, 0.5, leaf(1), split(Dir::Horizontal, 0.5, leaf(2), leaf(3)));
        let mut popped = None;
        let rest = extract_pane(tree, 2, &mut popped).expect("tree not empty");
        assert_eq!(popped.map(|p| p.id), Some(2));
        // 1 and 3 remain, 3 promoted into the collapsed right split
        assert_eq!(ids(&rest), vec![1, 3]);
        // popping the only pane leaves an empty tree but still yields the pane
        let mut popped = None;
        assert!(extract_pane(leaf(9), 9, &mut popped).is_none());
        assert_eq!(popped.map(|p| p.id), Some(9));
        // a missing id extracts nothing and keeps the tree
        let mut popped = None;
        let kept = extract_pane(leaf(9), 42, &mut popped).expect("kept");
        assert!(popped.is_none());
        assert_eq!(ids(&kept), vec![9]);
    }

    #[test]
    fn insert_pane_places_the_leaf_on_each_requested_side() {
        for (side, dir, expected) in [
            (PaneDropSide::Left, Dir::Vertical, vec![2, 1]),
            (PaneDropSide::Right, Dir::Vertical, vec![1, 2]),
            (PaneDropSide::Top, Dir::Horizontal, vec![2, 1]),
            (PaneDropSide::Bottom, Dir::Horizontal, vec![1, 2]),
        ] {
            let mut pane = Some(tp(2));
            let tree = insert_pane(leaf(1), 1, &mut pane, side);
            assert!(pane.is_none());
            assert_eq!(ids(&tree), expected);
            assert!(matches!(tree, Node::Split { dir: got, .. } if got == dir));
        }
    }

    #[test]
    fn pane_drop_side_uses_the_nearest_edge() {
        let rect = (10.0, 20.0, 200.0, 100.0);
        assert_eq!(pane_drop_side(rect, 12.0, 70.0), PaneDropSide::Left);
        assert_eq!(pane_drop_side(rect, 208.0, 70.0), PaneDropSide::Right);
        assert_eq!(pane_drop_side(rect, 110.0, 22.0), PaneDropSide::Top);
        assert_eq!(pane_drop_side(rect, 110.0, 118.0), PaneDropSide::Bottom);
    }

    #[test]
    fn pane_move_collapses_its_old_split_and_docks_live_leaf() {
        let tree = split(Dir::Vertical, 0.5, leaf(1), split(Dir::Horizontal, 0.5, leaf(2), leaf(3)));
        let mut moved = None;
        let tree = extract_pane(tree, 2, &mut moved).expect("surviving tree");
        let tree = insert_pane(tree, 1, &mut moved, PaneDropSide::Left);
        assert!(moved.is_none());
        assert_eq!(ids(&tree), vec![2, 1, 3]);
        assert!(matches!(tree, Node::Split { dir: Dir::Vertical, .. }));
    }

    #[test]
    fn grow_focused_adjusts_the_matching_split_in_the_right_direction() {
        // a vertical split (left | right); growing the left pane raises A's ratio
        let mut tree = split(Dir::Vertical, 0.5, leaf(1), leaf(2));
        let mut done = false;
        assert!(grow_focused(&mut tree, 1, Dir::Vertical, true, 0.1, &mut done));
        assert!(done);
        if let Node::Split { ratio, .. } = &tree {
            assert!((*ratio - 0.6).abs() < 1e-6, "ratio {ratio}");
        } else {
            panic!("expected split");
        }
        // growing the right pane (B side) lowers A's ratio
        let mut done = false;
        grow_focused(&mut tree, 2, Dir::Vertical, true, 0.1, &mut done);
        if let Node::Split { ratio, .. } = &tree {
            assert!((*ratio - 0.5).abs() < 1e-6, "ratio {ratio}");
        }
        // a resize whose axis doesn't match the split orientation is ignored
        let mut done = false;
        grow_focused(&mut tree, 1, Dir::Horizontal, true, 0.1, &mut done);
        assert!(!done);
        // the ratio can't drive a pane below the 0.1 floor
        let mut tree = split(Dir::Vertical, 0.15, leaf(1), leaf(2));
        let mut done = false;
        grow_focused(&mut tree, 1, Dir::Vertical, false, 0.1, &mut done);
        if let Node::Split { ratio, .. } = &tree {
            assert!(*ratio >= 0.1, "ratio {ratio} below floor");
        }
    }

    #[test]
    fn handle_kitty_transmit_display_and_delete_all() {
        let mut term = term::Terminal::new(4, 8);
        // a=T transmits + displays a 1x1 RGBA image scaled to a 3x2 cell box
        // and (quiet 0) queues an OK ack
        let cmd = apc::KittyCmd {
            action: b'T',
            format: 32,
            width: 1,
            height: 1,
            id: 5,
            cols: 3,
            rows: 2,
            z: 0,
            delete: 0,
            x: 0,
            y: 0,
            more: false,
            no_cursor_move: false,
            unicode_placeholder: false,
            quiet: 0,
            payload: vec![1, 2, 3, 4],
        };
        handle_kitty(&mut term, &cmd);
        assert_eq!(term.grid.placements().len(), 1);
        assert_eq!((term.grid.placements()[0].cols, term.grid.placements()[0].rows), (3, 2));
        // the cursor steps past the box like text: right 3, down onto row 2's last line
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (1, 3));
        assert!(!term.responses.is_empty(), "OK ack should be queued");
        // bare a=d (no id) deletes all placements
        let del = apc::KittyCmd {
            action: b'd',
            format: 0,
            width: 0,
            height: 0,
            id: 0,
            cols: 0,
            rows: 0,
            z: 0,
            delete: 0,
            x: 0,
            y: 0,
            more: false,
            no_cursor_move: false,
            unicode_placeholder: false,
            quiet: 0,
            payload: vec![],
        };
        handle_kitty(&mut term, &del);
        assert!(term.grid.placements().is_empty());
    }

    fn kitty_display(id: u32, cols: u32, rows: u32, no_move: bool) -> apc::KittyCmd {
        apc::KittyCmd {
            action: b'T',
            format: 32,
            width: 1,
            height: 1,
            id,
            cols,
            rows,
            z: 0,
            delete: 0,
            x: 0,
            y: 0,
            more: false,
            no_cursor_move: no_move,
            unicode_placeholder: false,
            quiet: 2,
            payload: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn kitty_cursor_policy_c1_scroll_and_wrap() {
        // C=1: the cursor must not move at all
        let mut term = term::Terminal::new(4, 8);
        handle_kitty(&mut term, &kitty_display(1, 3, 2, true));
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (0, 0));

        // a 3-row box placed on the bottom row scrolls two lines, exactly like
        // printing three lines of text there would
        let mut term = term::Terminal::new(4, 8);
        term.grid.cursor.row = 3;
        handle_kitty(&mut term, &kitty_display(2, 2, 3, false));
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (3, 2));
        assert_eq!(term.grid.scrollback.len(), 2, "the advance scrolled");
        // the placement anchor scrolled up with its surrounding text
        assert_eq!(term.grid.placements()[0].abs_line, 3);

        // a box crossing the right edge wraps to column 0 one row further down
        let mut term = term::Terminal::new(4, 8);
        term.grid.cursor.col = 6;
        handle_kitty(&mut term, &kitty_display(3, 3, 1, false));
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (1, 0));
    }

    #[test]
    fn kitty_chunked_transfer_advances_once_on_completion() {
        let mut term = term::Terminal::new(6, 20);
        // first chunk carries a=T + the box; more=1 so nothing shows yet
        let mut first = kitty_display(9, 4, 2, false);
        first.more = true;
        first.payload = vec![1, 2];
        handle_kitty(&mut term, &first);
        assert!(term.grid.placements().is_empty());
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (0, 0));
        // the completing chunk parses with default action 't' and no box; the
        // stored display intent must still place AND step the cursor
        let done = apc::KittyCmd {
            action: b't',
            format: 0,
            width: 0,
            height: 0,
            id: 9,
            cols: 0,
            rows: 0,
            z: 0,
            delete: 0,
            x: 0,
            y: 0,
            more: false,
            no_cursor_move: false,
            unicode_placeholder: false,
            quiet: 2,
            payload: vec![3, 4],
        };
        handle_kitty(&mut term, &done);
        assert_eq!(term.grid.placements().len(), 1);
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (1, 4));
    }

    // position-scoped deletes hit by 1-based cell coordinates: p by cell,
    // q additionally by z, x by column, y by row; an absent coordinate (0)
    // matches nothing instead of everything
    #[test]
    fn kitty_position_deletes_hit_by_cell_column_row_and_z() {
        let mut term = term::Terminal::new(6, 20);
        // a 2x2-cell box at the origin (id 1) and one at column 5 (id 2)
        handle_kitty(&mut term, &kitty_display(1, 2, 2, true));
        term.grid.cursor.col = 5;
        handle_kitty(&mut term, &kitty_display(2, 2, 2, true));
        // d=p at 1-based cell (6, 1) = column 5, row 0: id 2 only
        let mut del = kitty_del(b'p', 0, 0);
        (del.x, del.y) = (6, 1);
        handle_kitty(&mut term, &del);
        assert_eq!(term.grid.placements().len(), 1);
        assert_eq!(term.grid.placements()[0].image_id, 1);
        // d=p with an absent y key must not wipe anything
        let mut del = kitty_del(b'p', 0, 0);
        del.x = 1;
        handle_kitty(&mut term, &del);
        assert_eq!(term.grid.placements().len(), 1);
        // d=x at 1-based column 2 crosses id 1's box (columns 0..2)
        let mut del = kitty_del(b'x', 0, 0);
        del.x = 2;
        handle_kitty(&mut term, &del);
        assert!(term.grid.placements().is_empty());
        // two boxes on one cell, different z: d=q takes only the named layer,
        // then d=y clears the row
        let mut low = kitty_display(3, 1, 1, true);
        low.z = -4;
        handle_kitty(&mut term, &low);
        handle_kitty(&mut term, &kitty_display(4, 1, 1, true));
        let mut del = kitty_del(b'q', 0, -4);
        (del.x, del.y) = (6, 1);
        handle_kitty(&mut term, &del);
        assert_eq!(term.grid.placements().len(), 1);
        assert_eq!(term.grid.placements()[0].image_id, 4);
        let mut del = kitty_del(b'y', 0, 0);
        del.y = 1;
        handle_kitty(&mut term, &del);
        assert!(term.grid.placements().is_empty());
    }

    // d=r wipes the id range [x, y] — placements and virtual boxes both —
    // and only the uppercase form frees the pixels
    #[test]
    fn kitty_ranged_delete_reaches_virtual_placements() {
        let mut term = term::Terminal::new(6, 20);
        for id in [3u32, 5, 9] {
            handle_kitty(&mut term, &kitty_display(id, 1, 1, false));
        }
        let mut virt = kitty_display(5, 2, 2, false);
        virt.unicode_placeholder = true;
        handle_kitty(&mut term, &virt);
        // x=4 y=8 catches only id 5: its placement and virtual box go, the
        // pixels stay (lowercase)
        let mut del = kitty_del(b'r', 0, 0);
        (del.x, del.y) = (4, 8);
        handle_kitty(&mut term, &del);
        assert!(term.grid.placements().iter().all(|p| p.image_id != 5));
        assert_eq!(term.grid.virtual_placement(5), None);
        assert!(term.images.get(5).is_some(), "lowercase keeps the pixels");
        // uppercase R frees them; neighbors outside the range survive
        let mut del = kitty_del(b'R', 0, 0);
        (del.x, del.y) = (5, 5);
        handle_kitty(&mut term, &del);
        assert!(term.images.get(5).is_none());
        assert!(term.images.get(3).is_some() && term.images.get(9).is_some());
    }

    // a U=1 placement is a prototype for placeholder cells: it paints nothing,
    // holds the cursor, survives delete-all, and pins the image's pixels
    #[test]
    fn kitty_virtual_placement_lifecycle() {
        let mut term = term::Terminal::new(4, 8);
        let mut virt = kitty_display(4, 3, 2, false);
        virt.unicode_placeholder = true;
        handle_kitty(&mut term, &virt);
        assert!(term.grid.placements().is_empty(), "nothing paints");
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (0, 0), "cursor holds");
        assert_eq!(term.grid.virtual_placement(4), Some((3, 2)));

        // a regular placement of the same image, then delete-all: the regular
        // placement dies, the virtual one survives and keeps the pixels alive
        handle_kitty(&mut term, &kitty_display(4, 1, 1, false));
        handle_kitty(&mut term, &kitty_del(b'A', 0, 0));
        assert!(term.grid.placements().is_empty());
        assert_eq!(term.grid.virtual_placement(4), Some((3, 2)), "A never reaches U=1");
        assert!(term.images.get(4).is_some(), "the virtual reference pins the pixels");

        // a=p,U=1 re-boxes the stored image without painting
        let mut put = kitty_display(4, 5, 6, true);
        put.action = b'p';
        put.payload = vec![];
        put.unicode_placeholder = true;
        handle_kitty(&mut term, &put);
        assert_eq!(term.grid.virtual_placement(4), Some((5, 6)));
        assert!(term.grid.placements().is_empty());

        // d=I reaches virtual placements and frees the now-unreferenced pixels
        handle_kitty(&mut term, &kitty_del(b'I', 4, 0));
        assert_eq!(term.grid.virtual_placement(4), None);
        assert!(term.images.get(4).is_none());
    }

    fn kitty_del(delete: u8, id: u32, z: i32) -> apc::KittyCmd {
        apc::KittyCmd {
            action: b'd',
            format: 0,
            width: 0,
            height: 0,
            id,
            cols: 0,
            rows: 0,
            z,
            delete,
            x: 0,
            y: 0,
            more: false,
            no_cursor_move: false,
            unicode_placeholder: false,
            quiet: 2,
            payload: vec![],
        }
    }

    #[test]
    fn kitty_delete_subtargets_scope_placements_and_data() {
        let mut term = term::Terminal::new(6, 20);
        handle_kitty(&mut term, &kitty_display(1, 1, 1, false));
        let mut low = kitty_display(2, 1, 1, false);
        low.z = -4;
        handle_kitty(&mut term, &low);
        assert_eq!(term.grid.placements().len(), 2);

        // lowercase d=z drops only that layer's placements, data stays
        handle_kitty(&mut term, &kitty_del(b'z', 0, -4));
        assert_eq!(term.grid.placements().len(), 1);
        assert!(term.images.get(2).is_some(), "lowercase keeps the pixels");

        // uppercase d=C at the cursor cell frees the covering image too
        term.grid.cursor.row = 0;
        term.grid.cursor.col = 0;
        handle_kitty(&mut term, &kitty_del(b'C', 0, 0));
        assert!(term.grid.placements().is_empty());
        assert!(term.images.get(1).is_none(), "uppercase frees the pixels");

        // bare a=d (no d=, no i=) clears every placement but keeps the data
        handle_kitty(&mut term, &kitty_display(3, 1, 1, false));
        handle_kitty(&mut term, &kitty_del(0, 0, 0));
        assert!(term.grid.placements().is_empty());
        assert!(term.images.get(3).is_some(), "spec default 'a' keeps the pixels");

        // the legacy i=-only form still frees the image (old termie behavior)
        handle_kitty(&mut term, &kitty_display(4, 1, 1, false));
        handle_kitty(&mut term, &kitty_del(0, 4, 0));
        assert!(term.grid.placements().is_empty());
        assert!(term.images.get(4).is_none());

        // an unimplemented target (d=p) must not escalate to a wipe
        handle_kitty(&mut term, &kitty_display(5, 1, 1, false));
        handle_kitty(&mut term, &kitty_del(b'p', 0, 0));
        assert_eq!(term.grid.placements().len(), 1, "scoped delete never wipes");

        // an uppercase scoped delete keeps pixels a surviving placement of
        // the same image still needs
        let mut second = kitty_display(5, 1, 1, false);
        second.action = b'p';
        second.z = -2;
        second.payload = vec![];
        handle_kitty(&mut term, &second);
        assert_eq!(term.grid.placements().len(), 2, "image 5 placed twice");
        handle_kitty(&mut term, &kitty_del(b'Z', 0, -2));
        assert_eq!(term.grid.placements().len(), 1);
        assert!(term.images.get(5).is_some(), "the z=0 placement still references it");
        // once the last placement goes, the uppercase free proceeds
        handle_kitty(&mut term, &kitty_del(b'Z', 0, 0));
        assert!(term.grid.placements().is_empty());
        assert!(term.images.get(5).is_none());
    }

    #[test]
    fn kitty_put_advances_and_falls_back_to_pixel_size() {
        let mut term = term::Terminal::new(6, 20);
        // store without displaying (a=t), then a=p with no c=/r= box: the
        // advance derives 1 cell from the 1x1 px image over the assumed cell
        let mut store = kitty_display(5, 0, 0, false);
        store.action = b't';
        handle_kitty(&mut term, &store);
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (0, 0));
        let mut put = kitty_display(5, 0, 0, false);
        put.action = b'p';
        put.payload = vec![];
        handle_kitty(&mut term, &put);
        assert_eq!(term.grid.placements().len(), 1);
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (0, 1));
    }

    #[test]
    fn kitty_single_axis_box_advances_by_the_scaled_size() {
        // a 1x2 px image with c=4 draws 4 cols wide and, aspect-scaled at the
        // assumed 10x20 cell, 4 rows tall (4*10px wide -> 80px tall); the
        // cursor must land on that scaled box's last row, not the 1-row box
        // the raw pixel height suggests
        let mut term = term::Terminal::new(8, 20);
        let mut cmd = kitty_display(6, 4, 0, false);
        cmd.width = 1;
        cmd.height = 2;
        cmd.payload = vec![0; 8];
        handle_kitty(&mut term, &cmd);
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (3, 4));

        // the symmetric r-only case scales the columns: 2x1 px with r=2 is
        // 40px tall -> 80px wide -> 8 cols
        let mut term = term::Terminal::new(8, 20);
        let mut cmd = kitty_display(7, 0, 2, false);
        cmd.width = 2;
        cmd.height = 1;
        cmd.payload = vec![0; 8];
        handle_kitty(&mut term, &cmd);
        assert_eq!((term.grid.cursor.row, term.grid.cursor.col), (1, 8));
    }
}
