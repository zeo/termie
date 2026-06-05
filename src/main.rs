// no extra console window in release; keep one in debug for logs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod a11y;
mod apc;
mod color;
mod image;
mod grid;
mod input;
mod plugin;
mod pty;
mod render;
mod session;
mod term;
mod win;
#[cfg(debug_assertions)]
mod termview;
#[cfg(debug_assertions)]
mod uiview;

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
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

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
    /// the marketplace catalog finished fetching on a worker thread
    Market(Vec<plugin::market::Entry>),
    /// the global quake hotkey fired (from the hotkey thread)
    ToggleQuake,
    /// an accesskit adapter event (screen-reader tree request / action)
    Accessibility(accesskit_winit::Event),
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
}

impl Pane {
    // resize the screen and the pty together so the two can never diverge or be
    // transposed; both take (rows, cols)
    fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(rows, cols);
        self.pty.resize(rows as u16, cols as u16);
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
}

/// a torn-off pane living in its own OS-decorated window. its pty reader still
/// routes output by pane id, so the UserEvent handlers also search here
struct Satellite {
    window: Arc<Window>,
    renderer: render::Renderer,
    pane: Pane,
}

/// an active text selection within one pane's viewport (row, col)
#[derive(Clone, Copy)]
struct Sel {
    pane: usize,
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaletteAction {
    NewTab,
    NewTabHere,
    NewShell(ShellKind),
    SplitV,
    SplitH,
    NextTab,
    PrevTab,
    CloseTab,
    Settings,
    PaneMode,
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
    ToggleZoom,
    RenameTab,
    /// prompt-jump passes through to the program when there are no OSC-133 marks
    JumpPromptPrev,
    JumpPromptNext,
    /// 0-based tab index (Ctrl+1..9)
    SelectTab(u8),
}

const PALETTE_ACTIONS: &[(&str, PaletteAction)] = &[
    ("new tab", PaletteAction::NewTab),
    ("new tab here", PaletteAction::NewTabHere),
    ("new tab: pwsh", PaletteAction::NewShell(ShellKind::Pwsh)),
    ("new tab: cmd", PaletteAction::NewShell(ShellKind::Cmd)),
    ("new tab: wsl", PaletteAction::NewShell(ShellKind::Wsl)),
    ("split vertical", PaletteAction::SplitV),
    ("split horizontal", PaletteAction::SplitH),
    ("next tab", PaletteAction::NextTab),
    ("previous tab", PaletteAction::PrevTab),
    ("close tab", PaletteAction::CloseTab),
    ("settings", PaletteAction::Settings),
    ("pane mode", PaletteAction::PaneMode),
    ("zoom pane", PaletteAction::ToggleZoom),
    ("rename tab", PaletteAction::RenameTab),
    ("quake drop-down", PaletteAction::Quake),
    ("cycle theme", PaletteAction::Theme),
    ("plugins", PaletteAction::Plugins),
    ("find", PaletteAction::OpenFind),
    ("copy", PaletteAction::Copy),
    ("paste", PaletteAction::Paste),
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
fn palette_filter(query: &str) -> Vec<(&'static str, PaletteAction)> {
    if query.trim().is_empty() {
        return PALETTE_ACTIONS.to_vec();
    }
    let mut scored: Vec<(i32, &'static str, PaletteAction)> = PALETTE_ACTIONS
        .iter()
        .filter_map(|(label, a)| fuzzy_score(query, label).map(|s| (s, *label, *a)))
        .collect();
    scored.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(y.1)));
    scored.into_iter().map(|(_, l, a)| (l, a)).collect()
}

struct PaletteState {
    query: String,
    selected: usize,
}

/// right-click pane context menu state: anchor point + hovered item
struct PaneMenu {
    x: f32,
    y: f32,
    hovered: Option<usize>,
}

/// find-in-scrollback overlay state for the focused pane; matches are
/// (global_line_index, col) into that pane's grid
struct FindState {
    query: String,
    matches: Vec<(usize, usize)>,
    current: usize,
}

/// a pending modal confirmation: the action runs on enter, esc cancels
enum ConfirmAction {
    /// send these bytes to a pane — a risky multiline paste held for confirm
    PasteBytes { pane: usize, bytes: Vec<u8> },
    /// close a tab that holds more than one pane
    CloseTab { tab: usize },
}

/// tab-rename text field overlay: which tab is being renamed + the current input
struct RenameState {
    tab: usize,
    buf: String,
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

/// derive a short tab/title label from an OSC-7 cwd uri (e.g. file:///C:/Users/dev -> dev)
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
    let Some(dir) = std::env::var_os("APPDATA") else {
        return out;
    };
    let path = std::path::Path::new(&dir).join("termie").join("colors.conf");
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && let Some(c) = parse_color(v)
        {
            out.push((k.trim().to_string(), c));
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
        (cs, chr("t"), A::NewTab),
        (cs, chr("c"), A::Copy),
        (cs, chr("v"), A::Paste),
        (cs, chr("w"), A::CloseFocusedPane),
        (cs, chr("e"), A::SplitV),
        (cs, chr("o"), A::SplitH),
    ];
    for n in 1u8..=9 {
        v.push((ctrl, chr(&n.to_string()), A::SelectTab(n - 1)));
    }
    v
}

/// resolve a keybindings.conf action label to an action — the palette entries
/// plus the keybinding-only actions (copy/paste/find/font/select-tab/etc.)
fn action_from_label(name: &str) -> Option<PaletteAction> {
    let n = name.trim();
    if let Some((_, a)) = PALETTE_ACTIONS.iter().find(|(l, _)| l.eq_ignore_ascii_case(n)) {
        return Some(*a);
    }
    let lower = n.to_ascii_lowercase();
    if let Some(d) = lower.strip_prefix("select tab ")
        && let Ok(num) = d.trim().parse::<u8>()
        && (1..=9).contains(&num)
    {
        return Some(PaletteAction::SelectTab(num - 1));
    }
    Some(match lower.as_str() {
        "copy" => PaletteAction::Copy,
        "paste" => PaletteAction::Paste,
        "find" => PaletteAction::OpenFind,
        "broadcast" | "toggle broadcast" => PaletteAction::ToggleBroadcast,
        "command palette" | "palette" => PaletteAction::OpenPalette,
        "toggle settings" => PaletteAction::ToggleSettings,
        "font increase" | "font bigger" => PaletteAction::FontInc,
        "font decrease" | "font smaller" => PaletteAction::FontDec,
        "font reset" => PaletteAction::FontReset,
        "close pane" => PaletteAction::CloseFocusedPane,
        "prompt prev" | "previous prompt" => PaletteAction::JumpPromptPrev,
        "prompt next" | "next prompt" => PaletteAction::JumpPromptNext,
        _ => return None,
    })
}

/// load keybindings: built-in defaults first, then user overrides from
/// %APPDATA%\termie\keybindings.conf. `combo=none` (or `unbind`) frees a chord
/// so it falls through to the program; any other line overrides; unknown actions
/// and unparseable combos warn instead of silently dropping
fn load_keybindings() -> Vec<(ModifiersState, Key, PaletteAction)> {
    let mut out = default_keybindings();
    let Some(dir) = std::env::var_os("APPDATA") else {
        return out;
    };
    let path = std::path::Path::new(&dir).join("termie").join("keybindings.conf");
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((combo, action)) = line.split_once('=') else {
            log::warn!("keybindings.conf: no '=' in line: {line}");
            continue;
        };
        let Some((mods, key)) = parse_combo(combo.trim()) else {
            log::warn!("keybindings.conf: unparseable combo: {}", combo.trim());
            continue;
        };
        let action = action.trim();
        // a user line replaces any default (or earlier line) for the same combo
        out.retain(|(m, k, _)| !(*m == mods && key_matches(&key, k)));
        if action.eq_ignore_ascii_case("none") || action.eq_ignore_ascii_case("unbind") {
            continue;
        }
        match action_from_label(action) {
            Some(a) => out.push((mods, key, a)),
            None => log::warn!("keybindings.conf: unknown action '{action}' (combo {})", combo.trim()),
        }
    }
    out
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
        unsafe { core::ptr::swap(pa, pb) };
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

/// build a pane (pty + child + screen) without starting its reader thread.
/// the slow part (process spawn) — safe to run off the main thread
/// parsed command line. always-new-window means each launch is its own process,
/// so this is per-process; a bare launch (no cwd/command) is what session
/// restore keys off of
#[derive(Clone, Default)]
struct CliArgs {
    cwd: Option<String>,
    command: Option<Vec<String>>,
}

impl CliArgs {
    fn is_bare(&self) -> bool {
        self.cwd.is_none() && self.command.is_none()
    }
}

/// parse `termie [--cwd DIR | -d DIR | --cwd=DIR] [-- COMMAND...]`. lenient and
/// silent: release is a windowed subsystem with no console to print help to, so
/// unknown flags are ignored rather than erroring. `--` ends option parsing and
/// the remainder is a command argv to run instead of the default shell
fn parse_args<I: Iterator<Item = String>>(args: I) -> CliArgs {
    let mut out = CliArgs::default();
    let mut it = args;
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
        }
    }
    out
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

/// feed pty output through the kitty-graphics scanner, then the vte parser. the
/// scanner pulls kitty APC image sequences out of the stream (vte has no APC
/// callback) and the remaining bytes flow to the terminal unchanged
fn pump_bytes(pane: &mut Pane, bytes: &[u8]) {
    let (pass, imgs) = pane.apc.feed(bytes);
    pane.parser.advance(&mut pane.term, &pass);
    for raw in &imgs {
        if let Some(cmd) = apc::KittyCmd::parse(raw) {
            handle_kitty(&mut pane.term, &cmd);
        }
    }
}

/// apply a kitty graphics command to a pane's terminal: store/decode images,
/// anchor placements at the cursor, delete, and queue the APC ack
fn handle_kitty(term: &mut Terminal, cmd: &apc::KittyCmd) {
    match cmd.action {
        b't' | b'T' => {
            if let Some(id) =
                term.images.transmit(cmd.id, cmd.format, cmd.width, cmd.height, cmd.more, &cmd.payload)
            {
                if cmd.action == b'T' {
                    term.grid.place_image(id);
                }
                if cmd.quiet == 0 {
                    kitty_ok(term, cmd.id);
                }
            }
        }
        b'p' => {
            if term.images.get(cmd.id).is_some() {
                term.grid.place_image(cmd.id);
                if cmd.quiet == 0 {
                    kitty_ok(term, cmd.id);
                }
            }
        }
        b'd' => {
            term.images.delete(cmd.id);
            term.grid.clear_placements();
        }
        b'q' => {
            if cmd.quiet == 0 {
                kitty_ok(term, cmd.id);
            }
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
) -> Result<Pane> {
    let pty = Pty::spawn(rows as u16, cols as u16, shell, load_profile, cwd, command, wsl_distro)?;
    let mut term = Terminal::new(rows, cols);
    term.grid.set_scrollback_limit(scrollback);
    Ok(Pane {
        id,
        term,
        parser: Parser::new(),
        pty,
        shell,
        ready: false,
        flash: None,
        apc: apc::ApcScanner::default(),
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
    /// restore the saved tab/split layout on a bare launch
    restore_on_launch: bool,
    // global quake hotkey as (win32 modifiers, virtual-key); None disables it
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
            backend: render::BackendChoice::Auto,
            restore_on_launch: true,
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
    backend: render::BackendChoice,
    restore_on_launch: bool,
    font_size: f32,
    padding: f32,
    cursor: grid::CursorShape,
    cursor_blink: bool,
    bold_as_bright: bool,
    line_height: f32,
    theme: color::ThemeId,
    font: Option<String>,
    opacity: i32,
    quake_key: Option<(u32, u32)>,
    /// the WSL distribution `new tab: wsl` launches (None = wsl.exe default)
    wsl_distro: Option<String>,
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
            restore_on_launch: true,
            font_size: CONTENT_PT,
            padding: 6.0,
            cursor: grid::CursorShape::Block,
            cursor_blink: true,
            bold_as_bright: true,
            line_height: 1.32,
            theme: color::ThemeId::Instrument,
            font: None,
            opacity: 85,
            quake_key: None,
            wsl_distro: None,
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

fn session_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(base).join("termie").join("session.json"))
}

/// %APPDATA%\termie\plugins — one subdirectory per installed plugin
fn plugins_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(base).join("termie").join("plugins"))
}

/// %APPDATA%\termie\plugins.cfg — per-plugin enabled state + granted perms.
/// one line per plugin: `id=on` or `id=off`, optionally `;perms=a,b`
fn plugins_cfg_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(base).join("termie").join("plugins.cfg"))
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
            "theme" => p.theme = color::ThemeId::from_name(v),
            "font" => {
                if !v.is_empty() {
                    p.font = Some(v.to_string());
                }
            }
            "quake_key" => p.quake_key = parse_quake_key(v),
            "wsl_distro" => {
                if !v.is_empty() {
                    p.wsl_distro = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    p
}

/// per-window state. step 1 of the multi-window refactor extracts the main
/// window's state here (App.pw); torn-off panes still live in App.satellites
/// until step 2 graduates them to first-class PaneWindows
struct PaneWindow {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tabs: Vec<Tab>,
    active_tab: usize,
    layout_cache: Vec<(usize, Rect)>,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    /// this process's parsed command line (always-new-window: one per process)
    cli: CliArgs,
    /// the main window's state (window/renderer/tabs/active_tab/layout)
    pw: PaneWindow,
    /// torn-off panes, each in its own window (keyed by window id at routing)
    satellites: Vec<Satellite>,
    next_id: usize,
    mods: ModifiersState,
    focused: bool,
    maximized: bool,
    pane_mode: bool,
    /// open right-click pane context menu (None = closed)
    pane_menu: Option<PaneMenu>,
    shown: bool,
    pool: Vec<Pane>,
    selection: Option<Sel>,
    selecting: bool,
    last_click: Option<(Instant, f64, f64)>,
    // consecutive click count in a pane's content for word/line select cycling
    click_seq: u32,
    git: Option<String>,
    palette: Option<PaletteState>,
    find: Option<FindState>,
    /// a modal confirm prompt (e.g. a risky paste), when open
    confirm: Option<ConfirmState>,
    /// the tab-rename text field, when open
    rename: Option<RenameState>,
    /// accesskit adapter for the main window (screen-reader tree); None until boot
    a11y: Option<accesskit_winit::Adapter>,
    /// true while the user is composing via the IME; raw keystrokes are then
    /// ignored and the OS candidate window is parked at the cursor
    ime_composing: bool,
    /// the plugins marketplace overlay, when open
    market: Option<MarketState>,
    cursor: PhysicalPosition<f64>,
    pressed: Option<Hot>,
    last_title: String,
    config: Config,
    /// user keybindings (combo -> palette action) loaded from disk; checked
    /// before the built-in shortcuts, empty when there is no config file
    keybindings: Vec<(ModifiersState, Key, PaletteAction)>,
    settings_open: bool,
    settings_anim: Option<Instant>,
    /// set when the focused pane changes, so its accent border eases in instead
    /// of snapping; cleared once the ease settles
    focus_anim: Option<Instant>,
    /// debug-only: TERMIE_BENCH=N auto-opens N tabs after startup to measure
    /// warm-pool tab-open latency via the TERMIE_TIMING log
    #[cfg(debug_assertions)]
    bench_left: u32,
    #[cfg(debug_assertions)]
    bench_next: Option<Instant>,
    /// pool shells currently spawning on worker threads (not yet in `pool`)
    pending_warm: usize,
    /// set once the app is exiting so no new shells are spawned during teardown
    shutting_down: bool,
    /// pane-mode drag state: a divider being resized (path) or a pane being moved
    drag_divider: Option<Vec<usize>>,
    drag_pane: Option<usize>,
    /// quake drop-down currently summoned (always-on-top at screen top)
    quake_shown: bool,
    /// the global quake hotkey thread has been spawned (once per process)
    quake_hotkey_spawned: bool,
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
    /// the tab/split layout changed since the last session write
    session_dirty: bool,
    /// debounce deadline for the session write; re-armed on every layout change
    /// so a burst (e.g. a divider drag) collapses to one write after it settles
    session_flush_at: Option<Instant>,
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
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let p = load_persisted();
        App {
            proxy,
            cli: parse_args(std::env::args().skip(1)),
            pw: PaneWindow {
                window: None,
                renderer: None,
                tabs: Vec::new(),
                active_tab: 0,
                layout_cache: Vec::new(),
            },
            satellites: Vec::new(),
            next_id: 0,
            mods: ModifiersState::empty(),
            keybindings: load_keybindings(),
            focused: true,
            maximized: false,
            pane_mode: false,
            pane_menu: None,
            shown: false,
            pool: Vec::new(),
            selection: None,
            selecting: false,
            last_click: None,
            click_seq: 0,
            git: None,
            palette: None,
            find: None,
            confirm: None,
            rename: None,
            a11y: None,
            ime_composing: false,
            market: None,
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
                restore_on_launch: p.restore_on_launch,
                quake_key: p.quake_key,
            },
            persisted: p,
            last_git_cwd: None,
            broadcast: false,
            mouse_down: None,
            cursor_icon: CursorIcon::Default,
            link: None,
            system_fonts_pending: true,
            ascii_warmed: false,
            warm_fails: 0,
            warm_backoff_until: None,
            sync_redraw_pending: None,
            resize_settle: None,
            pty_dirty: false,
            session_dirty: false,
            session_flush_at: None,
            plugins: Vec::new(),
            plugins_started: false,
            plugin_ids: Vec::new(),
            plugin_granted: Vec::new(),
            plugin_widgets: Vec::new(),
            plugin_subs: Vec::new(),
            settings_open: false,
            settings_anim: None,
            focus_anim: None,
            #[cfg(debug_assertions)]
            bench_left: std::env::var("TERMIE_BENCH").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            #[cfg(debug_assertions)]
            bench_next: None,
            pending_warm: 0,
            shutting_down: false,
            drag_divider: None,
            drag_pane: None,
            quake_shown: false,
            quake_hotkey_spawned: false,
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
        timing("window created");

        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw() {
                win::apply_window_effects(h.hwnd.get());
            }

        let renderer = Renderer::new(window.clone(), CONTENT_PT, CHROME_PT, self.config.backend)?;
        timing("renderer ready (gpu init)");
        window.set_ime_allowed(true);
        self.a11y = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));
        self.pw.window = Some(window.clone());
        self.pw.renderer = Some(renderer);

        // apply persisted renderer-owned settings before sizing the first pane
        {
            let p = &self.persisted;
            if let Some(r) = self.pw.renderer.as_mut() {
                r.set_theme(p.theme);
                r.set_color_overrides(load_color_overrides());
                r.set_cursor_style(p.cursor);
                r.set_cursor_blink(p.cursor_blink);
                r.set_bold_as_bright(p.bold_as_bright);
                r.set_line_height(p.line_height);
                r.set_pane_pad_px(p.padding);
                r.set_opacity_pct(p.opacity);
                if let Some(f) = p.font.as_deref() {
                    r.set_font_by_name(f);
                }
                r.set_content_pt(p.font_size);
            }
        }

        self.pw.active_tab = 0;
        // register the global quake hotkey once (opt-in via the quake_key setting)
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
        // an explicit cwd/command (cli or a context-menu verb): spawn the first
        // tab here at that location instead of adopting a home-dir pool shell.
        // a bare launch leaves the fast async warm-pool path below unchanged
        // surface installed WSL distros so the user knows valid wsl_distro values
        let distros = win::wsl_distros();
        if !distros.is_empty() {
            log::info!("wsl distros available: {}", distros.join(", "));
        }
        if !self.cli.is_bare() {
            let (cols, rows) = self.content_pane_size();
            let cwd = self.cli.cwd.clone();
            let command = self.cli.command.clone();
            match self.spawn_pane(cols, rows, cwd, None, command.as_deref()) {
                Ok(pane) => self.install_first_tab(pane),
                Err(e) => log::error!("failed to spawn the requested command: {e}"),
            }
        } else if self.config.restore_on_launch
            && let Some(sf) = session_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| session::SessionFile::parse(&t))
        {
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
        )?;
        self.start_reader(&mut pane);
        Ok(pane)
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

    /// is there a visible, blinking cursor on the focused pane that needs the
    /// periodic blink tick? (false when blink is off, cursor hidden, or scrolled)
    fn blinking_cursor_on_screen(&self) -> bool {
        let Some(r) = self.pw.renderer.as_ref() else {
            return false;
        };
        if !r.cursor_blink() {
            return false;
        }
        self.pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|root| find_pane(root, t.focused)))
            .map(|p| p.term.grid.cursor.visible && p.term.grid.view_offset == 0)
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
        let (shell, profile, sb) = (self.config.shell, self.config.load_profile, self.config.scrollback);
        while self.pool.len() + self.pending_warm < POOL_TARGET {
            let id = self.next_id;
            self.next_id += 1;
            let proxy = self.proxy.clone();
            let wsl = self.persisted.wsl_distro.clone();
            self.pending_warm += 1;
            std::thread::spawn(move || {
                let pane = build_pane(id, cols, rows, shell, profile, sb, None, None, wsl.as_deref()).ok().map(Box::new);
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
        // only launch enabled plugins; disabled ones still appear in the
        // marketplace UI but never spawn a process
        for d in discover_plugins().into_iter().filter(|d| d.enabled) {
            let id = d.manifest.id.clone();
            // the index this plugin will occupy once pushed
            let idx = self.plugins.len();
            let proxy = self.proxy.clone();
            match plugin::Plugin::spawn(id.clone(), &d.program, &d.manifest.args, move |msg| {
                let _ = proxy.send_event(UserEvent::Plugin { id: idx, msg });
            }) {
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

    fn open_find(&mut self) {
        self.find = Some(FindState {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
        });
        self.redraw();
    }

    /// re-run the search for the current query against the focused pane and jump
    /// to the first match
    fn find_recompute(&mut self) {
        let query = match &self.find {
            Some(f) => f.query.clone(),
            None => return,
        };
        let matches = self
            .focused_grid()
            .map(|g| g.search(&query))
            .unwrap_or_default();
        if let Some(f) = self.find.as_mut() {
            f.matches = matches;
            f.current = 0;
        }
        self.find_scroll_to_current();
        self.redraw();
    }

    fn find_scroll_to_current(&mut self) {
        let target = self
            .find
            .as_ref()
            .and_then(|f| f.matches.get(f.current).copied());
        if let Some((g, _)) = target
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
        let qlen = f.query.chars().count();
        let mut vps = Vec::new();
        if qlen > 0
            && let Some(g) = self.focused_grid() {
                for (i, &(gl, col)) in f.matches.iter().enumerate() {
                    if let Some(vr) = g.global_to_viewport(gl) {
                        vps.push((vr, col, qlen, i == f.current));
                    }
                }
            }
        Some(render::FindView {
            query: f.query.clone(),
            count: f.matches.len(),
            current: f.current,
            matches: vps,
        })
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
        let bracketed = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, id))
            .map(|p| p.term.bracketed_paste)
            .unwrap_or(false);
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(normalized.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(normalized.as_bytes());
        }
        // without bracketed paste a multiline paste runs each line as its own
        // command the moment it lands; hold it behind a confirm so a stray paste
        // can't fire a string of commands. bracketed-paste programs (modern
        // shells, TUIs) buffer the whole paste safely, so they go straight
        let multiline = normalized.trim_end_matches('\r').contains('\r');
        if !bracketed && multiline {
            let lines = normalized.split('\r').filter(|l| !l.is_empty()).count();
            self.confirm = Some(ConfirmState {
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
            p.pty.write(bytes);
        }
    }

    /// run a confirmed modal action
    fn run_confirm(&mut self, action: ConfirmAction, event_loop: &ActiveEventLoop) {
        match action {
            ConfirmAction::PasteBytes { pane, bytes } => self.send_paste_bytes(pane, &bytes),
            ConfirmAction::CloseTab { tab } => self.do_close_tab(tab, event_loop),
        }
    }

    /// render one frame: window title + every visible pane
    fn paint(&mut self) {
        let clock = win::local_hm();
        let focus_ease = self.focus_ease();
        let git = self.git.clone();
        let sessions = self.pw.tabs.len();
        let palette_view = self.palette.as_ref().map(|p| render::PaletteView {
            query: p.query.clone(),
            items: palette_filter(&p.query)
                .into_iter()
                .map(|(l, _)| l.to_string())
                .collect(),
            selected: p.selected,
        });
        let find_view = self.build_find_view();
        let market_view = self.market.as_ref().map(|m| render::MarketView {
            rows: m
                .rows
                .iter()
                .map(|r| {
                    let tag = if !r.installed {
                        "install".to_string()
                    } else if r.enabled {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    };
                    let sub = if r.permissions.is_empty() {
                        format!("v{}", r.version)
                    } else {
                        format!("v{} · perms: {}", r.version, r.permissions.join(", "))
                    };
                    render::MarketRowView {
                        label: r.name.clone(),
                        tag,
                        sub,
                    }
                })
                .collect(),
            selected: m.selected,
            status: m.status.clone(),
        });
        let config = self.config;
        let settings_open = self.settings_open;
        let settings_p = self.settings_p();
        let pane_menu_view = self.pane_menu.as_ref().map(|m| render::PaneMenuView {
            x: m.x,
            y: m.y,
            hovered: m.hovered,
        });
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_status(git, clock, sessions);
            r.set_palette(palette_view);
            r.set_pane_menu(pane_menu_view);
            r.set_find(find_view);
            r.set_market(market_view);
            r.set_confirm(self.confirm.as_ref().map(|c| render::ConfirmView {
                prompt: c.prompt.clone(),
                hint: c.hint.clone(),
            }));
            r.set_rename(self.rename.as_ref().map(|rs| render::RenameView { buf: rs.buf.clone() }));
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
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, t.focused)))
            .and_then(|p| p.term.cwd.as_deref())
            .map(|c| format!("{} — termie", cwd_label(Some(c))))
            .unwrap_or_else(|| "termie".to_string());
        if self.last_title != title {
            if let Some(w) = &self.pw.window {
                w.set_title(&title);
            }
            self.last_title = title;
        }
        let App {
            pw,
            focused,
            maximized,
            selection,
            link,
            ..
        } = self;
        let PaneWindow {
            renderer,
            tabs,
            active_tab,
            layout_cache,
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
            if let Err(e) = r.render(&views, *focused, *maximized, focus_ease, false) {
                log::error!("render error: {e:#}");
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
        // mid-PSReadLine-startup wedges it, so only touch ready ones
        for sp in &mut self.pool {
            if sp.ready && (sp.term.grid.cols != pool_cols || sp.term.grid.rows != pool_rows) {
                sp.resize(pool_rows, pool_cols);
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
        let labels: Vec<String> = self
            .pw.tabs
            .iter()
            .map(|t| {
                // a user-given title wins; otherwise label by the focused cwd
                if let Some(title) = t.title.as_deref().filter(|s| !s.is_empty()) {
                    return title.to_string();
                }
                let cwd = t
                    .root
                    .as_ref()
                    .and_then(|r| find_pane(r, t.focused))
                    .and_then(|p| p.term.cwd.as_deref());
                cwd_label(cwd)
            })
            .collect();
        let active = self.pw.active_tab;
        let cwd: Option<String> = self
            .pw.tabs
            .get(active)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, t.focused)))
            .and_then(|p| p.term.cwd.clone());
        // only walk the filesystem for .git/HEAD when the cwd actually changed
        if cwd != self.last_git_cwd {
            self.git = git_branch(cwd.as_deref());
            self.last_git_cwd = cwd;
        }
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_tabs(labels, active);
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
        if let Some(mut adapter) = self.a11y.take() {
            adapter.update_if_active(|| self.build_a11y_update());
            self.a11y = Some(adapter);
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
            if self.broadcast {
                each_pane_mut(root, &mut |p| p.pty.write(bytes));
            } else if let Some(p) = find_pane_mut(root, id) {
                p.pty.write(bytes);
            }
        }
    }

    /// park the OS IME candidate window at the focused pane's cursor cell
    fn apply_ime_area(&mut self) {
        let Some(id) = self.active_focused_id() else {
            return;
        };
        let rect = self.pw.layout_cache.iter().find(|(pid, _)| *pid == id).map(|(_, r)| *r);
        let cursor = self
            .pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref())
            .and_then(|r| find_pane(r, id))
            .map(|p| (p.term.grid.cursor.row, p.term.grid.cursor.col));
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

    fn new_tab(&mut self) {
        self.new_tab_cwd(None, None);
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
            let fid = pane.id;
            self.pw.tabs.push(Tab {
                focused: fid,
                root: Some(Node::Leaf(pane)),
                zoom: None,
                title: None,
            });
            self.pw.active_tab = self.pw.tabs.len() - 1;
            self.relayout_all();
            self.sync_tabs();
            self.redraw();
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
        if !self.settings_open {
            self.settings_open = true;
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
            PaletteAction::NewTab => self.new_tab(),
            PaletteAction::NewTabHere => {
                let cwd = self.focused_cwd();
                self.new_tab_cwd(cwd, None);
            }
            PaletteAction::NewShell(s) => {
                let cwd = self.focused_cwd();
                self.new_tab_cwd(cwd, Some(s));
            }
            PaletteAction::SplitV => self.split_focused(Dir::Vertical),
            PaletteAction::SplitH => self.split_focused(Dir::Horizontal),
            PaletteAction::NextTab => {
                let n = self.pw.tabs.len();
                if n > 1 {
                    self.pw.active_tab = (self.pw.active_tab + 1) % n;
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
            }
            PaletteAction::PrevTab => {
                let n = self.pw.tabs.len();
                if n > 1 {
                    self.pw.active_tab = (self.pw.active_tab + n - 1) % n;
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
            }
            PaletteAction::CloseTab => {
                let i = self.pw.active_tab;
                self.close_tab(i, event_loop);
            }
            PaletteAction::Settings => self.open_settings(),
            PaletteAction::Plugins => self.open_market(),
            PaletteAction::PaneMode => self.set_pane_mode(true),
            PaletteAction::Quake => self.toggle_quake(),
            PaletteAction::Theme => {
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.cycle_theme();
                }
                self.redraw();
                self.save_config();
            }
            PaletteAction::Quit => {
                for tab in &mut self.pw.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
                self.flush_session_now();
                self.kill_pool();
                event_loop.exit();
            }
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
                self.palette = Some(PaletteState { query: String::new(), selected: 0 });
                self.redraw();
            }
            PaletteAction::Copy => self.copy_selection(),
            PaletteAction::Paste => self.paste(),
            PaletteAction::CloseFocusedPane => self.close_focused_pane(event_loop),
            PaletteAction::ToggleZoom => self.toggle_zoom(),
            PaletteAction::RenameTab => {
                if let Some(tab) = self.pw.tabs.get(self.pw.active_tab) {
                    let buf = tab.title.clone().unwrap_or_default();
                    self.rename = Some(RenameState { tab: self.pw.active_tab, buf });
                    self.redraw();
                }
            }
            PaletteAction::SelectTab(n) => {
                let n = n as usize;
                if n < self.pw.tabs.len() && n != self.pw.active_tab {
                    self.pw.active_tab = n;
                    self.relayout_all();
                    self.sync_tabs();
                    self.redraw();
                }
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
            status: "fetching catalog… (enter: toggle/install · r: remove · esc: close)".to_string(),
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

    /// act on the selected marketplace row (Enter): toggle enable for an
    /// installed plugin, or install an available one
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
                description: String::new(),
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
                            description: String::new(),
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
        let panes = self
            .pw.tabs
            .get(idx)
            .and_then(|t| t.root.as_ref())
            .map(pane_count)
            .unwrap_or(0);
        if panes > 1 {
            self.confirm = Some(ConfirmState {
                prompt: format!("close this tab? it has {panes} panes"),
                hint: "enter: close \u{b7} esc: cancel".to_string(),
                action: ConfirmAction::CloseTab { tab: idx },
            });
            self.redraw();
        } else {
            self.do_close_tab(idx, event_loop);
        }
    }

    fn do_close_tab(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.pw.tabs.len() {
            return;
        }
        let mut tab = self.pw.tabs.remove(idx);
        if let Some(root) = tab.root.as_mut() {
            kill_all(root);
        }
        if self.pw.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if self.pw.active_tab > idx {
            self.pw.active_tab -= 1;
        }
        self.pw.active_tab = self.pw.active_tab.min(self.pw.tabs.len() - 1);
        self.relayout_all();
        self.sync_tabs();
        self.redraw();
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx < self.pw.tabs.len() {
            self.pw.active_tab = idx;
            self.relayout_all();
            self.sync_tabs();
            self.redraw();
        }
    }

    fn split_focused(&mut self, dir: Dir) {
        let Some(focused) = self.active_focused_id() else {
            return;
        };
        let cwd = self.focused_cwd();
        // a known cwd means spawn fresh there (pool shells live in home); else
        // prefer a ready pool shell (instant — relayout resizes it to the split
        // rect, safe since it's past startup), spawning fresh at the post-split
        // rect only as a fallback so pwsh is never resized mid-startup
        let pane = if cwd.is_none()
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
            let Ok(p) = self.spawn_pane(cols, rows, cwd, None, None) else {
                return;
            };
            p
        };
        let new_id = pane.id;
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
        self.redraw();
        self.warm_pool();
    }

    fn close_focused_pane(&mut self, event_loop: &ActiveEventLoop) {
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
                self.redraw();
            }
            None => {
                // last pane in the tab closed → close the tab
                let idx = self.pw.active_tab;
                self.close_tab(idx, event_loop);
            }
        }
    }

    fn focus_pane_at(&mut self, x: f32, y: f32) {
        let hit = self
            .pw.layout_cache
            .iter()
            .find(|(_, (rx, ry, rw, rh))| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(id, _)| *id);
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
            self.redraw();
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
                self.maximized = !self.maximized;
                if let Some(w) = &self.pw.window {
                    w.set_maximized(self.maximized);
                }
            }
            Hot::Close => {
                if self.config.close_action == CloseAction::Minimize {
                    if let Some(w) = &self.pw.window {
                        w.set_minimized(true);
                    }
                } else {
                    for tab in &mut self.pw.tabs {
                        if let Some(root) = tab.root.as_mut() {
                            kill_all(root);
                        }
                    }
                    self.flush_session_now();
                    self.kill_pool();
                    event_loop.exit();
                }
            }
            Hot::Gear => self.toggle_settings(),
            Hot::PanelClose => self.close_settings(),
            Hot::ThemeSet(id) => {
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_theme(id);
                }
                self.redraw();
            }
            Hot::SplitV => self.split_focused(Dir::Vertical),
            Hot::SplitH => self.split_focused(Dir::Horizontal),
            Hot::PaneMode => self.set_pane_mode(!self.pane_mode),
            Hot::NewTab => self.new_tab(),
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
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.cycle_font();
                }
                self.relayout_all();
                self.redraw();
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
    fn save_config(&self) {
        use std::fmt::Write as _;
        // never persist a partial file: renderer-owned keys would be dropped and
        // fall back to defaults on the next load
        let Some(r) = self.pw.renderer.as_ref() else {
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
        let _ = writeln!(s, "restore_on_launch={}", self.config.restore_on_launch);
        let _ = writeln!(s, "font_size={}", r.content_pt() as i32);
        let _ = writeln!(s, "padding={}", r.pane_pad_px() as i32);
        let _ = writeln!(s, "opacity={}", r.opacity_pct());
        let _ = writeln!(s, "cursor={}", r.cursor_style_name());
        let _ = writeln!(s, "cursor_blink={}", r.cursor_blink());
        let _ = writeln!(s, "bold_as_bright={}", r.bold_as_bright());
        let _ = writeln!(s, "line_height={}", r.line_height());
        let _ = writeln!(s, "theme={}", r.theme().name());
        let _ = writeln!(s, "font={}", r.font_name());
        if let Some(d) = &self.persisted.wsl_distro {
            let _ = writeln!(s, "wsl_distro={d}");
        }
        let _ = std::fs::write(&path, s);
    }

    /// build a snapshot of the current window's tabs + split tree for persistence
    fn session_snapshot(&self) -> session::SessionFile {
        let mut tabs = Vec::new();
        for tab in &self.pw.tabs {
            let Some(root) = tab.root.as_ref() else {
                continue;
            };
            let mut leaf_ids = Vec::new();
            let root = node_to_snap(root, &mut leaf_ids);
            let focused_leaf = leaf_ids.iter().position(|&id| id == tab.focused).unwrap_or(0);
            tabs.push(session::TabSnap { focused_leaf, root, title: tab.title.clone() });
        }
        session::SessionFile { active_tab: self.pw.active_tab, tabs }
    }

    /// mark the layout changed and (re)arm the debounced session write so a burst
    /// of mutations collapses to one write ~750ms after the last change
    fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
        self.session_flush_at = Some(Instant::now() + Duration::from_millis(750));
    }

    /// write the session atomically (temp + rename) so a reader never sees a
    /// half-written file; never clobber a good session with an empty one
    fn write_session(&self) {
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
            self.pw.tabs.push(Tab { focused, root: Some(root), zoom: None, title: tab.title.clone() });
        }
        if self.pw.tabs.is_empty() {
            return;
        }
        self.pw.active_tab = sf.active_tab.min(self.pw.tabs.len() - 1);
        self.relayout_all();
        self.sync_tabs();
    }

    fn set_pane_mode(&mut self, on: bool) {
        self.pane_mode = on;
        if let Some(r) = self.pw.renderer.as_mut() {
            r.set_pane_mode(on);
        }
        self.redraw();
    }

    /// run a pane context-menu item (index into render::PANE_MENU_ITEMS:
    /// 0 split vertical, 1 split horizontal, 2 pop out, 3 close pane, 4 paste)
    fn pane_menu_action(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        match idx {
            0 => self.split_focused(Dir::Vertical),
            1 => self.split_focused(Dir::Horizontal),
            2 => self.pop_out_focused(event_loop),
            3 => self.close_focused_pane(event_loop),
            4 => self.paste(),
            _ => {}
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
        let attrs = Window::default_attributes()
            .with_title("termie — pane")
            .with_inner_size(LogicalSize::new(760.0, 480.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(_) => {
                self.dock_loose_pane(pane);
                return;
            }
        };
        let renderer = match render::Renderer::new(window.clone(), CONTENT_PT, CHROME_PT, self.config.backend) {
            Ok(mut r) => {
                r.set_theme(self.persisted.theme);
                r
            }
            Err(_) => {
                self.dock_loose_pane(pane);
                return;
            }
        };
        self.satellites.push(Satellite { window, renderer, pane });
        self.relayout_all();
        self.sync_tabs();
        let idx = self.satellites.len() - 1;
        self.paint_satellite(idx);
        self.redraw();
    }

    /// re-attach a loose pane as a new tab (used if a satellite window won't open)
    fn dock_loose_pane(&mut self, pane: Pane) {
        let fid = pane.id;
        self.pw.tabs.push(Tab { focused: fid, root: Some(Node::Leaf(pane)), zoom: None, title: None });
        self.pw.active_tab = self.pw.tabs.len() - 1;
        self.relayout_all();
        self.sync_tabs();
        self.redraw();
    }

    /// render one satellite window: its single pane filling the client area
    fn paint_satellite(&mut self, idx: usize) {
        let Some(sat) = self.satellites.get_mut(idx) else {
            return;
        };
        let size = sat.window.inner_size();
        sat.renderer.resize(size.width, size.height);
        let pad = 4.0f32;
        let rect = (pad, pad, (size.width as f32 - pad * 2.0).max(1.0), (size.height as f32 - pad * 2.0).max(1.0));
        let (_, _, cols, rows) = sat.renderer.pane_metrics(rect);
        if sat.pane.term.grid.cols != cols || sat.pane.term.grid.rows != rows {
            sat.pane.resize(rows, cols);
        }
        let pv = render::PaneView {
            term: &sat.pane.term,
            rect,
            focused: true,
            sel: None,
            flash: 0.0,
            link: None,
        };
        let _ = sat.renderer.render(&[pv], true, false, 1.0, true);
    }

    /// handle a window event addressed to satellite `idx`
    fn satellite_event(&mut self, idx: usize, event: &WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if idx < self.satellites.len() {
                    let mut sat = self.satellites.remove(idx);
                    sat.pane.pty.kill();
                }
            }
            WindowEvent::Resized(_) | WindowEvent::RedrawRequested => self.paint_satellite(idx),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(sat) = self.satellites.get_mut(idx) {
                    sat.renderer.set_scale(*scale_factor as f32);
                }
                self.paint_satellite(idx);
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event: ke, .. } => {
                if ke.state == ElementState::Pressed
                    && let Some(sat) = self.satellites.get_mut(idx)
                {
                    let app_cursor = sat.pane.term.app_cursor_keys;
                    let kbd = sat.pane.term.kbd_flags();
                    if let Some(bytes) = input::key_to_bytes(
                        &ke.logical_key,
                        ke.text.as_deref(),
                        ke.state,
                        ke.repeat,
                        self.mods,
                        app_cursor,
                        kbd,
                    ) {
                        sat.pane.pty.write(&bytes);
                    }
                }
            }
            _ => {}
        }
    }

    /// toggle the quake drop-down: summon the window to the top of the active
    /// monitor (full width, ~45% height, always-on-top, focused), or hide it.
    /// only ever reached via the global hotkey or the palette action
    fn toggle_quake(&mut self) {
        let Some(win) = self.pw.window.clone() else {
            return;
        };
        if self.quake_shown {
            win.set_visible(false);
            win.set_window_level(WindowLevel::Normal);
            self.quake_shown = false;
            return;
        }
        let mon = win
            .current_monitor()
            .or_else(|| win.primary_monitor())
            .or_else(|| win.available_monitors().next());
        if let Some(mon) = mon {
            let pos = mon.position();
            let size = mon.size();
            let h = ((size.height as f64 * 0.45).round() as u32).max(120);
            win.set_outer_position(PhysicalPosition::new(pos.x, pos.y));
            let _ = win.request_inner_size(PhysicalSize::new(size.width, h));
        }
        win.set_window_level(WindowLevel::AlwaysOnTop);
        win.set_visible(true);
        win.focus_window();
        self.quake_shown = true;
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
        let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut()) else {
            return false;
        };
        let Some(p) = find_pane_mut(root, id) else {
            return false;
        };
        if let Some(bytes) = p.term.encode_mouse(btn, pressed, motion, col, row, mmods) {
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
        self.pw.tabs
            .get(self.pw.active_tab)
            .and_then(|t| t.root.as_ref().and_then(|r| find_pane(r, id)))
            .map(|p| p.term.wants_motion(self.mouse_down.is_some()))
            .unwrap_or(false)
    }

    /// set the OS pointer icon, skipping the call when it hasn't changed
    fn set_pointer(&mut self, icon: CursorIcon) {
        if self.cursor_icon != icon {
            self.cursor_icon = icon;
            if let Some(w) = &self.pw.window {
                w.set_cursor(icon);
            }
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
            if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab) {
                tab.focused = id;
            }
            // ease the accent border in on the newly focused pane
            self.focus_anim = Some(Instant::now());
            self.sync_tabs();
            self.redraw();
        }
    }

    /// intercept chrome shortcuts; returns true if consumed
    fn handle_shortcut(&mut self, event: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        // Esc closes an open pane context menu before anything else sees it
        if self.pane_menu.is_some() && event.logical_key == Key::Named(NamedKey::Escape) {
            self.pane_menu = None;
            self.redraw();
            return true;
        }
        // a modal confirm prompt captures every key while open: enter runs the
        // held action, esc cancels, anything else is swallowed (no accidental
        // dismissal and no leakage to the pane underneath)
        if self.confirm.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Enter) => {
                    if let Some(c) = self.confirm.take() {
                        self.run_confirm(c.action, event_loop);
                    }
                }
                Key::Named(NamedKey::Escape) => self.confirm = None,
                _ => {}
            }
            self.redraw();
            return true;
        }
        // tab rename text field: enter commits (an empty name clears back to the
        // cwd label), esc cancels, the rest edits the buffer
        if self.rename.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Enter) => {
                    if let Some(rs) = self.rename.take() {
                        let name = rs.buf.trim().to_string();
                        if let Some(tab) = self.pw.tabs.get_mut(rs.tab) {
                            tab.title = (!name.is_empty()).then_some(name);
                        }
                        self.sync_tabs();
                    }
                }
                Key::Named(NamedKey::Escape) => self.rename = None,
                Key::Named(NamedKey::Backspace) => {
                    if let Some(rs) = self.rename.as_mut() {
                        rs.buf.pop();
                    }
                }
                _ => {
                    if !self.mods.control_key()
                        && let Some(t) = event.text.as_ref()
                        && !t.is_empty()
                        && !t.chars().any(|c| c.is_control())
                    {
                        let t = t.to_string();
                        if let Some(rs) = self.rename.as_mut() {
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
            && !self.settings_open
            && !self.pane_mode
        {
            let mods = self.mods;
            let act = self
                .keybindings
                .iter()
                .find(|(m, k, _)| *m == mods && key_matches(&event.logical_key, k))
                .map(|(_, _, a)| *a);
            if let Some(a) = act {
                // run_action returns false only for prompt-jump with no marks, so
                // that key falls through to the program unchanged
                return self.run_action(a, event_loop);
            }
        }
        // the plugins marketplace overlay captures keys while open
        if self.market.is_some() && self.market_input(&event.logical_key) {
            return true;
        }
        // find-in-scrollback overlay captures every key while open
        if self.find.is_some() {
            match &event.logical_key {
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
                _ => {
                    if !self.mods.control_key()
                        && let Some(t) = event.text.as_ref()
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
                        self.run_action(a, event_loop);
                    }
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let len = self
                        .palette
                        .as_ref()
                        .map(|p| palette_filter(&p.query).len())
                        .unwrap_or(0);
                    if let Some(p) = self.palette.as_mut()
                        && len > 0 {
                            p.selected = (p.selected + 1) % len;
                        }
                    self.redraw();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    let len = self
                        .palette
                        .as_ref()
                        .map(|p| palette_filter(&p.query).len())
                        .unwrap_or(0);
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
                        && let Some(t) = event.text.as_ref()
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
            && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("p"))
        {
            self.set_pane_mode(!self.pane_mode);
            return true;
        }
        // pane control mode captures every key until exited
        if self.pane_mode {
            match &event.logical_key {
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
                let mut newly_ready = false;
                let mut cwd_changed = false;
                for tab in &mut self.pw.tabs {
                    if let Some(root) = tab.root.as_mut()
                        && let Some(p) = find_pane_mut(root, id) {
                            pump_bytes(p, &bytes);
                            // first output means the shell has settled past its
                            // PSReadLine startup, so it's now safe to resize
                            if !p.ready {
                                p.ready = true;
                                newly_ready = true;
                            }
                            if p.term.cwd_dirty {
                                p.term.cwd_dirty = false;
                                cwd_changed = true;
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
                            }
                            found = true;
                            break;
                        }
                }
                if !found
                    && let Some(idx) = self.satellites.iter().position(|s| s.pane.id == id)
                {
                    let sat = &mut self.satellites[idx];
                    pump_bytes(&mut sat.pane, &bytes);
                    if !sat.pane.term.responses.is_empty() {
                        let resp = std::mem::take(&mut sat.pane.term.responses);
                        sat.pane.pty.write(&resp);
                    }
                    if let Some(text) = sat.pane.term.clipboard.take() {
                        clip = Some(text);
                    }
                    sat.pane.term.bell = false;
                    self.paint_satellite(idx);
                    found = true;
                }
                // a pane that just became ready may need its deferred resize
                if newly_ready {
                    self.relayout_all();
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
                // relabel tabs only when a tab pane's cwd actually changed
                if cwd_changed {
                    self.sync_tabs();
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
                if let Some(idx) = self.satellites.iter().position(|s| s.pane.id == id) {
                    let mut sat = self.satellites.remove(idx);
                    sat.pane.pty.kill();
                    return;
                }
                // find which tab holds this pane, close that pane
                let owner = self.pw.tabs.iter().position(|t| {
                    t.root.as_ref().map(|r| find_pane(r, id).is_some()).unwrap_or(false)
                });
                if let Some(ti) = owner {
                    let prev_active = self.pw.active_tab;
                    self.pw.active_tab = ti;
                    self.close_focused_pane_by_id(id, event_loop);
                    if ti != prev_active && prev_active < self.pw.tabs.len() {
                        self.pw.active_tab = prev_active.min(self.pw.tabs.len().saturating_sub(1));
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
            UserEvent::Market(catalog) => {
                // the remote catalog arrived: merge it into the open overlay
                if self.market.is_some() {
                    let rows = self.market_rows(&catalog);
                    if let Some(m) = self.market.as_mut() {
                        m.selected = m.selected.min(rows.len().saturating_sub(1));
                        m.rows = rows;
                        m.status = if catalog.is_empty() {
                            "catalog unavailable (offline?) · showing installed only".to_string()
                        } else {
                            "enter: toggle/install · r: remove · esc: close".to_string()
                        };
                    }
                    self.redraw();
                }
            }
            UserEvent::ToggleQuake => self.toggle_quake(),
            UserEvent::Accessibility(e) => match e.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => self.update_a11y(),
                // read-only v1: the screen reader can't drive actions
                accesskit_winit::WindowEvent::ActionRequested(_) => {}
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // events for a torn-off pane's window go to its satellite handler
        if let Some(idx) = self.satellites.iter().position(|s| s.window.id() == id) {
            self.satellite_event(idx, &event);
            return;
        }
        // feed the main window's events to the accesskit adapter (focus/bounds)
        if let (Some(a), Some(w)) = (self.a11y.as_mut(), self.pw.window.as_ref()) {
            a.process_event(w, &event);
        }
        match event {
            WindowEvent::CloseRequested => {
                for tab in &mut self.pw.tabs {
                    if let Some(root) = tab.root.as_mut() {
                        kill_all(root);
                    }
                }
                self.flush_session_now();
                self.kill_pool();
                event_loop.exit();
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
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
                // while the pane menu is open, only track which item is hovered
                if self.pane_menu.is_some() {
                    let h = self.pw.renderer.as_ref().and_then(|r| r.pane_menu_item_at(px, py));
                    if let Some(m) = self.pane_menu.as_mut()
                        && m.hovered != h
                    {
                        m.hovered = h;
                        self.redraw();
                    }
                    return;
                }
                // mouse-tracking motion (1002 drag / 1003 any-motion)
                if self.drag_divider.is_none() && !self.settings_open && !self.mods.shift_key() {
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
                    if let (Some(mut sel), Some(cell)) =
                        (self.selection, self.cell_in_focused(px, py))
                    {
                        sel.end = cell;
                        self.selection = Some(sel);
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
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 3.0,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 20.0) as f32,
                };
                if let Some(id) = self.pane_at(cx, cy)
                    && let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
                        && let Some(root) = tab.root.as_mut()
                            && let Some(p) = find_pane_mut(root, id) {
                                // alt screen has no scrollback — don't local-scroll it
                                if !p.term.using_alt {
                                    p.term.grid.scroll_view(lines.round() as isize);
                                    self.redraw();
                                }
                            }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                // right-click on a pane opens the pane context menu (split / close
                // / paste) at the cursor; focus the pane under it first so the
                // actions target it
                let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
                let on_content =
                    matches!(self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy)), Some(Hit::Content));
                if on_content {
                    self.focus_pane_at(cx, cy);
                    self.pane_menu = Some(PaneMenu { x: cx, y: cy, hovered: None });
                    self.redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
                if let Some(Hit::Button(Hot::Tab(i) | Hot::TabClose(i))) =
                    self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy))
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
                let hit = self.pw.renderer.as_ref().map(|r| r.hit_test(cx, cy));
                // a left-press while the pane menu is open runs the clicked item
                // (or dismisses it when the click lands elsewhere)
                if self.pane_menu.is_some() && state == ElementState::Pressed {
                    let item = self.pw.renderer.as_ref().and_then(|r| r.pane_menu_item_at(cx, cy));
                    self.pane_menu = None;
                    if let Some(i) = item {
                        self.pane_menu_action(i, event_loop);
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
                if self.settings_open && state == ElementState::Pressed {
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
                // pane mode: drag a divider to resize, or drag a pane onto another
                // to swap them (instead of selecting text)
                if self.pane_mode && !matches!(hit, Some(Hit::Button(_)) | Some(Hit::TitleBar) | Some(Hit::Resize(_))) {
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
                                self.drag_pane = Some(id);
                                self.focus_pane_at(cx, cy);
                            }
                        }
                        ElementState::Released => {
                            if self.drag_divider.take().is_none()
                                && let Some(src) = self.drag_pane.take()
                                    && let Some(dst) = self.pane_at(cx, cy)
                                        && dst != src {
                                            if let Some(root) = self.pw.tabs.get_mut(self.pw.active_tab).and_then(|t| t.root.as_mut()) {
                                                swap_panes(root, src, dst);
                                            }
                                            self.relayout_all();
                                            self.sync_tabs();
                                            self.redraw();
                                        }
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
                        Some(Hit::Button(h)) => self.pressed = Some(h),
                        Some(Hit::Content) => {
                            self.focus_pane_at(cx, cy);
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
                            {
                                let grid = self
                                    .pw.tabs
                                    .get(self.pw.active_tab)
                                    .and_then(|t| t.root.as_ref())
                                    .and_then(|r| find_pane(r, pane))
                                    .map(|p| &p.term.grid);
                                match self.click_seq {
                                    2 => {
                                        let (lo, hi) = grid
                                            .map(|g| g.word_bounds(row, col))
                                            .unwrap_or((col, col));
                                        self.selection =
                                            Some(Sel { pane, start: (row, lo), end: (row, hi) });
                                        self.selecting = false;
                                        if self.config.copy_on_select {
                                            self.copy_selection();
                                        }
                                    }
                                    3 => {
                                        let hi =
                                            grid.map(|g| g.line_last_col(row)).unwrap_or(0);
                                        self.selection =
                                            Some(Sel { pane, start: (row, 0), end: (row, hi) });
                                        self.selecting = false;
                                        if self.config.copy_on_select {
                                            self.copy_selection();
                                        }
                                    }
                                    _ => {
                                        self.selection = Some(Sel {
                                            pane,
                                            start: (row, col),
                                            end: (row, col),
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
                        if let Some(h) = self.pressed.take()
                            && matches!(hit, Some(Hit::Button(hh)) if hh == h) {
                                self.button_action(event_loop, h);
                            }
                    }
                }
            }
            WindowEvent::Resized(size) => {
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
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // monitor/dpi change: re-raster the atlas at the new scale so text
                // stays crisp. winit applies the os-suggested size and a Resized
                // follows (which arms the resize-settle reflow at the new size)
                if let Some(r) = self.pw.renderer.as_mut() {
                    r.set_scale(scale_factor as f32);
                }
                self.relayout_all();
                self.redraw();
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Enabled => {}
                Ime::Preedit(text, _cursor) => {
                    self.ime_composing = !text.is_empty();
                    self.apply_ime_area();
                    self.redraw();
                }
                Ime::Commit(text) => {
                    self.ime_composing = false;
                    self.write_to_focused(text.as_bytes());
                    self.redraw();
                }
                Ime::Disabled => {
                    self.ime_composing = false;
                    self.redraw();
                }
            },
            WindowEvent::KeyboardInput { event, .. } => {
                // while composing, the IME owns keystrokes; ignore raw keys so a
                // committed glyph isn't also typed as its latin keys
                if self.ime_composing {
                    return;
                }
                if self.handle_shortcut(&event, event_loop) {
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
                    event.state,
                    event.repeat,
                    self.mods,
                    app_cursor,
                    kbd_flags,
                ) {
                    self.selection = None; // typing clears the selection
                    self.write_to_focused(&bytes);
                }
            }
            WindowEvent::DroppedFile(path) => {
                // typing a dropped file's path at the prompt; quote it if it has
                // spaces so the shell treats it as a single argument
                if let Some(id) = self.active_focused_id() {
                    let s = path.to_string_lossy();
                    let text = if s.contains(' ') {
                        format!("\"{s}\" ")
                    } else {
                        format!("{s} ")
                    };
                    if let Some(tab) = self.pw.tabs.get_mut(self.pw.active_tab)
                        && let Some(root) = tab.root.as_mut()
                        && let Some(p) = find_pane_mut(root, id)
                    {
                        p.pty.write(text.as_bytes());
                    }
                }
            }
            WindowEvent::RedrawRequested => self.paint(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // refresh the screen-reader tree (no-op unless an assistive tech is on)
        self.update_a11y();
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
        // coalesced pty-output paint: one redraw per loop turn no matter how many
        // pty chunks arrived since the last frame
        if self.pty_dirty {
            self.pty_dirty = false;
            self.redraw();
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
                .pw.renderer
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
    timing("process start");
    // stop child shells (esp. pool shells racing exit) from popping OS error dialogs
    win::suppress_child_error_dialogs();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cmd = p(&["--", "vim", "a.txt"]);
        assert_eq!(
            cmd.command.as_deref(),
            Some(&["vim".to_string(), "a.txt".to_string()][..])
        );
        assert!(!cmd.is_bare());
        // unknown flags are ignored, not misread as a cwd or command
        assert!(p(&["--frobnicate"]).is_bare());
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
        assert!(has(cs, "e", PaletteAction::SplitV));
        assert!(has(ctrl, "1", PaletteAction::SelectTab(0)));
        assert!(has(ctrl, "9", PaletteAction::SelectTab(8)));
        // '+' and '_' are typed with shift, so they must be bound Ctrl+Shift to
        // match the modifiers that actually arrive (regression guard)
        assert!(has(cs, "+", PaletteAction::FontInc));
        assert!(has(cs, "_", PaletteAction::FontDec));
        // Ctrl+Shift+P is deliberately NOT a default (dedicated pane-mode handler)
        assert!(!d.iter().any(|(m, k, _)| *m == cs && key_matches(&Key::Character("p".into()), k)));
        // label resolution covers palette + keybinding-only + select-tab
        assert_eq!(action_from_label("new tab"), Some(PaletteAction::NewTab));
        assert_eq!(action_from_label("copy"), Some(PaletteAction::Copy));
        assert_eq!(action_from_label("select tab 3"), Some(PaletteAction::SelectTab(2)));
        assert_eq!(action_from_label("bogus action"), None);
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
        assert_eq!(palette_filter("").len(), PALETTE_ACTIONS.len());
    }

    #[test]
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
    fn swap_panes_exchanges_two_leaves_in_place() {
        let mut tree = split(Dir::Vertical, 0.5, leaf(1), split(Dir::Horizontal, 0.5, leaf(2), leaf(3)));
        swap_panes(&mut tree, 1, 3);
        // structure is unchanged, but the payloads at the 1 and 3 slots traded
        assert_eq!(ids(&tree), vec![3, 2, 1]);
        // first_leaf follows the a-side spine, now holding the swapped-in pane 3
        assert_eq!(first_leaf(&tree), 3);
        // swapping an id with itself is a no-op
        swap_panes(&mut tree, 2, 2);
        assert_eq!(ids(&tree), vec![3, 2, 1]);
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
}
