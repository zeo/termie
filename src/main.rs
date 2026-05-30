// no extra console window in release; keep one in debug for logs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod color;
mod grid;
mod input;
mod plugin;
mod pty;
mod render;
mod term;
mod win;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, Window, WindowId};

use pty::{Pty, PtyMsg, ShellKind};
use render::{Hit, Hot, PaneView, Renderer};
use term::Terminal;

const CONTENT_PT: f32 = 14.0;
const CHROME_PT: f32 = 12.5;
/// pre-warmed shells kept ready so splits/tabs open instantly
const POOL_TARGET: usize = 3;
/// stop respawning after this many consecutive shell-spawn failures so a broken
/// shell can't peg a CPU core; the window then stays up (logged) instead
const MAX_WARM_FAILS: usize = 10;

type Rect = (f32, f32, f32, f32);

enum UserEvent {
    Pty { id: usize, bytes: Vec<u8> },
    Exited { id: usize },
    /// a pool shell finished spawning on a worker thread (None = spawn failed)
    PaneReady(Option<Box<Pane>>),
    /// a plugin process emitted a protocol message (id = plugin index)
    Plugin { id: usize, msg: plugin::PluginMsg },
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
    /// true once the shell has produced output (prompt up) — safe to resize
    ready: bool,
    /// set when the shell rang the bell (BEL); drives a brief border flash
    flash: Option<Instant>,
}

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
}

/// an active text selection within one pane's viewport (row, col)
#[derive(Clone, Copy)]
struct Sel {
    pane: usize,
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Clone, Copy)]
enum PaletteAction {
    NewTab,
    SplitV,
    SplitH,
    NextTab,
    PrevTab,
    CloseTab,
    Settings,
    PaneMode,
    Theme,
    Quit,
}

const PALETTE_ACTIONS: &[(&str, PaletteAction)] = &[
    ("new tab", PaletteAction::NewTab),
    ("split vertical", PaletteAction::SplitV),
    ("split horizontal", PaletteAction::SplitH),
    ("next tab", PaletteAction::NextTab),
    ("previous tab", PaletteAction::PrevTab),
    ("close tab", PaletteAction::CloseTab),
    ("settings", PaletteAction::Settings),
    ("pane mode", PaletteAction::PaneMode),
    ("cycle theme", PaletteAction::Theme),
    ("quit", PaletteAction::Quit),
];

fn palette_filter(query: &str) -> Vec<(&'static str, PaletteAction)> {
    let q = query.to_ascii_lowercase();
    PALETTE_ACTIONS
        .iter()
        .filter(|(label, _)| q.is_empty() || label.to_ascii_lowercase().contains(&q))
        .copied()
        .collect()
}

struct PaletteState {
    query: String,
    selected: usize,
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

/// derive a short tab/title label from an OSC-7 cwd uri (e.g. file:///C:/Users/miko -> miko)
fn cwd_label(cwd: Option<&str>) -> String {
    let Some(u) = cwd else {
        return "pwsh".to_string();
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
        "pwsh".to_string()
    } else {
        seg
    }
}

/// turn an OSC-7 file:// uri into a filesystem path (forward slashes are fine for std::fs)
fn cwd_path(cwd: Option<&str>) -> Option<String> {
    let u = cwd?;
    let path = u
        .strip_prefix("file://")
        .map(|r| match r.find('/') {
            Some(i) => &r[i..],
            None => r,
        })
        .unwrap_or(u);
    Some(path.strip_prefix('/').unwrap_or(path).replace("%20", " "))
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
            if p.id == id {
                if let Some(np) = new.take() {
                    return Node::Split {
                        dir,
                        ratio: 0.5,
                        a: Box::new(Node::Leaf(p)),
                        b: Box::new(Node::Leaf(np)),
                    };
                }
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

/// swap the panes of two distinct leaves (by id) in place
fn swap_panes(node: &mut Node, a: usize, b: usize) {
    if a == b {
        return;
    }
    fn collect(n: &mut Node, a: usize, b: usize, pa: &mut *mut Pane, pb: &mut *mut Pane) {
        match n {
            Node::Leaf(p) => {
                if p.id == a {
                    *pa = p;
                } else if p.id == b {
                    *pb = p;
                }
            }
            Node::Split { a: x, b: y, .. } => {
                collect(x, a, b, pa, pb);
                collect(y, a, b, pa, pb);
            }
        }
    }
    let mut pa: *mut Pane = std::ptr::null_mut();
    let mut pb: *mut Pane = std::ptr::null_mut();
    collect(node, a, b, &mut pa, &mut pb);
    if !pa.is_null() && !pb.is_null() {
        // safety: a != b and pane ids are unique, so pa/pb point to distinct Panes
        unsafe { std::mem::swap(&mut *pa, &mut *pb) };
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
fn discover_plugins() -> Vec<(String, String, Vec<String>)> {
    let Some(base) = config_path().and_then(|p| p.parent().map(|d| d.join("plugins"))) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = dir.join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Some(json) = plugin::Json::parse(&text) else {
            log::warn!("plugin manifest {} is not valid json", manifest.display());
            continue;
        };
        let id = json
            .get_str("id")
            .map(str::to_string)
            .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .unwrap_or_default();
        // entry: { "cmd": "program", "args": ["..."] } — args optional. the
        // program is resolved relative to the plugin dir if not absolute
        let Some(entry_obj) = json.get("entry") else {
            log::warn!("plugin {id} manifest has no entry");
            continue;
        };
        let Some(cmd) = entry_obj.get_str("cmd") else {
            log::warn!("plugin {id} entry has no cmd");
            continue;
        };
        let prog = {
            let p = std::path::Path::new(cmd);
            if p.is_absolute() {
                cmd.to_string()
            } else {
                dir.join(cmd).to_string_lossy().into_owned()
            }
        };
        let args = entry_obj
            .get("args")
            .and_then(plugin::Json::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        out.push((id, prog, args));
    }
    out
}

/// build a pane (pty + child + screen) without starting its reader thread.
/// the slow part (process spawn) — safe to run off the main thread
fn build_pane(
    id: usize,
    cols: usize,
    rows: usize,
    shell: ShellKind,
    load_profile: bool,
    scrollback: usize,
) -> Result<Pane> {
    let pty = Pty::spawn(rows as u16, cols as u16, shell, load_profile)?;
    let mut term = Terminal::new(rows, cols);
    term.grid.set_scrollback_limit(scrollback);
    Ok(Pane {
        id,
        term,
        parser: Parser::new(),
        pty,
        ready: false,
        flash: None,
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

/// user-tunable settings backed by real effects (see the settings page)
#[derive(Clone, Copy)]
struct Config {
    scrollback: usize,
    copy_on_select: bool,
    shell: ShellKind,
    load_profile: bool,
    close_action: CloseAction,
    backend: render::BackendChoice,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            scrollback: 10_000,
            copy_on_select: false,
            shell: ShellKind::Auto,
            load_profile: false,
            close_action: CloseAction::Quit,
            backend: render::BackendChoice::Auto,
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
    backend: render::BackendChoice,
    font_size: f32,
    padding: f32,
    cursor: grid::CursorShape,
    cursor_blink: bool,
    theme: color::ThemeId,
    font: Option<String>,
    opacity: i32,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            scrollback: 10_000,
            copy_on_select: false,
            shell: ShellKind::Auto,
            load_profile: false,
            close_action: CloseAction::Quit,
            backend: render::BackendChoice::Auto,
            font_size: CONTENT_PT,
            padding: 6.0,
            cursor: grid::CursorShape::Bar,
            cursor_blink: true,
            theme: color::ThemeId::Instrument,
            font: None,
            opacity: 85,
        }
    }
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
    matches!(
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
            | Hot::ScrollbackDec
            | Hot::ScrollbackInc
            | Hot::CopyOnSelect
            | Hot::ShellCycle
            | Hot::LoadProfile
            | Hot::CloseActionCycle
            | Hot::BackendCycle
    )
}

/// %APPDATA%\termie\config — a simple key=value store for every setting
fn config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(base).join("termie").join("config"))
}

fn load_persisted() -> Persisted {
    let mut p = Persisted::default();
    let Some(path) = config_path() else {
        return p;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return p;
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "scrollback" => {
                if let Ok(n) = v.parse() {
                    p.scrollback = n;
                }
            }
            "copy_on_select" => p.copy_on_select = v == "true",
            "shell" => p.shell = ShellKind::from_label(v),
            "load_profile" => p.load_profile = v == "true",
            "close_action" => p.close_action = CloseAction::from_label(v),
            "backend" => p.backend = render::BackendChoice::from_label(v),
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
            "theme" => p.theme = color::ThemeId::from_name(v),
            "font" => {
                if !v.is_empty() {
                    p.font = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    p
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_id: usize,
    layout_cache: Vec<(usize, Rect)>,
    mods: ModifiersState,
    focused: bool,
    maximized: bool,
    pane_mode: bool,
    shown: bool,
    pool: Vec<Pane>,
    selection: Option<Sel>,
    selecting: bool,
    last_click: Option<(Instant, f64, f64)>,
    git: Option<String>,
    palette: Option<PaletteState>,
    cursor: PhysicalPosition<f64>,
    pressed: Option<Hot>,
    last_title: String,
    config: Config,
    settings_open: bool,
    settings_anim: Option<Instant>,
    /// pool shells currently spawning on worker threads (not yet in `pool`)
    pending_warm: usize,
    /// set once the app is exiting so no new shells are spawned during teardown
    shutting_down: bool,
    /// pane-mode drag state: a divider being resized (path) or a pane being moved
    drag_divider: Option<Vec<usize>>,
    drag_pane: Option<usize>,
    /// persisted settings loaded at startup; renderer-owned ones applied in boot
    persisted: Persisted,
    /// last cwd we computed a git branch for (skip the FS walk when unchanged)
    last_git_cwd: Option<String>,
    /// broadcast input: typed keys go to every pane in the active tab
    broadcast: bool,
    /// button + pane that received a forwarded press; drag motion and release
    /// stay locked to this pane even if the cursor leaves it
    mouse_down: Option<(u8, usize)>,
    /// last OS pointer icon set, to avoid redundant set_cursor calls
    cursor_icon: CursorIcon,
    /// url under the cursor while ctrl is held, to underline + open on click:
    /// (focused-pane viewport row, col_start, col_end exclusive)
    link: Option<(usize, usize, usize)>,
    /// system fonts are scanned lazily after first paint to keep startup fast
    system_fonts_pending: bool,
    /// consecutive failed pool spawns; backs off + gives up so a broken shell
    /// can't spin a busy respawn loop with a permanently empty window
    warm_fails: usize,
    warm_backoff_until: Option<Instant>,
    /// set when a paint is deferred because a pane is mid synchronized-output
    /// (DEC 2026) frame; the safety deadline forces a paint if the frame stalls
    sync_redraw_pending: Option<Instant>,
    /// running plugin processes (out-of-process, supervised); spawned deferred
    /// after the window is shown so disabled/no plugins cost nothing at boot
    plugins: Vec<plugin::Plugin>,
    plugins_started: bool,
    /// manifest id per plugin, parallel to `plugins`, used as the `from` on
    /// bus messages so subscribers know who published
    plugin_ids: Vec<String>,
    /// declared Tier-1 widgets keyed by (plugin index, widget id) so two
    /// plugins can't clobber each other; rendered in the right-side dock
    plugin_widgets: Vec<(usize, String, render::DockWidget)>,
    /// in-process bus subscriptions: (subscriber plugin index, topic); topic
    /// "*" matches every published topic
    plugin_subs: Vec<(usize, String)>,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let p = load_persisted();
        App {
            proxy,
            window: None,
            renderer: None,
            tabs: Vec::new(),
            active_tab: 0,
            next_id: 0,
            layout_cache: Vec::new(),
            mods: ModifiersState::empty(),
            focused: true,
            maximized: false,
            pane_mode: false,
            shown: false,
            pool: Vec::new(),
            selection: None,
            selecting: false,
            last_click: None,
            git: None,
            palette: None,
            cursor: PhysicalPosition::new(0.0, 0.0),
            pressed: None,
            last_title: String::new(),
            config: Config {
                scrollback: p.scrollback,
                copy_on_select: p.copy_on_select,
                shell: p.shell,
                load_profile: p.load_profile,
                close_action: p.close_action,
                backend: p.backend,
            },
            persisted: p,
            last_git_cwd: None,
            broadcast: false,
            mouse_down: None,
            cursor_icon: CursorIcon::Default,
            link: None,
            system_fonts_pending: true,
            warm_fails: 0,
            warm_backoff_until: None,
            sync_redraw_pending: None,
            plugins: Vec::new(),
            plugins_started: false,
            plugin_ids: Vec::new(),
            plugin_widgets: Vec::new(),
            plugin_subs: Vec::new(),
            settings_open: false,
            settings_anim: None,
            pending_warm: 0,
            shutting_down: false,
            drag_divider: None,
            drag_pane: None,
        }
    }

    fn boot(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        // start hidden; reveal after the first painted frame to avoid a white flash
        let (irgba, iw, ih) = win::app_icon();
        let icon = winit::window::Icon::from_rgba(irgba, iw, ih).ok();
        let attrs = Window::default_attributes()
            .with_title("termie")
            .with_window_icon(icon)
            .with_decorations(false)
            .with_visible(false)
            .with_inner_size(LogicalSize::new(1000.0, 640.0));
        let window = Arc::new(event_loop.create_window(attrs)?);

        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                win::apply_window_effects(h.hwnd.get());
            }
        }

        let renderer = Renderer::new(window.clone(), CONTENT_PT, CHROME_PT, self.config.backend)?;
        self.window = Some(window.clone());
        self.renderer = Some(renderer);

        // apply persisted renderer-owned settings before sizing the first pane
        {
            let p = &self.persisted;
            if let Some(r) = self.renderer.as_mut() {
                r.set_theme(p.theme);
                r.set_cursor_style(p.cursor);
                r.set_cursor_blink(p.cursor_blink);
                r.set_pane_pad_px(p.padding);
                r.set_opacity_pct(p.opacity);
                if let Some(f) = p.font.as_deref() {
                    r.set_font_by_name(f);
                }
                r.set_content_pt(p.font_size);
            }
        }

        self.active_tab = 0;
        // paint the chrome immediately (no pane yet) and reveal the window, then
        // spawn the first shell asynchronously — pwsh startup never blocks the
        // window appearing. the first pool shell to arrive becomes tab one.
        self.paint();
        window.set_visible(true);
        self.shown = true;
        self.warm_pool();
        window.request_redraw();
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
    fn spawn_pane(&mut self, cols: usize, rows: usize) -> Result<Pane> {
        let id = self.next_id;
        self.next_id += 1;
        let mut pane = build_pane(
            id,
            cols,
            rows,
            self.config.shell,
            self.config.load_profile,
            self.config.scrollback,
        )?;
        self.start_reader(&mut pane);
        Ok(pane)
    }

    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// is there a visible, blinking cursor on the focused pane that needs the
    /// periodic blink tick? (false when blink is off, cursor hidden, or scrolled)
    fn blinking_cursor_on_screen(&self) -> bool {
        let Some(r) = self.renderer.as_ref() else {
            return false;
        };
        if !r.cursor_blink() {
            return false;
        }
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|root| find_pane(root, t.focused)))
            .map(|p| p.term.grid.cursor.visible && p.term.grid.view_offset == 0)
            .unwrap_or(false)
    }

    /// any visible pane mid bell-flash (keeps the tick alive so it fades out)
    fn any_flash(&self) -> bool {
        let Some(root) = self.tabs.get(self.active_tab).and_then(|t| t.root.as_ref()) else {
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
        if let Some(r) = self.renderer.as_ref() {
            let (_, _, cols, rows) = r.pane_metrics(r.content_rect());
            (cols, rows)
        } else {
            (80, 24)
        }
    }

    /// keep one fully-started shell ready so opening a tab feels instant
    fn warm_pool(&mut self) {
        if self.shutting_down || self.renderer.is_none() {
            return;
        }
        // gave up after repeated spawn failures: stop trying (no CPU burn)
        if self.warm_fails >= MAX_WARM_FAILS {
            return;
        }
        // hold off respawning while a backoff from a recent failure is active
        if let Some(t) = self.warm_backoff_until {
            if Instant::now() < t {
                return;
            }
        }
        // build shells on worker threads so the slow pwsh spawn never blocks the
        // event loop; dispatch all that are needed at once so they spawn in
        // parallel and the pool fills fast. each arrives via UserEvent::PaneReady
        let (cols, rows) = self.content_pane_size();
        let (shell, profile, sb) = (self.config.shell, self.config.load_profile, self.config.scrollback);
        while self.pool.len() + self.pending_warm < POOL_TARGET {
            let id = self.next_id;
            self.next_id += 1;
            let proxy = self.proxy.clone();
            self.pending_warm += 1;
            std::thread::spawn(move || {
                let pane = build_pane(id, cols, rows, shell, profile, sb).ok().map(Box::new);
                let _ = proxy.send_event(UserEvent::PaneReady(pane));
            });
        }
    }

    /// kill every pre-warmed shell (called on shutdown); also latches the
    /// shutting-down flag so no further shells are spawned during teardown
    fn kill_pool(&mut self) {
        self.shutting_down = true;
        for p in &mut self.pool {
            p.pty.kill();
        }
        self.pool.clear();
        // shutdown chokepoint (hit on every exit path) — tear down plugins too
        self.kill_plugins();
    }

    /// discover + spawn enabled plugins once, after the window is shown. each
    /// plugin is a separate process wired to the event loop via the proxy; a
    /// plugin's stdout line arrives as UserEvent::Plugin. failures are logged and
    /// skipped so a broken plugin can never block startup or the core
    fn start_plugins(&mut self) {
        for (idx, (id, program, args)) in discover_plugins().into_iter().enumerate() {
            let proxy = self.proxy.clone();
            match plugin::Plugin::spawn(id.clone(), &program, &args, move |msg| {
                let _ = proxy.send_event(UserEvent::Plugin { id: idx, msg });
            }) {
                Ok(mut p) => {
                    // handshake: tell the plugin our api version + granted perms
                    p.send(&plugin::HostEvent::Hello {
                        api_version: plugin::API_VERSION,
                        permissions: Vec::new(),
                    });
                    self.plugins.push(p);
                    // keep ids parallel to `plugins` (only on success) so a
                    // publisher can be named on bus messages
                    self.plugin_ids.push(id);
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

    fn kill_plugins(&mut self) {
        for p in &mut self.plugins {
            p.kill();
        }
        self.plugins.clear();
        self.plugin_ids.clear();
        self.plugin_subs.clear();
    }

    /// handle one command from plugin `pidx` (Tier-1 widgets + safe verbs only;
    /// write_pty/read_output are gated by permissions, not yet granted in v1)
    fn handle_plugin_cmd(&mut self, pidx: usize, cmd: plugin::PluginCmd) {
        use plugin::PluginCmd as C;
        match cmd {
            C::Ready { name, api_version } => {
                log::info!("plugin ready: {name} (api {api_version})");
            }
            C::Notify { text } => {
                log::info!("plugin notify: {text}");
            }
            // Tier-1 widgets: upsert by (plugin, id) and rebuild the dock
            C::DeclareWidget(w) | C::UpdateWidget(w) => {
                let dw = render::DockWidget { title: w.title, lines: w.lines };
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
            C::WritePty { .. } => log::warn!("plugin write_pty denied (no permission)"),
            C::Unknown(t) => log::warn!("plugin sent unknown command: {t}"),
        }
    }

    /// push the current widget set into the renderer's dock; if the dock's
    /// presence toggled it changed content_rect, so panes must relayout
    fn rebuild_dock(&mut self) {
        let widgets: Vec<render::DockWidget> =
            self.plugin_widgets.iter().map(|(_, _, w)| w.clone()).collect();
        let toggled = self
            .renderer
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
        for tab in &mut self.tabs {
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
        self.layout_cache
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, r)| *r)
    }

    /// (row, col) under a pixel position within the focused pane
    fn cell_in_focused(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let rect = self.focused_pane_rect()?;
        let r = self.renderer.as_ref()?;
        let (col, row) = r.cell_at(rect, x, y);
        Some((row, col))
    }

    /// the web url under a pixel position in the focused pane, as
    /// (viewport row, col_start, col_end, url); only when the point is actually
    /// inside the pane, not clamped to its edge
    fn focused_url_at(&self, x: f32, y: f32) -> Option<(usize, usize, usize, String)> {
        let (rx, ry, rw, rh) = self.focused_pane_rect()?;
        if x < rx || y < ry || x >= rx + rw || y >= ry + rh {
            return None;
        }
        let (col, row) = self.renderer.as_ref()?.cell_at((rx, ry, rw, rh), x, y);
        let id = self.active_focused_id()?;
        let root = self.tabs.get(self.active_tab)?.root.as_ref()?;
        let p = find_pane(root, id)?;
        let (start, end, url) = p.term.grid.url_at(row, col)?;
        Some((row, start, end, url))
    }

    /// which pane (id) sits under a pixel position
    fn pane_at(&self, x: f32, y: f32) -> Option<usize> {
        self.layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(id, _)| *id)
    }

    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else {
            return;
        };
        let text = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, sel.pane))
            .map(|p| p.term.grid.selected_text(sel.start, sel.end))
            .unwrap_or_default();
        if !text.is_empty() {
            win::clipboard_set(&text);
        }
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
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(root) = tab.root.as_mut() {
                if let Some(p) = find_pane_mut(root, id) {
                    let mut bytes = Vec::new();
                    if p.term.bracketed_paste {
                        bytes.extend_from_slice(b"\x1b[200~");
                        bytes.extend_from_slice(normalized.as_bytes());
                        bytes.extend_from_slice(b"\x1b[201~");
                    } else {
                        bytes.extend_from_slice(normalized.as_bytes());
                    }
                    p.pty.write(&bytes);
                }
            }
        }
    }

    /// render one frame: window title + every visible pane
    fn paint(&mut self) {
        let clock = win::local_hm();
        let git = self.git.clone();
        let sessions = self.tabs.len();
        let palette_view = self.palette.as_ref().map(|p| render::PaletteView {
            query: p.query.clone(),
            items: palette_filter(&p.query)
                .into_iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            selected: p.selected,
        });
        let config = self.config;
        let settings_open = self.settings_open;
        let settings_p = self.settings_p();
        if let Some(r) = self.renderer.as_mut() {
            r.set_status(git, clock, sessions);
            r.set_palette(palette_view);
            r.set_settings(render::SettingsView {
                scrollback: config.scrollback,
                copy_on_select: config.copy_on_select,
                load_profile: config.load_profile,
                shell_name: config.shell.label(),
                close_action_name: config.close_action.label(),
                backend_name: config.backend.label(),
            });
            r.set_settings_panel(settings_open, settings_p);
        }
        let title = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, t.focused)))
            .and_then(|p| p.term.cwd.as_deref())
            .map(|c| format!("{} — termie", cwd_label(Some(c))))
            .unwrap_or_else(|| "termie".to_string());
        if self.last_title != title {
            if let Some(w) = &self.window {
                w.set_title(&title);
            }
            self.last_title = title;
        }
        let App {
            renderer,
            tabs,
            active_tab,
            layout_cache,
            focused,
            maximized,
            selection,
            link,
            ..
        } = self;
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
                                    sel: selection.filter(|s| s.pane == *id).map(|s| (s.start, s.end)),
                                    flash: p
                                        .flash
                                        .map(|t| {
                                            // hold full for 120ms, then ease to 0 by 220ms
                                            let e = t.elapsed().as_millis() as f32;
                                            (1.0 - (e - 120.0) / 100.0).clamp(0.0, 1.0)
                                        })
                                        .unwrap_or(0.0),
                                    link: if *id == tab.focused { *link } else { None },
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if let Err(e) = r.render(&views, *focused, *maximized) {
                log::error!("render error: {e:#}");
            }
        }
    }

    /// recompute every tab's pane rects and resize each pane's term + pty
    fn relayout_all(&mut self) {
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        // a settings tab has no panes; clear so stale rects don't linger
        self.layout_cache.clear();
        let content = r.content_rect();
        let (_, _, pool_cols, pool_rows) = r.pane_metrics(content);
        // keep ready pool shells sized to a full content pane; resizing a shell
        // mid-PSReadLine-startup wedges it, so only touch ready ones
        for sp in &mut self.pool {
            if sp.ready && (sp.term.grid.cols != pool_cols || sp.term.grid.rows != pool_rows) {
                sp.term.resize(pool_rows, pool_cols);
                sp.pty.resize(pool_cols as u16, pool_rows as u16);
            }
        }
        for (ti, tab) in self.tabs.iter_mut().enumerate() {
            let Some(root) = tab.root.as_mut() else {
                continue;
            };
            let mut rects = Vec::new();
            layout(root, content, &mut rects);
            for (id, rect) in &rects {
                let (_, _, cols, rows) = r.pane_metrics(*rect);
                if let Some(p) = find_pane_mut(root, *id) {
                    // skip redundant resizes — resizing pwsh mid-startup wedges PSReadLine
                    if p.term.grid.rows != rows || p.term.grid.cols != cols {
                        p.term.resize(rows, cols);
                        p.pty.resize(rows as u16, cols as u16);
                    }
                }
            }
            if ti == self.active_tab {
                self.layout_cache = rects;
            }
        }
    }

    fn sync_tabs(&mut self) {
        let labels: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                let cwd = t
                    .root
                    .as_ref()
                    .and_then(|r| find_pane(r, t.focused))
                    .and_then(|p| p.term.cwd.as_deref());
                cwd_label(cwd)
            })
            .collect();
        let active = self.active_tab;
        let cwd: Option<String> = self
            .tabs
            .get(active)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, t.focused)))
            .and_then(|p| p.term.cwd.clone());
        // only walk the filesystem for .git/HEAD when the cwd actually changed
        if cwd != self.last_git_cwd {
            self.git = git_branch(cwd.as_deref());
            self.last_git_cwd = cwd;
        }
        if let Some(r) = self.renderer.as_mut() {
            r.set_tabs(labels, active);
        }
    }

    fn active_focused_id(&self) -> Option<usize> {
        self.tabs.get(self.active_tab).map(|t| t.focused)
    }

    fn new_tab(&mut self) {
        if self.renderer.is_none() {
            return;
        }
        let (cols, rows) = self.content_pane_size();
        // grab a pre-warmed pool shell that already fits (instant); else spawn fresh
        let pane = match self
            .pool
            .iter()
            .position(|p| p.term.grid.cols == cols && p.term.grid.rows == rows)
        {
            Some(i) => Ok(self.pool.remove(i)),
            None => self.spawn_pane(cols, rows),
        };
        if let Ok(pane) = pane {
            let fid = pane.id;
            self.tabs.push(Tab {
                focused: fid,
                root: Some(Node::Leaf(pane)),
            });
            self.active_tab = self.tabs.len() - 1;
            self.relayout_all();
            self.sync_tabs();
            self.redraw();
            self.warm_pool();
        }
    }

    /// open (or focus) the settings tab
    /// open the slide-in settings panel (resets scroll to the top)
    fn open_settings(&mut self) {
        if !self.settings_open {
            self.settings_open = true;
            self.settings_anim = Some(Instant::now());
            if let Some(r) = self.renderer.as_mut() {
                r.reset_settings_scroll();
            }
            self.redraw();
        }
    }

    fn close_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.settings_anim = Some(Instant::now());
            self.redraw();
        }
    }

    fn toggle_settings(&mut self) {
        if self.settings_open {
            self.close_settings();
        } else {
            self.open_settings();
        }
    }

    /// docked fraction of the settings panel (0 = hidden, 1 = fully in)
    fn settings_p(&self) -> f32 {
        const DUR: f32 = 0.14;
        match self.settings_anim {
            None => {
                if self.settings_open {
                    1.0
                } else {
                    0.0
                }
            }
            Some(t) => {
                let e = (t.elapsed().as_secs_f32() / DUR).clamp(0.0, 1.0);
                // ease-out cubic
                let eased = 1.0_f32 - (1.0_f32 - e).powi(3);
                if self.settings_open {
                    eased
                } else {
                    1.0 - eased
                }
            }
        }
    }

    fn run_palette_action(&mut self, a: PaletteAction, event_loop: &ActiveEventLoop) {
        match a {
            PaletteAction::NewTab => self.new_tab(),
            PaletteAction::SplitV => self.split_focused(Dir::Vertical),
            PaletteAction::SplitH => self.split_focused(Dir::Horizontal),
            PaletteAction::NextTab => {
                let n = self.tabs.len();
                if n > 1 {
                    self.active_tab = (self.active_tab + 1) % n;
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
            }
            PaletteAction::PrevTab => {
                let n = self.tabs.len();
                if n > 1 {
                    self.active_tab = (self.active_tab + n - 1) % n;
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
            }
            PaletteAction::CloseTab => {
                let i = self.active_tab;
                self.close_tab(i, event_loop);
            }
            PaletteAction::Settings => self.open_settings(),
            PaletteAction::PaneMode => self.set_pane_mode(true),
            PaletteAction::Theme => {
                if let Some(r) = self.renderer.as_mut() {
                    r.cycle_theme();
                }
                self.redraw();
                self.save_config();
            }
            PaletteAction::Quit => {
                for tab in &mut self.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
                self.kill_pool();
                event_loop.exit();
            }
        }
    }

    fn close_tab(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut tab = self.tabs.remove(idx);
        if let Some(root) = tab.root.as_mut() {
            kill_all(root);
        }
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if self.active_tab > idx {
            self.active_tab -= 1;
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.relayout_all();
        self.sync_tabs();
        self.redraw();
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            self.relayout_all();
            self.sync_tabs();
            self.redraw();
        }
    }

    fn split_focused(&mut self, dir: Dir) {
        let Some(focused) = self.active_focused_id() else {
            return;
        };
        // prefer a ready pool shell (instant — relayout resizes it to the split
        // rect, safe since it's past startup); else spawn fresh at exactly the
        // post-split rect so the immediate relayout never resizes pwsh mid-startup
        let pane = if let Some(i) = self.pool.iter().position(|p| p.ready) {
            self.pool.remove(i)
        } else {
            let foc_rect = self
                .layout_cache
                .iter()
                .find(|(i, _)| *i == focused)
                .map(|(_, r)| *r);
            let (cols, rows) = match (self.renderer.as_ref(), foc_rect) {
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
            let Ok(p) = self.spawn_pane(cols, rows) else {
                return;
            };
            p
        };
        let new_id = pane.id;
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
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
        self.relayout_all();
        self.sync_tabs();
        self.redraw();
        self.warm_pool();
    }

    fn close_focused_pane(&mut self, event_loop: &ActiveEventLoop) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
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
                self.relayout_all();
                self.redraw();
            }
            None => {
                // last pane in the tab closed → close the tab
                let idx = self.active_tab;
                self.close_tab(idx, event_loop);
            }
        }
    }

    fn focus_pane_at(&mut self, x: f32, y: f32) {
        let hit = self
            .layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(id, _)| *id);
        let changed = if let (Some(id), Some(tab)) = (hit, self.tabs.get_mut(self.active_tab)) {
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
            // tab label + git track the focused pane
            self.sync_tabs();
            self.redraw();
            if let (Some(id), false) = (hit, self.plugins.is_empty()) {
                self.plugins_broadcast(&plugin::HostEvent::FocusChanged { pane: id as u64 });
            }
        }
    }

    fn button_action(&mut self, event_loop: &ActiveEventLoop, hot: Hot) {
        match hot {
            Hot::Minimize => {
                if let Some(w) = &self.window {
                    w.set_minimized(true);
                }
            }
            Hot::Maximize => {
                self.maximized = !self.maximized;
                if let Some(w) = &self.window {
                    w.set_maximized(self.maximized);
                }
            }
            Hot::Close => {
                if self.config.close_action == CloseAction::Minimize {
                    if let Some(w) = &self.window {
                        w.set_minimized(true);
                    }
                } else {
                    for tab in &mut self.tabs {
                        if let Some(root) = tab.root.as_mut() {
                            kill_all(root);
                        }
                    }
                    self.kill_pool();
                    event_loop.exit();
                }
            }
            Hot::Gear => self.toggle_settings(),
            Hot::PanelClose => self.close_settings(),
            Hot::ThemeSet(id) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_theme(id);
                }
                self.redraw();
            }
            Hot::SplitV => self.split_focused(Dir::Vertical),
            Hot::SplitH => self.split_focused(Dir::Horizontal),
            Hot::NewTab => self.new_tab(),
            Hot::Tab(i) => self.switch_tab(i),
            Hot::TabClose(i) => self.close_tab(i, event_loop),
            Hot::FontDec | Hot::FontInc => {
                let d = if hot == Hot::FontInc { 1.0 } else { -1.0 };
                if let Some(r) = self.renderer.as_mut() {
                    r.set_content_pt(r.content_pt() + d);
                }
                self.relayout_all();
                self.redraw();
            }
            Hot::FontCycle => {
                if let Some(r) = self.renderer.as_mut() {
                    r.cycle_font();
                }
                self.relayout_all();
                self.redraw();
            }
            Hot::PadDec | Hot::PadInc => {
                let d = if hot == Hot::PadInc { 2.0 } else { -2.0 };
                let changed = self
                    .renderer
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
                if let Some(r) = self.renderer.as_mut() {
                    r.nudge_opacity(d);
                }
                self.redraw();
            }
            Hot::CursorCycle => {
                if let Some(r) = self.renderer.as_mut() {
                    r.cycle_cursor();
                }
                self.redraw();
            }
            Hot::CursorBlink => {
                if let Some(r) = self.renderer.as_mut() {
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
        }
        // persist whenever a setting changed
        if is_settings_hot(hot) {
            self.save_config();
        }
    }

    /// write every setting (App-owned + renderer-owned) to the config file
    fn save_config(&self) {
        use std::fmt::Write as _;
        // never persist a partial file: renderer-owned keys would be dropped and
        // fall back to defaults on the next load
        let Some(r) = self.renderer.as_ref() else {
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
        let _ = writeln!(s, "backend={}", self.config.backend.label());
        let _ = writeln!(s, "font_size={}", r.content_pt() as i32);
        let _ = writeln!(s, "padding={}", r.pane_pad_px() as i32);
        let _ = writeln!(s, "opacity={}", r.opacity_pct());
        let _ = writeln!(s, "cursor={}", r.cursor_style_name());
        let _ = writeln!(s, "cursor_blink={}", r.cursor_blink());
        let _ = writeln!(s, "theme={}", r.theme().name());
        let _ = writeln!(s, "font={}", r.font_name());
        let _ = std::fs::write(&path, s);
    }

    fn set_pane_mode(&mut self, on: bool) {
        self.pane_mode = on;
        if let Some(r) = self.renderer.as_mut() {
            r.set_pane_mode(on);
        }
        self.redraw();
    }

    /// report a mouse event to the pane under the cursor if it has mouse mode on;
    /// returns true if forwarded (caller should skip local selection/scroll)
    fn mouse_report(&mut self, btn: u8, pressed: bool, motion: bool) -> bool {
        let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
        let Some(id) = self.pane_at(cx, cy) else {
            return false;
        };
        self.report_to_pane(id, btn, pressed, motion)
    }

    /// report a mouse event to a specific pane (coords clamped to its rect);
    /// used to keep a drag locked to the pane that received the press
    fn report_to_pane(&mut self, id: usize, btn: u8, pressed: bool, motion: bool) -> bool {
        let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
        let Some(rect) = self.layout_cache.iter().find(|(i, _)| *i == id).map(|(_, r)| *r) else {
            return false;
        };
        let Some((col, row)) = self.renderer.as_ref().map(|r| r.cell_at(rect, cx, cy)) else {
            return false;
        };
        let Some(root) = self.tabs.get_mut(self.active_tab).and_then(|t| t.root.as_mut()) else {
            return false;
        };
        let Some(p) = find_pane_mut(root, id) else {
            return false;
        };
        if let Some(bytes) = p.term.encode_mouse(btn, pressed, motion, col, row) {
            p.pty.write(&bytes);
            true
        } else {
            false
        }
    }

    /// does the pane under the cursor want motion events right now?
    fn pane_wants_motion(&self) -> bool {
        let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
        let Some(id) = self.pane_at(cx, cy) else {
            return false;
        };
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, id)))
            .map(|p| p.term.wants_motion(self.mouse_down.is_some()))
            .unwrap_or(false)
    }

    /// set the OS pointer icon, skipping the call when it hasn't changed
    fn set_pointer(&mut self, icon: CursorIcon) {
        if self.cursor_icon != icon {
            self.cursor_icon = icon;
            if let Some(w) = &self.window {
                w.set_cursor(icon);
            }
        }
    }

    /// change the content font size: d>0 bigger, d<0 smaller, d==0 reset to default
    fn nudge_font(&mut self, d: f32) {
        if let Some(r) = self.renderer.as_mut() {
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
        let Some((_, (cx0, cy0, cw, ch))) = self.layout_cache.iter().find(|(id, _)| *id == cur)
        else {
            return;
        };
        let (cx, cy) = (cx0 + cw / 2.0, cy0 + ch / 2.0);
        let mut best: Option<(usize, f32)> = None;
        for (id, (x, y, w, h)) in &self.layout_cache {
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
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.focused = id;
            }
            self.sync_tabs();
            self.redraw();
        }
    }

    /// intercept chrome shortcuts; returns true if consumed
    fn handle_shortcut(&mut self, event: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        // command palette captures every key while open
        if self.palette.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.palette = None;
                    self.redraw();
                }
                Key::Named(NamedKey::Enter) => {
                    let (q, sel) = self
                        .palette
                        .as_ref()
                        .map(|p| (p.query.clone(), p.selected))
                        .unwrap_or_default();
                    self.palette = None;
                    if let Some(&(_, a)) = palette_filter(&q).get(sel) {
                        self.run_palette_action(a, event_loop);
                    }
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let len = self
                        .palette
                        .as_ref()
                        .map(|p| palette_filter(&p.query).len())
                        .unwrap_or(0);
                    if let Some(p) = self.palette.as_mut() {
                        if len > 0 {
                            p.selected = (p.selected + 1) % len;
                        }
                    }
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    let len = self
                        .palette
                        .as_ref()
                        .map(|p| palette_filter(&p.query).len())
                        .unwrap_or(0);
                    if let Some(p) = self.palette.as_mut() {
                        if len > 0 {
                            p.selected = (p.selected + len - 1) % len;
                        }
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
                    if !self.mods.control_key() {
                        if let Some(t) = event.text.as_ref() {
                            if !t.is_empty() && !t.chars().any(|c| c.is_control()) {
                                let t = t.to_string();
                                if let Some(p) = self.palette.as_mut() {
                                    p.query.push_str(&t);
                                    p.selected = 0;
                                }
                                self.redraw();
                            }
                        }
                    }
                }
            }
            return true;
        }
        // pane control mode captures every key until exited
        if self.pane_mode {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                    self.set_pane_mode(false)
                }
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
                    "n" => self.new_tab(),
                    "q" => self.set_pane_mode(false),
                    _ => {}
                },
                _ => {}
            }
            return true;
        }

        // the settings panel captures keys while open (Esc or Ctrl+, closes it)
        if self.settings_open {
            let esc = event.logical_key == Key::Named(NamedKey::Escape);
            let ctrl_comma = self.mods.control_key()
                && matches!(&event.logical_key, Key::Character(c) if c.as_str() == ",");
            if esc || ctrl_comma {
                self.close_settings();
            }
            return true;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();
        if !ctrl {
            return false;
        }
        match &event.logical_key {
            Key::Character(c) if c.as_str() == "," => {
                self.toggle_settings();
                true
            }
            // font zoom: Ctrl+= / Ctrl++ bigger, Ctrl+- smaller, Ctrl+0 reset
            Key::Character(c) if c.as_str() == "=" || c.as_str() == "+" => {
                self.nudge_font(1.0);
                true
            }
            Key::Character(c) if c.as_str() == "-" || c.as_str() == "_" => {
                self.nudge_font(-1.0);
                true
            }
            Key::Character(c) if c.as_str() == "0" => {
                self.nudge_font(0.0);
                true
            }
            Key::Named(NamedKey::Tab) => {
                let n = self.tabs.len();
                if n > 1 {
                    self.active_tab = if shift {
                        (self.active_tab + n - 1) % n
                    } else {
                        (self.active_tab + 1) % n
                    };
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("p") => {
                self.set_pane_mode(true);
                true
            }
            // broadcast input: type once, send to every pane in the tab
            Key::Character(c) if shift && c.eq_ignore_ascii_case("b") => {
                self.broadcast = !self.broadcast;
                if let Some(r) = self.renderer.as_mut() {
                    r.set_broadcast(self.broadcast);
                }
                self.redraw();
                true
            }
            Key::Character(c) if !shift && c.eq_ignore_ascii_case("p") => {
                self.palette = Some(PaletteState {
                    query: String::new(),
                    selected: 0,
                });
                self.redraw();
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("t") => {
                self.new_tab();
                true
            }
            // ctrl+t (no shift) is a quick new-tab too
            Key::Character(c) if !shift && c.eq_ignore_ascii_case("t") => {
                self.new_tab();
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("c") => {
                self.copy_selection();
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("v") => {
                self.paste();
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("w") => {
                self.close_focused_pane(event_loop);
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("e") => {
                self.split_focused(Dir::Vertical);
                true
            }
            Key::Character(c) if shift && c.eq_ignore_ascii_case("o") => {
                self.split_focused(Dir::Horizontal);
                true
            }
            _ => false,
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.boot(event_loop) {
            log::error!("failed to start termie: {e:#}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Pty { id, bytes } => {
                let mut responses: Option<Vec<u8>> = None;
                let mut found = false;
                let mut in_sync = false;
                let mut rang = false;
                for tab in &mut self.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        if let Some(p) = find_pane_mut(root, id) {
                            p.parser.advance(&mut p.term, &bytes);
                            in_sync = p.term.sync_output;
                            if !p.term.responses.is_empty() {
                                responses = Some(std::mem::take(&mut p.term.responses));
                            }
                            if p.term.bell {
                                p.term.bell = false;
                                p.flash = Some(Instant::now());
                                rang = true;
                            }
                            found = true;
                            break;
                        }
                    }
                }
                // let plugins react to the bell (host -> plugin event direction)
                if rang && !self.plugins.is_empty() {
                    self.plugins_broadcast(&plugin::HostEvent::Bell { pane: id as u64 });
                }
                if !found {
                    // route to a warm pool shell; first output means it's started,
                    // so it's now safe to size it to a full content pane
                    let (ccols, crows) = self.content_pane_size();
                    if let Some(sp) = self.pool.iter_mut().find(|sp| sp.id == id) {
                        sp.parser.advance(&mut sp.term, &bytes);
                        sp.ready = true;
                        if sp.term.grid.cols != ccols || sp.term.grid.rows != crows {
                            sp.term.resize(crows, ccols);
                            sp.pty.resize(ccols as u16, crows as u16);
                        }
                        if !sp.term.responses.is_empty() {
                            responses = Some(std::mem::take(&mut sp.term.responses));
                        }
                    }
                }
                if let Some(r) = responses {
                    let mut wrote = false;
                    for tab in &mut self.tabs {
                        if let Some(root) = tab.root.as_mut() {
                            if let Some(p) = find_pane_mut(root, id) {
                                p.pty.write(&r);
                                wrote = true;
                                break;
                            }
                        }
                    }
                    if !wrote {
                        if let Some(sp) = self.pool.iter_mut().find(|sp| sp.id == id) {
                            sp.pty.write(&r);
                        }
                    }
                }
                // refresh tab labels when a cwd (OSC-7) likely just arrived
                if bytes.windows(3).any(|w| w == b"\x1b]7") {
                    self.sync_tabs();
                }
                if self.layout_cache.iter().any(|(pid, _)| *pid == id) {
                    if in_sync {
                        // mid synchronized-output frame: defer the paint so the
                        // screen isn't shown torn (cursor stranded mid-redraw)
                        if self.sync_redraw_pending.is_none() {
                            self.sync_redraw_pending = Some(Instant::now());
                        }
                    } else {
                        self.sync_redraw_pending = None;
                        self.redraw();
                    }
                }
            }
            UserEvent::Exited { id } => {
                // a warm pool shell that died — drop it so warm_pool respawns
                self.pool.retain(|p| p.id != id);
                // find which tab holds this pane, close that pane
                let owner = self.tabs.iter().position(|t| {
                    t.root.as_ref().map(|r| find_pane(r, id).is_some()).unwrap_or(false)
                });
                if let Some(ti) = owner {
                    let prev_active = self.active_tab;
                    self.active_tab = ti;
                    self.close_focused_pane_by_id(id, event_loop);
                    if ti != prev_active && prev_active < self.tabs.len() {
                        self.active_tab = prev_active.min(self.tabs.len().saturating_sub(1));
                        self.relayout_all();
                    }
                    self.sync_tabs();
                    self.redraw();
                }
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
                    if self.tabs.is_empty() {
                        // first shell of an async startup -> becomes tab one
                        let fid = pane.id;
                        self.tabs.push(Tab {
                            focused: fid,
                            root: Some(Node::Leaf(*pane)),
                        });
                        self.active_tab = 0;
                        self.relayout_all();
                        self.sync_tabs();
                        self.redraw();
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
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                for tab in &mut self.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
                self.kill_pool();
                event_loop.exit();
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                // a held drag can't survive losing focus: release it so the TUI
                // doesn't see a stuck button
                if !f {
                    if let Some((btn, id)) = self.mouse_down.take() {
                        self.report_to_pane(id, btn, false, false);
                    }
                }
                // report focus in/out to a pane that enabled mode 1004
                if let Some(id) = self.active_focused_id() {
                    if let Some(root) = self.tabs.get_mut(self.active_tab).and_then(|t| t.root.as_mut()) {
                        if let Some(p) = find_pane_mut(root, id) {
                            if p.term.focus_events {
                                p.pty.write(if f { b"\x1b[I" } else { b"\x1b[O" });
                            }
                        }
                    }
                }
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                // releasing ctrl removes a link underline even without moving
                if !self.mods.control_key() && self.link.is_some() {
                    self.link = None;
                    self.set_pointer(CursorIcon::Default);
                    self.redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                let (px, py) = (position.x as f32, position.y as f32);
                // mouse-tracking motion (1002 drag / 1003 any-motion)
                if self.drag_divider.is_none() && !self.settings_open && !self.mods.shift_key() {
                    if let Some((btn, id)) = self.mouse_down {
                        // a forwarded press is held: lock motion to the press-pane
                        // (even off its rect) and don't fall through to selection
                        let wants = self
                            .tabs
                            .get(self.active_tab)
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
                    if let Some(content) = self.renderer.as_ref().map(|r| r.content_rect()) {
                        if let Some(root) = self.tabs.get_mut(self.active_tab).and_then(|t| t.root.as_mut()) {
                            set_divider_ratio(root, content, &path, px, py);
                        }
                        self.relayout_all();
                        self.redraw();
                    }
                } else if self.selecting {
                    if let (Some(mut sel), Some(cell)) =
                        (self.selection, self.cell_in_focused(px, py))
                    {
                        sel.end = cell;
                        self.selection = Some(sel);
                        self.redraw();
                    }
                } else {
                    if let Some(r) = self.renderer.as_mut() {
                        let hovered = match r.hit_test(px, py) {
                            Hit::Button(c) => Some(c),
                            _ => None,
                        };
                        if r.set_hovered(hovered) {
                            self.redraw();
                        }
                    }
                    // ctrl-hover a url: underline it and show a hand (click opens)
                    let new_link = if self.mods.control_key() && !self.settings_open {
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
                        let dir = if self.settings_open || self.mods.shift_key() {
                            None
                        } else if let (Some(content), Some(root)) = (
                            self.renderer.as_ref().map(|r| r.content_rect()),
                            self.tabs.get(self.active_tab).and_then(|t| t.root.as_ref()),
                        ) {
                            divider_dir(root, content, px, py, 6.0)
                        } else {
                            None
                        };
                        match dir {
                            Some(Dir::Vertical) => CursorIcon::EwResize,
                            Some(Dir::Horizontal) => CursorIcon::NsResize,
                            None => CursorIcon::Default,
                        }
                    };
                    self.set_pointer(icon);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
                // the open settings panel grabs the wheel when hovered
                if self.settings_open {
                    let over = self.renderer.as_ref().map(|r| r.in_settings_panel(cx, cy)).unwrap_or(false);
                    if over {
                        let amt = match delta {
                            MouseScrollDelta::LineDelta(_, y) => -y * 40.0,
                            MouseScrollDelta::PixelDelta(p) => -(p.y as f32),
                        };
                        if let Some(r) = self.renderer.as_mut() {
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
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 3.0,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 20.0) as f32,
                };
                if let Some(id) = self.pane_at(cx, cy) {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        if let Some(root) = tab.root.as_mut() {
                            if let Some(p) = find_pane_mut(root, id) {
                                // alt screen has no scrollback — don't local-scroll it
                                if !p.term.using_alt {
                                    p.term.grid.scroll_view(lines.round() as isize);
                                    self.redraw();
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
                if let Some(Hit::Button(Hot::Tab(i) | Hot::TabClose(i))) =
                    self.renderer.as_ref().map(|r| r.hit_test(cx, cy))
                {
                    self.close_tab(i, event_loop);
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
                let hit = self.renderer.as_ref().map(|r| r.hit_test(cx, cy));
                // always finalize a forwarded press with a release report, even if
                // the cursor left the pane (else the TUI sees a stuck drag)
                if state == ElementState::Released {
                    if let Some((btn, id)) = self.mouse_down.take() {
                        self.report_to_pane(id, btn, false, false);
                        return;
                    }
                }
                // while the settings panel is open, a press outside it dismisses it
                // (and is consumed); presses inside fall through to its controls
                if self.settings_open && state == ElementState::Pressed {
                    let in_panel = self
                        .renderer
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
                // pane mode: drag a divider to resize, or drag a pane onto another
                // to swap them (instead of selecting text)
                if self.pane_mode && !matches!(hit, Some(Hit::Button(_)) | Some(Hit::TitleBar) | Some(Hit::Resize(_))) {
                    match state {
                        ElementState::Pressed => {
                            let found = if let (Some(content), Some(root)) = (
                                self.renderer.as_ref().map(|r| r.content_rect()),
                                self.tabs.get(self.active_tab).and_then(|t| t.root.as_ref()),
                            ) {
                                let mut path = Vec::new();
                                find_divider(root, content, cx, cy, 8.0, &mut path)
                            } else {
                                None
                            };
                            if let Some(p) = found {
                                self.drag_divider = Some(p);
                            } else if let Some(id) = self.pane_at(cx, cy) {
                                self.drag_pane = Some(id);
                                self.focus_pane_at(cx, cy);
                            }
                        }
                        ElementState::Released => {
                            if self.drag_divider.take().is_none() {
                                if let Some(src) = self.drag_pane.take() {
                                    if let Some(dst) = self.pane_at(cx, cy) {
                                        if dst != src {
                                            if let Some(root) = self.tabs.get_mut(self.active_tab).and_then(|t| t.root.as_mut()) {
                                                swap_panes(root, src, dst);
                                            }
                                            self.relayout_all();
                                            self.sync_tabs();
                                            self.redraw();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return;
                }
                // ctrl+click opens a web link under the cursor (before any TUI
                // forwarding, so it works inside mouse-reporting apps too)
                if state == ElementState::Pressed && self.mods.control_key() {
                    if let Some((_, _, _, url)) = self.focused_url_at(cx, cy) {
                        win::open_url(&url);
                        return;
                    }
                }
                // forward a press to a TUI with mouse reporting on (Shift bypasses
                // for manual selection); release is finalized at the top of the arm
                if matches!(hit, Some(Hit::Content))
                    && !self.mods.shift_key()
                    && state == ElementState::Pressed
                {
                    if let Some(id) = self.pane_at(cx, cy) {
                        if self.report_to_pane(id, 0, true, false) {
                            self.focus_pane_at(cx, cy);
                            self.mouse_down = Some((0, id));
                            return;
                        }
                    }
                }
                // drag a split divider directly to resize it (no pane mode needed)
                if matches!(hit, Some(Hit::Content)) && !self.mods.shift_key() {
                    match state {
                        ElementState::Pressed => {
                            let found = if let (Some(content), Some(root)) = (
                                self.renderer.as_ref().map(|r| r.content_rect()),
                                self.tabs.get(self.active_tab).and_then(|t| t.root.as_ref()),
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
                        Some(Hit::Button(h)) => self.pressed = Some(h),
                        Some(Hit::Content) => {
                            self.focus_pane_at(cx, cy);
                            if let (Some(pane), Some(cell)) =
                                (self.active_focused_id(), self.cell_in_focused(cx, cy))
                            {
                                self.selection = Some(Sel { pane, start: cell, end: cell });
                                self.selecting = true;
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
                                if let Some(w) = &self.window {
                                    let _ = w.drag_window();
                                }
                            }
                        }
                        Some(Hit::Resize(dir)) => {
                            if let Some(w) = &self.window {
                                let _ = w.drag_resize_window(dir);
                            }
                        }
                        _ => {}
                    },
                    ElementState::Released => {
                        self.selecting = false;
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
                        if let Some(h) = self.pressed.take() {
                            if matches!(hit, Some(Hit::Button(hh)) if hh == h) {
                                self.button_action(event_loop, h);
                            }
                        }
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.relayout_all();
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.handle_shortcut(&event, event_loop) {
                    return;
                }
                let id = match self.active_focused_id() {
                    Some(id) => id,
                    None => return,
                };
                let app_cursor = self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| t.root.as_ref())
                    .and_then(|r| find_pane(r, id))
                    .map(|p| p.term.app_cursor_keys)
                    .unwrap_or(false);
                if let Some(bytes) = input::key_to_bytes(&event, self.mods, app_cursor) {
                    self.selection = None; // typing clears the selection
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        if let Some(root) = tab.root.as_mut() {
                            if self.broadcast {
                                // send to every pane in the tab (cockpit mode)
                                each_pane_mut(root, &mut |p| p.pty.write(&bytes));
                            } else if let Some(p) = find_pane_mut(root, id) {
                                p.pty.write(&bytes);
                            }
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.paint(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // top up the warm pool once the window is up (one per tick, no spawn burst)
        if self.shown {
            self.warm_pool();
            // scan system fonts once, deferred off the startup path so the window
            // appears instantly; enables the font picker + non-Latin fallbacks
            if self.system_fonts_pending {
                self.system_fonts_pending = false;
                let want = self.persisted.font.clone();
                let scanned = if let Some(r) = self.renderer.as_mut() {
                    let s = r.ensure_system_fonts();
                    // a persisted system font couldn't resolve before the scan;
                    // apply it now that the db has it
                    if s {
                        if let Some(f) = want.as_deref() {
                            r.set_font_by_name(f);
                        }
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
                // warm the printable-ASCII glyph cache once the final content
                // font is settled, so shell output paints from a warm atlas
                // instead of shaping ~95 glyphs on the first content frames
                if let Some(r) = self.renderer.as_mut() {
                    r.prewarm_glyphs();
                }
            }
            // spawn enabled plugins once, deferred off the boot path so a window
            // with no/disabled plugins pays nothing at startup
            if !self.plugins_started {
                self.plugins_started = true;
                self.start_plugins();
            }
        }
        // first shell hasn't spawned yet after a failure: wake at the backoff
        // deadline to retry, rather than hot-looping or sleeping indefinitely
        if self.tabs.is_empty() && self.warm_fails > 0 && self.warm_fails < MAX_WARM_FAILS {
            if let Some(t) = self.warm_backoff_until {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t));
                return;
            }
        }
        // a synchronized-output frame is open: hold the paint until it closes,
        // but force one if it stalls (~100ms) so a crash mid-frame can't freeze us
        if let Some(t) = self.sync_redraw_pending {
            if t.elapsed() >= Duration::from_millis(100) {
                self.sync_redraw_pending = None;
                self.redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(t + Duration::from_millis(100)));
                return;
            }
        }
        // startup reveal fade: drive it at ~60fps until it settles
        if self.renderer.as_ref().map(|r| r.startup_fading()).unwrap_or(false) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            return;
        }
        // settings slide: redraw at ~60fps until the transition settles
        if let Some(t) = self.settings_anim {
            if t.elapsed().as_secs_f32() >= 0.14 {
                self.settings_anim = None;
            } else {
                self.redraw();
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(16),
                ));
                return;
            }
        }
        // only tick (~2 redraws/sec) when a blinking cursor is actually on screen;
        // otherwise stay event-driven so idle panes cost nothing. content changes
        // already request redraws from their own events (pty output, keys, resize)
        if self.any_flash() {
            // fade the bell flash out quickly
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(60),
            ));
        } else if self.focused && self.blinking_cursor_on_screen() {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(530),
            ));
        } else if self.focused {
            // keep the status-bar clock current without busy-redrawing: wake
            // coarsely and only repaint when the displayed minute actually rolls
            let now_hm = win::local_hm();
            let stale = self
                .renderer
                .as_ref()
                .map(|r| r.status_clock() != now_hm)
                .unwrap_or(false);
            if stale {
                self.redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_secs(5),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl App {
    fn close_focused_pane_by_id(&mut self, id: usize, event_loop: &ActiveEventLoop) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
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
            }
            None => {
                let idx = self.active_tab;
                self.close_tab(idx, event_loop);
            }
        }
    }
}

fn main() -> Result<()> {
    // stop child shells (esp. pool shells racing exit) from popping OS error dialogs
    win::suppress_child_error_dialogs();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
