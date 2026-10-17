mod atlas;
mod boxdraw;
mod icons;
pub mod motion;
pub mod tokens;
#[cfg(any(test, debug_assertions))]
pub mod preview;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Result};
use bytemuck::{Pod, Zeroable};
use winit::window::{ResizeDirection, Window};

use crate::color::{Color, Palette, Rgb, ThemeId};
use crate::grid::CursorShape;
use crate::term::Terminal;
pub(crate) use atlas::{FontId, GlyphAtlas, GlyphKey};
use icons::Icon;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    color: [f32; 4],
    kind: u32,
    /// kind 4 only: (corner radius, shadow blur, ring width) in pixels
    shape: [f32; 3],
}

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x2,
    3 => Float32x2,
    4 => Float32x4,
    5 => Uint32,
    6 => Float32x3,
];

/// build a full mip chain from a 32bpp RGBA base via 2x2 box downsample;
/// returns (dim, data) per level from `dim` down to 1. used so the icon badge
/// stays crisp when scaled far down in the title bar
fn build_mips(base: &[u8], dim: u32) -> Vec<(u32, Vec<u8>)> {
    let mut levels: Vec<(u32, Vec<u8>)> = vec![(dim, base.to_vec())];
    let mut d = dim;
    // 3 levels (128→64→32) is enough mip coverage for the ~20px title-bar badge;
    // smaller levels never get sampled, so building them is wasted startup work
    while d > 32 {
        let nd = d / 2;
        let pd = d;
        let prev = &levels.last().unwrap().1;
        let mut out = vec![0u8; (nd * nd * 4) as usize];
        for y in 0..nd {
            for x in 0..nd {
                for ch in 0..4 {
                    let mut sum = 0u32;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let sx = x * 2 + dx;
                            let sy = y * 2 + dy;
                            sum += prev[((sy * pd + sx) * 4 + ch) as usize] as u32;
                        }
                    }
                    out[((y * nd + x) * 4 + ch) as usize] = (sum / 4) as u8;
                }
            }
        }
        levels.push((nd, out));
        d = nd;
    }
    levels
}

/// every hoverable/clickable chrome target
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hot {
    WindowTabs,
    Minimize,
    Maximize,
    Close,
    Gear,
    SplitV,
    SplitH,
    PaneMode,
    NewTab,
    /// the chevron next to '+' that opens the new-tab profile menu
    NewTabMenu,
    Tab(usize),
    TabClose(usize),
    PanelClose,
    // settings controls
    FontDec,
    FontInc,
    FontCycle,
    PadDec,
    PadInc,
    OpacityDec,
    OpacityInc,
    CursorCycle,
    CursorBlink,
    ThemeSet(ThemeId),
    /// follow the windows light/dark setting (theme=auto)
    ThemeAuto,
    LineHeightDec,
    LineHeightInc,
    BoldBright,
    Mica,
    ScrollbackDec,
    ScrollbackInc,
    CopyOnSelect,
    ShellCycle,
    LoadProfile,
    CloseActionCycle,
    BackendCycle,
    OpenPlugins,
    /// toggle the enabled state of installed plugin at this index
    PluginToggle(usize),
}

fn drag_hint_variants(hot: Option<Hot>) -> &'static [(&'static str, &'static str)] {
    match hot {
        Some(Hot::WindowTabs) => &[
            ("DRAG BADGE", "merge all tabs into another window"),
            ("DRAG BADGE", "merge all tabs"),
        ],
        Some(Hot::Tab(_)) => &[
            ("DRAG TAB", "reorder \u{00b7} move between windows \u{00b7} pull out"),
            ("DRAG TAB", "move \u{00b7} pull out"),
        ],
        _ => &[],
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneDropSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// a terminal to draw at a pixel rect within the window
pub struct PaneView<'a> {
    pub term: &'a Terminal,
    pub rect: (f32, f32, f32, f32),
    pub focused: bool,
    /// active selection range (row, col) within this pane's viewport
    pub sel: Option<SelSpan>,
    /// accent-border opacity after the shell rang the bell: 1 then eased to 0
    /// (0 = no flash) so the bell border fades out instead of snapping off
    pub flash: f32,
    /// hovered url to underline: (viewport row, col_start, col_end exclusive)
    pub link: Option<(usize, usize, usize)>,
    /// per-profile theme override (`theme.<shell>=` in config): this pane
    /// paints with that theme's palette while the window chrome keeps the
    /// global theme; None follows the window
    pub theme: Option<ThemeId>,
    /// command badge severity for the pane's corner dot (0 none, 1 running,
    /// 2 done, 4 failed — same scale as the tab strip)
    pub status: u8,
    /// in-flight IME composition to draw at this pane's cursor (focused pane
    /// only); the string lives in the compositor until commit, never the grid
    pub preedit: Option<PreeditView<'a>>,
}

/// an uncommitted IME composition: the text plus winit's cursor — a byte range
/// into it marking the active clause (start < end) or the caret (start == end)
#[derive(Clone, Copy)]
pub struct PreeditView<'a> {
    pub text: &'a str,
    pub caret: Option<(usize, usize)>,
}

/// terminal cells occupied by `text` up to byte offset `at` (caret math for
/// both the inline overlay and parking the OS candidate window)
pub(crate) fn preedit_cell_offset(text: &str, at: usize) -> usize {
    text.char_indices()
        .take_while(|&(i, _)| i < at)
        .map(|(_, c)| crate::grid::char_width(c))
        .sum()
}

/// command-palette display state
pub struct PaletteView {
    pub query: String,
    pub items: Vec<String>,
    /// parallel to `items`: the row's shortcut ("Ctrl+Shift+E"), empty if none
    pub hints: Vec<String>,
    pub selected: usize,
    pub scope: &'static str,
}

/// right-click context menu: a small overlay at (x, y) listing `items`. the
/// clicked item index maps to an action in main.rs's handler, keyed to the
/// menu's target (pane, tab, or new-tab profiles). items are owned because the
/// new-tab menu's profile labels are built at runtime
pub struct PaneMenuView {
    pub x: f32,
    pub y: f32,
    pub hovered: Option<usize>,
    pub items: Vec<String>,
    /// per-row swatch tints (ansi palette indices) for the tab-color menu;
    /// empty for every other menu. a None row draws a hollow "none" box
    pub accents: Vec<Option<u8>>,
}

pub const PANE_MENU_ITEMS: [&str; 6] =
    ["copy", "split vertical", "split horizontal", "pop out to window", "close pane", "paste"];

pub const TAB_MENU_ITEMS: [&str; 8] = [
    "rename",
    "duplicate",
    "bookmark location",
    "color",
    "move left",
    "move right",
    "close",
    "close others",
];

/// rows of the per-tab color menu; index n > 0 is ansi palette color n
pub const TAB_COLOR_ITEMS: [&str; 7] = ["none", "red", "green", "yellow", "blue", "magenta", "cyan"];

/// find-in-scrollback overlay display state. `matches` are on-screen rects
/// (viewport row, col, len, is_current) for the focused pane
pub struct FindView {
    pub query: String,
    pub count: usize,
    pub current: usize,
    pub capped: bool,
    pub matches: Vec<(usize, usize, usize, bool)>,
    /// regex mode is on (the .* button paints lit)
    pub regex_on: bool,
    /// the pattern failed to compile; the counter slot says so
    pub bad: bool,
}

/// a modal confirm overlay: a centered box with a prompt + a key hint, shown
/// until the user presses enter (confirm) or esc (cancel)
pub struct ConfirmView {
    pub prompt: String,
    pub hint: String,
}

/// the tab-rename text field overlay
pub struct RenameView {
    pub buf: String,
}

/// one row in the plugins marketplace overlay
pub struct MarketRowView {
    pub name: String,
    pub version: String,
    /// one-line description from the catalog (empty if unknown)
    pub description: String,
    /// declared permissions, shown as badges
    pub permissions: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
}

/// plugins marketplace overlay display state
pub struct MarketView {
    pub rows: Vec<MarketRowView>,
    pub selected: usize,
    pub status: String,
    /// true while the remote catalog is still being fetched
    pub loading: bool,
    /// set when the catalog fetch failed (vs. simply empty)
    pub fetch_failed: bool,
}

/// what a click in the marketplace overlay landed on; row indices are into the
/// view's `rows`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketHit {
    /// the card body — select the row
    Card(usize),
    /// the install/enable/disable action chip — act on the row
    Chip(usize),
    /// the close (×) control
    Close,
}

/// a plugin-declared widget to draw in the side dock. render-side mirror of the
/// plugin protocol's Widget, so the renderer doesn't depend on the plugin module.
/// `draw` is the Tier-2 immediate-mode primitive list (empty for a Tier-1 widget)
#[derive(Clone, Default)]
pub struct DockWidget {
    pub title: String,
    pub lines: Vec<String>,
    pub draw: Vec<DockDraw>,
    pub canvas_h: Option<f32>,
}

/// a Tier-2 draw primitive in dock-canvas space (coords normalized 0..1). render
/// mirror of the protocol's DrawCmd; `color` is resolved against the active
/// palette at draw time
#[derive(Clone)]
pub enum DockDraw {
    Rect { x: f32, y: f32, w: f32, h: f32, color: String },
    Text { x: f32, y: f32, text: String, color: String },
}

pub enum Hit {
    Button(Hot),
    TitleBar,
    Resize(ResizeDirection),
    Content,
}

/// what the focused pane's shell is doing, for the status bar
#[derive(Clone, Copy, Default, PartialEq)]
pub enum CommandState {
    #[default]
    Idle,
    Running(Instant),
    Finished { code: Option<i32>, took: std::time::Duration },
}

/// the status bar's left cluster and command readout, fed by the app from the
/// focused pane
#[derive(Clone, Default)]
pub struct StatusView {
    /// home-abbreviated working directory
    pub cwd: Option<String>,
    pub command: CommandState,
}

/// scroll thumb + track rectangle for a pane, in window pixels. shared by the
/// painter and the input hit-test so the grabbable strip is exactly what's drawn
#[derive(Clone, Copy)]
pub struct ScrollThumb {
    pub track_x: f32,
    pub track_y: f32,
    pub track_w: f32,
    pub track_h: f32,
    pub thumb_y: f32,
    pub thumb_h: f32,
}

/// read-only mirror of the App-owned settings so the renderer can label them;
/// renderer-owned values (font/padding/cursor/blink/theme) live on `Renderer`
#[derive(Clone, Copy)]
pub struct SettingsView {
    pub scrollback: usize,
    pub copy_on_select: bool,
    pub load_profile: bool,
    /// theme=auto: the trailing chip is lit and the theme follows the OS
    pub theme_auto: bool,
    /// system backdrop blur on/off (config `acrylic=`), toggled live
    pub acrylic: bool,
    pub shell_name: &'static str,
    pub close_action_name: &'static str,
    pub backend_name: &'static str,
}

impl Default for SettingsView {
    fn default() -> Self {
        SettingsView {
            scrollback: 10_000,
            copy_on_select: false,
            load_profile: false,
            theme_auto: false,
            acrylic: false,
            shell_name: "auto",
            close_action_name: "quit",
            backend_name: "auto",
        }
    }
}

type Rect = (f32, f32, f32, f32);
/// a tab in the title bar: (session index, tab rect, close-icon rect)
type TabEntry = (usize, Rect, Rect);
/// resolved per-pane paint origin: (origin x, origin y, focused, pane rect)
/// grid origin x/y, focused, rect, and the background the pane paints with
type PaneInfo = (f32, f32, bool, Rect, Rgb);
/// a selection span in viewport cells: (start, end, block) — block marks an
/// alt+drag rectangular selection
pub type SelSpan = ((usize, usize), (usize, usize), bool);
/// ascii punctuation that programming fonts chain into ligatures (==> !=
/// :: -> ...); letters and digits never join a run, so prose costs nothing
fn liga_char(c: char) -> bool {
    matches!(
        c,
        '=' | '<' | '>' | '!' | '&' | '|' | '+' | '-' | '*' | '/' | '~' | ':' | ';' | '?' | '#' | '_' | '^' | '.' | '%' | '$' | '@'
    )
}

/// snapshot of a tab row for painting: index, tab rect, close rect, label,
/// active, hovered, close-hovered, badge severity (0 none .. 4 failed),
/// user-picked tint (ansi palette index)
/// index, rect, close rect, label, active, hover, close hover, status, tint
type TabItem = (usize, Rect, Rect, String, bool, f32, f32, u8, Option<u8>);

/// GPU backend choice for compatibility; persisted + applied at startup
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendChoice {
    Auto,
    Dx12,
    Vulkan,
    Gl,
}

impl BackendChoice {
    pub fn next(self) -> Self {
        match self {
            BackendChoice::Auto => BackendChoice::Dx12,
            BackendChoice::Dx12 => BackendChoice::Vulkan,
            BackendChoice::Vulkan => BackendChoice::Gl,
            BackendChoice::Gl => BackendChoice::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::Dx12 => "dx12",
            BackendChoice::Vulkan => "vulkan",
            BackendChoice::Gl => "gl",
        }
    }

    pub fn from_label(s: &str) -> Self {
        match s {
            "dx12" => BackendChoice::Dx12,
            "vulkan" => BackendChoice::Vulkan,
            "gl" => BackendChoice::Gl,
            _ => BackendChoice::Auto,
        }
    }

    fn to_backends(self) -> wgpu::Backends {
        match self {
            // Auto = DX12 on Windows (Vulkan is slow under overlay layers)
            BackendChoice::Auto => {
                if cfg!(windows) {
                    wgpu::Backends::DX12
                } else {
                    wgpu::Backends::all()
                }
            }
            BackendChoice::Dx12 => wgpu::Backends::DX12,
            BackendChoice::Vulkan => wgpu::Backends::VULKAN,
            BackendChoice::Gl => wgpu::Backends::GL,
        }
    }
}

/// full geometry for the settings page; shared by `build_settings` (drawing)
/// and `hit_test` (the `controls` list), so the two never drift
struct SettingsGeom {
    // slide-in panel frame
    panel_x: f32,
    panel_w: f32,
    panel_top: f32,
    panel_h: f32,
    // scrollable body viewport + total content height (for scroll clamp)
    body_top: f32,
    body_bottom: f32,
    content_h: f32,
    // body content metrics
    content_x: f32,
    content_w: f32,
    bh: f32,
    val_w: f32,
    head_y: f32,
    close_btn: Rect,
    fontfam_y: f32,
    fontfam_btn: Rect,
    // section header baselines (absolute, scroll-adjusted)
    sec_app_y: f32,
    sec_beh_y: f32,
    sec_shell_y: f32,
    sec_plugins_y: f32,
    sec_keys_y: f32,
    sec_about_y: f32,
    // row label baselines (absolute, scroll-adjusted)
    font_y: f32,
    pad_y: f32,
    opacity_y: f32,
    line_height_y: f32,
    boldbright_y: f32,
    mica_y: f32,
    cursor_y: f32,
    blink_y: f32,
    theme_label_y: f32,
    scrollback_y: f32,
    copysel_y: f32,
    shell_y: f32,
    profile_y: f32,
    close_y: f32,
    backend_y: f32,
    /// (name, enabled, toggle rect, row baseline) per installed plugin
    plugin_rows: Vec<(String, bool, Rect, f32)>,
    keys_start_y: f32,
    about_start_y: f32,
    // interactive rects (absolute, scroll-adjusted)
    font_dec: Rect,
    font_inc: Rect,
    pad_dec: Rect,
    pad_inc: Rect,
    op_dec: Rect,
    op_inc: Rect,
    lh_dec: Rect,
    lh_inc: Rect,
    boldbright_btn: Rect,
    mica_btn: Rect,
    cursor_btn: Rect,
    blink_btn: Rect,
    // one extra slot: the trailing chip is the follow-the-OS "auto" toggle
    theme_chips: [Rect; crate::color::ThemeId::ALL.len() + 1],
    sb_dec: Rect,
    sb_inc: Rect,
    copysel_btn: Rect,
    shell_btn: Rect,
    profile_btn: Rect,
    close_action_btn: Rect,
    backend_btn: Rect,
    plugins_btn: Rect,
    /// body controls only (absolute); `close_btn` is handled separately
    controls: Vec<(Hot, Rect)>,
}

struct TabLayout {
    /// (session index, tab rect, close-icon rect)
    tabs: Vec<TabEntry>,
    newtab: (f32, f32, f32, f32),
    /// the profile-menu chevron, flush against the right edge of `newtab`
    newtab_menu: (f32, f32, f32, f32),
}

pub(crate) struct A11yChromeLayout {
    pub title_bar: (f32, f32, f32, f32),
    pub tabs: Vec<(usize, (f32, f32, f32, f32))>,
    pub new_tab: (f32, f32, f32, f32),
    pub new_tab_menu: (f32, f32, f32, f32),
    pub controls: Vec<(Hot, (f32, f32, f32, f32))>,
}

/// an overlay opening: fade everything in, and grow the surfaces from 98% about
/// their shared center (full-width scrims and pages only fade, and reduced
/// motion keeps just the fade)
fn bloom(range: &mut [Instance], p: f32, window_w: f32) {
    let full = |i: &Instance| i.size[0] >= window_w;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for i in range.iter().filter(|i| !full(i)) {
        x0 = x0.min(i.pos[0]);
        y0 = y0.min(i.pos[1]);
        x1 = x1.max(i.pos[0] + i.size[0]);
        y1 = y1.max(i.pos[1] + i.size[1]);
    }
    let k = if motion::reduced() || x0 > x1 { 1.0 } else { 0.98 + 0.02 * p };
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    for i in range.iter_mut() {
        i.color[3] *= p;
        if k < 1.0 && !full(i) {
            i.pos = [cx + (i.pos[0] - cx) * k, cy + (i.pos[1] - cy) * k];
            i.size = [i.size[0] * k, i.size[1] * k];
            if i.kind == 4 {
                i.uv_max = i.size;
                i.shape = [i.shape[0] * k, i.shape[1] * k, i.shape[2] * k];
            }
        }
    }
}

/// the shared edge between two tiled panes as a hairline rect, if they touch
/// along a side (within `tol` px of each other)
fn pane_seam(a: Rect, b: Rect, tol: f32, hair: f32) -> Option<Rect> {
    for (l, r) in [(a, b), (b, a)] {
        let gap = r.0 - (l.0 + l.2);
        let (y0, y1) = (l.1.max(r.1), (l.1 + l.3).min(r.1 + r.3));
        if (-0.5..=tol).contains(&gap) && y1 > y0 {
            return Some(((l.0 + l.2 + gap / 2.0 - hair / 2.0).round(), y0, hair, y1 - y0));
        }
        let gap = r.1 - (l.1 + l.3);
        let (x0, x1) = (l.0.max(r.0), (l.0 + l.2).min(r.0 + r.2));
        if (-0.5..=tol).contains(&gap) && x1 > x0 {
            return Some((x0, (l.1 + l.3 + gap / 2.0 - hair / 2.0).round(), x1 - x0, hair));
        }
    }
    None
}

/// compact elapsed time: 3.2s, 42s, 3m 05s, 1h 02m
fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..10 => format!("{:.1}s", d.as_secs_f32()),
        10..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs / 60 % 60),
    }
}

/// keep the tail of a path that's too long: "…/src/render"
fn truncate_head(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    std::iter::once('\u{2026}').chain(s.chars().skip(n - keep)).collect()
}

fn truncate_tail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).chain(std::iter::once('\u{2026}')).collect()
}

/// shrink tab widths to fit `avail`: lower a shared cap until the total fits
/// (widest give way first), and never go below an even share
fn fit_widths(natural: &[f32], avail: f32) -> Vec<f32> {
    let total: f32 = natural.iter().sum();
    if total <= avail || natural.is_empty() {
        return natural.to_vec();
    }
    let mut sorted = natural.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mut rest = avail;
    let mut cap = avail / natural.len() as f32;
    for (i, w) in sorted.iter().enumerate() {
        let share = rest / (sorted.len() - i) as f32;
        if *w > share {
            cap = share;
            break;
        }
        rest -= w;
    }
    natural.iter().map(|w| w.min(cap).max(1.0)).collect()
}

fn in_rect(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

struct LinkPreview {
    display: String,
    host_end: usize,
    display_chars: usize,
    host_chars: usize,
    fitted: String,
    fitted_chars: usize,
    fitted_for: usize,
}

impl LinkPreview {
    fn new(target: &str) -> Self {
        let (scheme, rest) = if target
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        {
            ("https", &target[8..])
        } else if target
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        {
            ("http", &target[7..])
        } else {
            ("", target)
        };
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let suffix = &rest[authority_end..];
        let display = if scheme.is_empty() {
            target.to_string()
        } else {
            format!("{host}  {scheme}{suffix}")
        };
        let host_end = if scheme.is_empty() { authority_end } else { host.len() };
        Self {
            display_chars: display.chars().count(),
            host_chars: display[..host_end].chars().count(),
            display,
            host_end,
            fitted: String::new(),
            fitted_chars: 0,
            fitted_for: usize::MAX,
        }
    }

    fn fit(&mut self, max_chars: usize) {
        if self.fitted_for == max_chars {
            return;
        }
        self.fitted_for = max_chars;
        self.fitted.clear();
        if max_chars == 0 {
            self.fitted_chars = 0;
            return;
        }
        if self.display_chars <= max_chars {
            self.fitted.push_str(&self.display);
            self.fitted_chars = self.display_chars;
            return;
        }
        let host = &self.display[..self.host_end];
        if self.host_chars > max_chars {
            if max_chars == 1 {
                self.fitted.push('\u{2026}');
            } else {
                let visible = max_chars - 1;
                let prefix_chars = visible.div_ceil(2);
                let suffix_chars = visible - prefix_chars;
                let prefix_end = host
                    .char_indices()
                    .nth(prefix_chars)
                    .map(|(index, _)| index)
                    .unwrap_or(host.len());
                let suffix_start = host
                    .char_indices()
                    .nth(self.host_chars - suffix_chars)
                    .map(|(index, _)| index)
                    .unwrap_or(host.len());
                self.fitted.push_str(&host[..prefix_end]);
                self.fitted.push('\u{2026}');
                self.fitted.push_str(&host[suffix_start..]);
            }
            self.fitted_chars = max_chars;
            return;
        }
        self.fitted.push_str(host);
        let remaining = max_chars - self.host_chars;
        if remaining >= 2 {
            self.fitted
                .extend(self.display[self.host_end..].chars().take(remaining - 1));
            self.fitted.push('\u{2026}');
            self.fitted_chars = max_chars;
        } else {
            self.fitted_chars = self.host_chars;
        }
    }
}

/// emit the rects (cell-local x,y,w,h) that draw an underline of the given
/// style; shared by the GPU renderer and the dev PNG preview so they match.
/// `ascent`/`px` anchor the line to the font's baseline — at line heights
/// above 1.0 the cell bottom floats in the leading, well clear of the text
fn underline_rects(
    style: crate::grid::UnderlineStyle,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    px: f32,
    t: f32,
    mut emit: impl FnMut(f32, f32, f32, f32),
) {
    use crate::grid::UnderlineStyle as U;
    // just under the baseline (~10% of an em), clamped inside the cell
    let yb = (ascent + 0.10 * px).round().min(cell_h - t).max(0.0);
    match style {
        U::None => {}
        U::Single => emit(0.0, yb, cell_w, t),
        U::Double => {
            emit(0.0, yb, cell_w, t);
            emit(0.0, (yb + t * 2.0).min(cell_h - t), cell_w, t);
        }
        U::Dotted => {
            let step = (t * 2.0).max(2.0);
            let mut dx = 0.0;
            while dx < cell_w {
                emit(dx, yb, t.min(cell_w - dx), t);
                dx += step;
            }
        }
        U::Dashed => {
            let dash = (cell_w / 3.0).max(2.0);
            let mut dx = 0.0;
            while dx < cell_w {
                emit(dx, yb, dash.min(cell_w - dx), t);
                dx += dash * 2.0;
            }
        }
        U::Curly => {
            // a connected per-pixel-column wave: each column's segment spans
            // to the next column's phase, so a steep slope can't leave the
            // vertical gaps the old fixed-height dots did
            let amp = (t * 1.25).max(1.5);
            let cy = (yb + amp).min(cell_h - amp - t).max(amp);
            let cols = cell_w.max(1.0) as i32;
            let phase =
                |dx: f32| ((dx / cell_w) * std::f32::consts::TAU).sin() * amp;
            for i in 0..cols {
                let dx = i as f32;
                let y0 = phase(dx);
                let y1 = phase(dx + 1.0);
                let top = y0.min(y1);
                emit(dx, cy + top, 1.0, (y0.max(y1) - top) + t);
            }
        }
    }
}


pub struct Renderer {
    /// None for a headless (offscreen) renderer used by the dev capture harness
    surface: Option<wgpu::Surface<'static>>,
    /// offscreen render target + readback buffer for the headless harness; None
    /// for the normal windowed renderer, which draws straight to the surface.
    /// only read by the debug-only capture path, so release sees it as unused
    #[cfg_attr(not(any(test, debug_assertions)), allow(dead_code))]
    offscreen: Option<(wgpu::Texture, wgpu::Buffer)>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    uploaded_screen: Option<[u32; 2]>,

    atlas_texture: wgpu::Texture,
    color_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    /// kept alive for the icon badge texture referenced by atlas_bind_group
    _icon_texture: wgpu::Texture,
    /// samplers + icon view referenced by atlas_bind_group, kept so the bind
    /// group can be rebuilt when the atlas grows (the 1024 -> 2048 grow path)
    sampler: wgpu::Sampler,
    icon_view: wgpu::TextureView,
    icon_sampler: wgpu::Sampler,
    color_sampler: wgpu::Sampler,
    /// the dim the gpu atlas textures were created at; when atlas.dim outgrows it
    /// upload_atlas recreates the textures + bind group at the new size
    atlas_gpu_dim: u32,
    /// set by the device-lost callback; render() recreates the gpu on the next
    /// frame so a driver reset / TDR doesn't permanently freeze the window
    device_lost: Arc<AtomicBool>,
    /// the window, kept so recreate() can rebuild the surface (None headless)
    window: Option<Arc<Window>>,
    /// persistent compositor connection for rebuilding a window surface
    display_handle: Option<winit::event_loop::OwnedDisplayHandle>,

    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    /// persistent CPU instance buffer reused across frames (cleared, not
    /// reallocated, each build) to avoid per-frame heap churn on the hot path
    scratch: Vec<Instance>,
    /// persistent pane-origin buffer reused across frames, same idea as scratch
    pane_scratch: Vec<PaneInfo>,

    atlas: GlyphAtlas,
    palette: Palette,
    /// palettes for panes whose profile overrides the window theme, built on
    /// demand in build() and dropped when the color overrides change
    pane_palettes: Vec<(ThemeId, Palette)>,

    scale: f32,
    pad: f32,
    content_pt: f32,
    content_line_height: f32,
    chrome_pt: f32,
    pub title_bar_h: f32,
    pub status_bar_h: f32,
    bg_alpha: f32,
    /// whether the surface supports translucency, and the user's chosen window
    /// opacity (0..1) applied as bg_alpha when it does
    transparent: bool,
    opacity: f32,
    start: Instant,
    /// independent clock for the power-on reveal, restarted the moment the
    /// window is shown so the whole animation plays in view (the gpu-init wait
    /// would otherwise eat most of it before the first visible frame)
    reveal_start: Instant,
    hovered: Option<Hot>,
    /// when the current hovered target was entered, for the hover fade-in
    hover_since: Option<Instant>,
    /// the target the pointer just left, fading its hover back out
    hover_prev: Option<(Hot, Instant)>,
    /// the focused pane's rect last frame, and the previously focused rect
    /// with when it lost focus, so focus crossfades instead of snapping
    focus_rect: Option<Rect>,
    focus_prev: Option<(Rect, Instant)>,
    /// parallel to `tabs`: when a background tab's command finished, so its
    /// rail draws in, holds, and settles into a dot
    tab_signal: Vec<Option<Instant>>,
    /// when the grid size last changed, for the transient size readout
    size_flash: Option<Instant>,
    /// the context-menu row under the pointer and when it lit, for the fade-in
    menu_hover: Option<(usize, Instant)>,
    /// (previous active tab index, start) so the active-tab accent rail slides
    /// to the newly selected tab instead of teleporting
    tab_slide: Option<(usize, Instant)>,
    /// when a centered overlay (palette/find/market/pane menu) opened, so it
    /// blooms in instead of popping; `overlay_shown` tracks last-frame presence
    overlay_since: Option<Instant>,
    overlay_shown: bool,
    settings_open: bool,
    settings_p: f32,
    settings_scroll: f32,
    /// (first body-instance index, clip rect) for the scissored panel scroll
    panel_clip: Option<(u32, [f32; 4])>,
    cursor_style: CursorShape,
    cursor_blink: bool,
    bold_as_bright: bool,
    pane_pad_px: f32,
    content_font: Option<&'static str>,
    /// base weight regular content text shapes at (100-900, default 400)
    content_weight: u16,
    /// minimum WCAG contrast ratio for text (1.0 = off)
    min_contrast: f32,
    /// window background image: (atlas cache key, rgba, w, h), drawn over the
    /// theme wash at bg_image_alpha; panes show it through their default bg
    bg_image: Option<(u64, Vec<u8>, u32, u32)>,
    bg_image_alpha: f32,
    /// shape punctuation runs through the font's ligature rules (default on)
    ligatures: bool,
    fonts: Vec<&'static str>,
    font_idx: usize,
    /// the gpu backend actually resolved at init (for the settings ABOUT block)
    backend_label: &'static str,
    settings_view: SettingsView,
    theme: ThemeId,
    /// user color overrides loaded from disk, applied on top of the theme
    color_overrides: Vec<(String, Rgb)>,
    broadcast: bool,
    /// cached background gradient quads, rebuilt only on size/theme change
    gradient_cache: Vec<Instance>,
    gradient_key: (u32, u32, ThemeId),
    pane_mode: bool,
    pane_drop: Option<((f32, f32, f32, f32), PaneDropSide)>,
    tab_drop: Option<usize>,
    /// pointer position, source label, and payload kind
    drag_preview: Option<(f32, f32, String, &'static str)>,
    mark_mode: bool,
    /// this process holds an admin token: a persistent shield chip on the
    /// status bar so an elevated window is never mistaken for a normal one
    elevated: bool,
    tabs: Vec<String>,
    /// parallel to `tabs`: badge severity — 0 none, 1 running, 2 done,
    /// 3 bell, 4 failed (the tab dot colors by it)
    tab_status: Vec<u8>,
    /// parallel to `tabs`: user-picked tint (ansi palette index 1-6)
    tab_colors: Vec<Option<u8>>,
    active_tab: usize,
    status: StatusView,
    /// transient program-notification text shown in the status bar's right
    /// cluster in place of READY (the app expires it after a few seconds)
    notice: Option<String>,
    link_preview: Option<LinkPreview>,
    /// a newer release's version: draws a clickable UPDATE chip on the status
    /// bar until installed. its hit rect is rebuilt each paint
    update_chip: Option<String>,
    update_hit: Option<(f32, f32, f32, f32)>,
    /// cached status-bar strings so the per-frame paint doesn't re-format them:
    /// (cols, rows, "W×H") and (command-state key, text)
    status_size: (usize, usize, String),
    status_cmd: (u64, String),
    /// installed plugins (display name, enabled) for the settings PLUGINS panel
    plugins_installed: Vec<(String, bool)>,
    palette_view: Option<PaletteView>,
    /// palette overlay hit geometry, refreshed each build: box, row pitch,
    /// shown item count, window offset (None while the palette is closed)
    palette_geom: Option<(Rect, f32, usize, usize)>,
    /// find-bar button rects, refreshed each build: (prev, next, close)
    find_btns: Option<(Rect, Rect, Rect, Rect)>,
    pane_menu_view: Option<PaneMenuView>,
    find_view: Option<FindView>,
    market_view: Option<MarketView>,
    /// per-frame clickable regions for the market overlay (close, cards, chips),
    /// rebuilt by build_market so a click can route to a row or its action
    market_hits: Vec<((f32, f32, f32, f32), MarketHit)>,
    confirm_view: Option<ConfirmView>,
    rename_view: Option<RenameView>,
    /// plugin-declared Tier-1 widgets shown in the right-side dock; when
    /// non-empty the dock carves width off content_rect so panes reflow
    dock: Vec<DockWidget>,
    /// per-widget clickable band (x, y, w, h), parallel to `dock`; rebuilt each
    /// frame by draw_dock so widget_at can route clicks to the owning plugin
    dock_hitboxes: Vec<(f32, f32, f32, f32)>,
    /// optional input-to-photon hud line, drawn bottom-left for latency testing
    latency_hud: Option<String>,

    pub cols: usize,
    pub rows: usize,
}

/// the uniform bind group layout (group 0): the screen-size uniform buffer
fn build_uniform_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("uniform-bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

/// the atlas bind group layout (group 1): alpha glyph atlas (0/1), app icon
/// (2/3), and the color-emoji atlas (4/5). kept in sync with shader.wgsl
fn build_atlas_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let tex = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let samp = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("atlas-bgl"),
        entries: &[tex(0), samp(1), tex(2), samp(3), tex(4), samp(5)],
    })
}

/// create the R8 coverage atlas + RGBA color atlas at `dim` and the 6-entry
/// atlas bind group over them (reusing the fixed icon view + the three
/// samplers). shared by from_parts and the 1024 -> 2048 grow so the layout can
/// never drift between the two
fn make_atlas_bind_group(
    device: &wgpu::Device,
    dim: u32,
    sampler: &wgpu::Sampler,
    icon_view: &wgpu::TextureView,
    icon_sampler: &wgpu::Sampler,
    color_sampler: &wgpu::Sampler,
) -> (wgpu::Texture, wgpu::Texture, wgpu::BindGroup) {
    let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph-atlas"),
        size: wgpu::Extent3d { width: dim, height: dim, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let color_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("color-glyph-atlas"),
        size: wgpu::Extent3d { width: dim, height: dim, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bgl = build_atlas_bgl(device);
    let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atlas-bg"),
        layout: &bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(icon_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(icon_sampler) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(color_sampler) },
        ],
    });
    (atlas_texture, color_texture, atlas_bind_group)
}

/// the cell render pipeline (shader + layout + premultiplied-alpha blend),
/// shared by Renderer::new and the headless pipeline-validation test so the
/// test exercises the real layout-vs-shader binding match
fn build_cell_pipeline(
    device: &wgpu::Device,
    uniform_bgl: &wgpu::BindGroupLayout,
    atlas_bgl: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cell-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pipeline-layout"),
        bind_group_layouts: &[Some(uniform_bgl), Some(atlas_bgl)],
        immediate_size: 0,
    });
    // premultiplied-alpha over operator
    let blend = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("cell-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &INSTANCE_ATTRS,
            })],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// label the resolved gpu backend for the settings ABOUT block
fn backend_label(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::Dx12 => "wgpu / DX12",
        wgpu::Backend::Vulkan => "wgpu / Vulkan",
        wgpu::Backend::Gl => "wgpu / GL",
        wgpu::Backend::Metal => "wgpu / Metal",
        wgpu::Backend::BrowserWebGpu => "wgpu / WebGPU",
        _ => "wgpu",
    }
}

/// the complete set of device-owned gpu handles. bundled so the windowed init
/// (from_parts) and a device-loss recreate rebuild exactly the same set with
/// nothing left dangling
struct GpuResources {
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    atlas_texture: wgpu::Texture,
    color_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    icon_texture: wgpu::Texture,
    sampler: wgpu::Sampler,
    icon_view: wgpu::TextureView,
    icon_sampler: wgpu::Sampler,
    color_sampler: wgpu::Sampler,
}

/// build every device-owned gpu handle from a device + queue. atlas/color
/// textures are sized to `atlas_dim`; the cpu glyph bitmaps upload separately
/// via upload_atlas (the caller flags the atlas dirty after a rebuild)
fn build_gpu_resources(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas_dim: u32,
    format: wgpu::TextureFormat,
) -> GpuResources {
    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let uniform_bgl = build_uniform_bgl(device);
    let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("uniform-bg"),
        layout: &uniform_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        }],
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("atlas-sampler"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });

    // the app icon (">_<" master, pre-decoded to 128x128 RGBA) lives in a small
    // color texture drawn as a title-bar badge; a full mip chain keeps the ~20px
    // downscale crisp (a single level sampled 6x down looks fuzzy/aliased)
    const ICON_DIM: u32 = 128;
    let icon_rgba: &[u8] = include_bytes!("../../assets/icon_128.rgba");
    let icon_mips = build_mips(icon_rgba, ICON_DIM);
    let icon_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("app-icon"),
        size: wgpu::Extent3d { width: ICON_DIM, height: ICON_DIM, depth_or_array_layers: 1 },
        mip_level_count: icon_mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, (dim, data)) in icon_mips.iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &icon_texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(dim * 4),
                rows_per_image: Some(*dim),
            },
            wgpu::Extent3d { width: *dim, height: *dim, depth_or_array_layers: 1 },
        );
    }
    let icon_view = icon_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let icon_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("icon-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });

    let color_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("color-atlas-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let (atlas_texture, color_texture, atlas_bind_group) =
        make_atlas_bind_group(device, atlas_dim, &sampler, &icon_view, &icon_sampler, &color_sampler);

    let atlas_bgl = build_atlas_bgl(device);
    let pipeline = build_cell_pipeline(device, &uniform_bgl, &atlas_bgl, format);

    let instance_capacity = 8192u64;
    let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instances"),
        size: instance_capacity * std::mem::size_of::<Instance>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    GpuResources {
        uniform_buffer,
        uniform_bind_group,
        pipeline,
        instance_buffer,
        instance_capacity,
        atlas_texture,
        color_texture,
        atlas_bind_group,
        icon_texture,
        sampler,
        icon_view,
        icon_sampler,
        color_sampler,
    }
}

impl Renderer {
    pub fn new(
        window: Arc<Window>,
        display_handle: winit::event_loop::OwnedDisplayHandle,
        content_pt: f32,
        chrome_pt: f32,
        backend: BackendChoice,
        _satellite: bool,
    ) -> Result<Renderer> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        // build the bundled-font glyph atlas on a worker thread so its font load
        // overlaps the gpu adapter/device request below (mostly driver wait)
        // instead of running sequentially after it — shaves startup latency
        let atlas_handle = std::thread::spawn(move || GlyphAtlas::new(content_pt, chrome_pt, scale, None, 1.32));

        // build instance+surface+adapter for a backend set; DX12 is the Windows
        // default (Vulkan is slow under injected overlay layers — OBS/Overwolf)
        let try_init = |backends: wgpu::Backends, force_fallback: bool| -> Result<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
            let mut desc =
                wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display_handle.clone()));
            desc.backends = backends;
            #[cfg(target_os = "linux")]
            if _satellite && std::env::var_os("WGPU_VALIDATION").is_none() {
                // vvl and nvidia can crash while a second instance destroys its swapchain
                desc.flags.remove(wgpu::InstanceFlags::VALIDATION);
            }
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone())?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                // a 2D terminal doesn't need the discrete GPU; low-power picks the
                // integrated adapter, which inits faster and saves battery
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: force_fallback,
                apply_limit_buckets: false,
            }))
            .map_err(|e| anyhow!("no suitable GPU adapter: {e}"))?;
            Ok((instance, surface, adapter))
        };

        // try the chosen backend; if it has no adapter, fall back to all backends
        // so a bad choice can never prevent launch
        let chosen = backend.to_backends();
        let fallback = if cfg!(windows) {
            wgpu::Backends::DX12 | wgpu::Backends::VULKAN | wgpu::Backends::GL
        } else {
            wgpu::Backends::all()
        };
        let (_instance, surface, adapter) = match try_init(chosen, false) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("backend {chosen:?} unavailable ({e:#}); falling back");
                match try_init(fallback, false) {
                    Ok(t) => t,
                    Err(e2) => {
                        // last resort: a software/WARP adapter so termie still
                        // launches on a broken/updating driver, an RDP session, or
                        // a locked-down VM — degraded but running beats not at all
                        log::warn!("no hardware GPU adapter ({e2:#}); using software fallback");
                        try_init(fallback, true)?
                    }
                }
            }
        };
        crate::timing("  gpu: adapter+surface");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))?;
        crate::timing("  gpu: device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // transparent compositing if the surface supports premultiplied alpha
        let transparent = caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        let alpha_mode = if transparent {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            wgpu::CompositeAlphaMode::Opaque
        };
        log::info!("surface format={format:?} alpha_mode={alpha_mode:?} transparent={transparent}");

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            // a terminal renders far below a refresh of gpu work, so queue only
            // one frame ahead — cuts up to a frame of input-to-photon latency
            desired_maximum_frame_latency: 1,
            alpha_mode,
            // auto keeps wgpu's historical srgb/linear pick for the format
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        crate::timing("  gpu: surface configured");

        let atlas = atlas_handle.join().expect("atlas build thread panicked");
        crate::timing("  gpu: atlas joined");
        let mut r = Self::from_parts(
            device,
            queue,
            Some(surface),
            format,
            config,
            atlas,
            scale,
            content_pt,
            chrome_pt,
            transparent,
        );
        r.backend_label = backend_label(adapter.get_info().backend);
        r.window = Some(window);
        r.display_handle = Some(display_handle);
        Ok(r)
    }

    /// rebuild the gpu (instance + surface + adapter + device + queue + all
    /// device-owned handles) after a device loss, preserving the cpu-side glyph
    /// atlas and all ui state. a no-op for the headless renderer
    pub fn recreate(&mut self, window: Arc<Window>) -> Result<()> {
        if self.surface.is_none() {
            return Ok(());
        }
        // a surface is bound to its instance and a fresh device can't adopt a
        // stale one, so the whole chain is rebuilt. recovery takes any adapter,
        // including the software/WARP fallback
        let backends = if cfg!(windows) {
            wgpu::Backends::DX12 | wgpu::Backends::VULKAN | wgpu::Backends::GL
        } else {
            wgpu::Backends::all()
        };
        let try_init = |force_fallback: bool| -> Result<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
            let display_handle = self
                .display_handle
                .as_ref()
                .ok_or_else(|| anyhow!("windowed renderer has no display handle"))?;
            let mut desc =
                wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display_handle.clone()));
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone())?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: force_fallback,
                apply_limit_buckets: false,
            }))
            .map_err(|e| anyhow!("no GPU adapter on recreate: {e}"))?;
            Ok((instance, surface, adapter))
        };
        let (_instance, surface, adapter) = match try_init(false) {
            Ok(t) => t,
            Err(_) => try_init(true)?,
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let transparent = caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        let mut config = self.config.clone();
        config.format = format;
        config.alpha_mode = if transparent {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            wgpu::CompositeAlphaMode::Opaque
        };
        surface.configure(&device, &config);

        // re-arm the lost callback on the new device, reusing the same flag
        {
            let flag = self.device_lost.clone();
            device.set_device_lost_callback(move |reason, msg| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    log::error!("gpu device lost: {reason:?}: {msg}");
                    flag.store(true, Ordering::SeqCst);
                }
            });
        }

        let gpu = build_gpu_resources(&device, &queue, self.atlas.dim, format);
        // swap in the new device-owned handles; every cpu field (atlas, palette,
        // tabs, ...) is preserved so the warm glyph cache and ui state survive
        self.surface = Some(surface);
        self.device = device;
        self.queue = queue;
        self.config = config;
        // recovery may have landed on a different backend (incl. software/WARP);
        // keep the settings ABOUT panel honest rather than showing the old one
        self.backend_label = backend_label(adapter.get_info().backend);
        self.transparent = transparent;
        self.bg_alpha = if transparent { self.opacity } else { 1.0 };
        self.uniform_buffer = gpu.uniform_buffer;
        self.uniform_bind_group = gpu.uniform_bind_group;
        self.uploaded_screen = None;
        self.pipeline = gpu.pipeline;
        self.instance_buffer = gpu.instance_buffer;
        self.instance_capacity = gpu.instance_capacity;
        self.atlas_texture = gpu.atlas_texture;
        self.color_texture = gpu.color_texture;
        self.atlas_bind_group = gpu.atlas_bind_group;
        self._icon_texture = gpu.icon_texture;
        self.sampler = gpu.sampler;
        self.icon_view = gpu.icon_view;
        self.icon_sampler = gpu.icon_sampler;
        self.color_sampler = gpu.color_sampler;
        self.atlas_gpu_dim = self.atlas.dim;
        // re-upload the warm cpu glyph bitmaps onto the fresh textures
        self.atlas.dirty = true;
        self.atlas.dirty_y = None;
        self.atlas.color_dirty = true;
        self.atlas.color_dirty_y = None;
        self.device_lost.store(false, Ordering::SeqCst);
        log::info!("gpu device recreated");
        Ok(())
    }

    /// build the renderer from already-created gpu parts. shared by the windowed
    /// `new` and the headless capture constructor; `surface` is None offscreen
    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: Option<wgpu::Surface<'static>>,
        format: wgpu::TextureFormat,
        config: wgpu::SurfaceConfiguration,
        mut atlas: GlyphAtlas,
        scale: f32,
        content_pt: f32,
        chrome_pt: f32,
        transparent: bool,
    ) -> Renderer {
        // the bundled default plus any common monospace families present on the
        // system (initially just the bundled one — system fonts load lazily)
        let fonts = Self::detect_fonts(&mut atlas);

        let GpuResources {
            uniform_buffer,
            uniform_bind_group,
            pipeline,
            instance_buffer,
            instance_capacity,
            atlas_texture,
            color_texture,
            atlas_bind_group,
            icon_texture,
            sampler,
            icon_view,
            icon_sampler,
            color_sampler,
        } = build_gpu_resources(&device, &queue, atlas.dim, format);
        crate::timing("  gpu: resources built");

        let pad = tokens::px(tokens::WINDOW_PAD, scale);
        let title_bar_h = tokens::px(tokens::TITLE_BAR, scale);
        let status_bar_h = tokens::px(tokens::STATUS_BAR, scale);

        // arm device-lost recovery before `device` moves into the struct; the
        // callback fires on a driver reset / TDR (not on our own teardown) and
        // render() rebuilds the gpu on the next frame
        let device_lost = Arc::new(AtomicBool::new(false));
        {
            let flag = device_lost.clone();
            device.set_device_lost_callback(move |reason, msg| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    log::error!("gpu device lost: {reason:?}: {msg}");
                    flag.store(true, Ordering::SeqCst);
                }
            });
        }

        let mut r = Renderer {
            surface,
            offscreen: None,
            device,
            queue,
            config,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            uploaded_screen: None,
            atlas_texture,
            color_texture,
            atlas_bind_group,
            _icon_texture: icon_texture,
            sampler,
            icon_view,
            icon_sampler,
            color_sampler,
            atlas_gpu_dim: atlas.dim,
            device_lost,
            window: None,
            display_handle: None,
            instance_buffer,
            instance_capacity,
            scratch: Vec::new(),
            pane_scratch: Vec::new(),
            atlas,
            palette: Palette::from_theme(ThemeId::Instrument),
            pane_palettes: Vec::new(),
            theme: ThemeId::Instrument,
            color_overrides: Vec::new(),
            broadcast: false,
            gradient_cache: Vec::new(),
            gradient_key: (0, 0, ThemeId::Instrument),
            scale,
            pad,
            content_pt,
            content_line_height: 1.32,
            chrome_pt,
            title_bar_h,
            status_bar_h,
            bg_alpha: if transparent { 0.85 } else { 1.0 },
            transparent,
            opacity: 0.85,
            start: Instant::now(),
            reveal_start: Instant::now(),
            hovered: None,
            hover_since: None,
            hover_prev: None,
            focus_rect: None,
            focus_prev: None,
            tab_signal: Vec::new(),
            size_flash: None,
            menu_hover: None,
            tab_slide: None,
            overlay_since: None,
            overlay_shown: false,
            settings_open: false,
            settings_p: 0.0,
            settings_scroll: 0.0,
            panel_clip: None,
            cursor_style: CursorShape::Block,
            cursor_blink: true,
            bold_as_bright: true,
            pane_pad_px: 6.0,
            content_font: None,
            content_weight: 400,
            min_contrast: 1.0,
            bg_image: None,
            bg_image_alpha: 0.3,
            ligatures: true,
            backend_label: "wgpu",
            fonts,
            font_idx: 0,
            settings_view: SettingsView::default(),
            pane_mode: false,
            pane_drop: None,
            tab_drop: None,
            drag_preview: None,
            mark_mode: false,
            elevated: false,
            tabs: Vec::new(),
            tab_status: Vec::new(),
            tab_colors: Vec::new(),
            active_tab: 0,
            status: StatusView::default(),
            notice: None,
            link_preview: None,
            update_chip: None,
            update_hit: None,
            status_size: (usize::MAX, usize::MAX, String::new()),
            status_cmd: (u64::MAX, String::new()),
            plugins_installed: Vec::new(),
            palette_view: None,
            palette_geom: None,
            find_btns: None,
            pane_menu_view: None,
            find_view: None,
            market_view: None,
            market_hits: Vec::new(),
            confirm_view: None,
            rename_view: None,
            dock: Vec::new(),
            dock_hitboxes: Vec::new(),
            latency_hud: None,
            cols: 0,
            rows: 0,
        };
        r.recompute_grid_size();
        r
    }

    fn recompute_grid_size(&mut self) {
        let m = self.atlas.metrics(FontId::Content);
        let chrome = self.title_bar_h + self.status_bar_h;
        let usable_w = (self.config.width as f32 - self.pad * 2.0).max(m.cell_w);
        let usable_h = (self.config.height as f32 - chrome - self.pad).max(m.cell_h);
        let (cols, rows) = ((usable_w / m.cell_w).floor().max(1.0) as usize, (usable_h / m.cell_h).floor().max(1.0) as usize);
        // the size readout only means something once a first size exists
        if (cols, rows) != (self.cols, self.rows) && self.cols > 0 {
            self.size_flash = Some(Instant::now());
        }
        self.cols = cols;
        self.rows = rows;
    }

    pub fn resize(&mut self, width: u32, height: u32) -> (usize, usize) {
        if width == 0 || height == 0 {
            return (self.cols, self.rows);
        }
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
        self.recompute_grid_size();
        // grow the GPU instance buffer eagerly here (off the render hot path) so
        // the first paint after a resize never reallocates mid-frame
        let needed = (self.cols * self.rows) as u64 + 1024;
        if needed > self.instance_capacity {
            self.instance_capacity = needed.next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        (self.cols, self.rows)
    }

    /// the pixel rect available for terminal panes (between the bars). the
    /// plugin dock, when present, carves its width off the right so panes reflow
    pub fn palette(&self) -> &crate::color::Palette {
        &self.palette
    }

    pub fn content_rect(&self) -> (f32, f32, f32, f32) {
        // symmetric inset: the same `pad` on top as the sides and bottom, so a
        // program drawing on row 0 (a TUI banner, a mascot) isn't jammed against
        // the title bar's bottom trim
        let x = self.pad;
        let y = self.title_bar_h + self.pad;
        let w = (self.config.width as f32 - self.pad * 2.0 - self.dock_w()).max(1.0);
        let h = (self.config.height as f32 - self.title_bar_h - self.status_bar_h - self.pad * 2.0)
            .max(1.0);
        (x, y, w, h)
    }

    /// width reserved for the right-side plugin dock (0 when no widgets). capped
    /// so the dock can never crowd out the terminal on a narrow window
    fn dock_w(&self) -> f32 {
        if self.dock.is_empty() {
            0.0
        } else {
            (224.0 * self.scale).round().min(self.config.width as f32 * 0.4)
        }
    }

    /// set the latency hud line (None hides it)
    pub fn set_latency_hud(&mut self, line: Option<String>) {
        self.latency_hud = line;
    }

    /// replace the dock's widget list. returns true if the dock's presence
    /// toggled (empty<->non-empty), since that changes content_rect and the
    /// caller must relayout panes
    pub fn set_dock(&mut self, widgets: Vec<DockWidget>) -> bool {
        let was = !self.dock.is_empty();
        self.dock = widgets;
        // draw_dock repopulates these each frame, but an emptied dock never
        // draws, so drop the stale bands now
        if self.dock.is_empty() {
            self.dock_hitboxes.clear();
        }
        was == self.dock.is_empty()
    }

    /// dock index of the widget whose clickable band contains (px, py), if any.
    /// the index is parallel to the widget set passed to set_dock, so the caller
    /// can map it back to the owning plugin
    pub fn widget_at(&self, px: f32, py: f32) -> Option<usize> {
        self.dock_hitboxes
            .iter()
            .position(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
    }

    /// inner padding inside each pane rect (keeps text off the dividers)
    fn pane_pad(&self) -> f32 {
        (self.pane_pad_px * self.scale).round()
    }

    /// given a pane's pixel rect, the grid origin + cols/rows that fit inside it
    /// physical-pixel rect (x, y, w, h) of a cell within a pane, for parking the
    /// IME candidate window at the cursor
    pub fn cell_screen_rect(&self, rect: (f32, f32, f32, f32), row: usize, col: usize) -> (f64, f64, f64, f64) {
        let (ox, oy, _, _) = self.pane_metrics(rect);
        let m = self.atlas.metrics(FontId::Content);
        (
            (ox + col as f32 * m.cell_w) as f64,
            (oy + row as f32 * m.cell_h) as f64,
            m.cell_w as f64,
            m.cell_h as f64,
        )
    }

    pub fn pane_metrics(&self, rect: (f32, f32, f32, f32)) -> (f32, f32, usize, usize) {
        let m = self.atlas.metrics(FontId::Content);
        let p = self.pane_pad();
        let ox = (rect.0 + p).round();
        let oy = (rect.1 + p).round();
        let cols = (((rect.2 - p * 2.0) / m.cell_w).floor()).max(1.0) as usize;
        let rows = (((rect.3 - p * 2.0) / m.cell_h).floor()).max(1.0) as usize;
        (ox, oy, cols, rows)
    }

    /// content cell size in physical pixels, fed to each terminal so the
    /// XTWINOPS pixel reports (CSI 14/16 t) can answer honestly
    pub fn cell_px(&self) -> (u16, u16) {
        let m = self.atlas.metrics(FontId::Content);
        (m.cell_w.round().max(1.0) as u16, m.cell_h.round().max(1.0) as u16)
    }

    /// the (col, row) cell at a pixel position within a pane rect, clamped
    pub fn cell_at(&self, rect: (f32, f32, f32, f32), x: f32, y: f32) -> (usize, usize) {
        let m = self.atlas.metrics(FontId::Content);
        let (ox, oy, cols, rows) = self.pane_metrics(rect);
        let col = (((x - ox) / m.cell_w).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let row = (((y - oy) / m.cell_h).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        (col, row)
    }

    /// cursor-beam / scroll-thumb width in pixels (kept in one place so the
    /// painter and the scrollbar hit-test agree)
    fn beam_w(&self) -> f32 {
        (2.0 * self.scale).round().max(1.0)
    }

    /// scroll thumb geometry for a pane grid, or None when there is nothing to
    /// scroll. `ox`/`oy` are the pane's content origin (from pane_metrics)
    #[allow(clippy::too_many_arguments)]
    fn scrollbar_geom(
        ox: f32,
        oy: f32,
        cols: usize,
        rows: usize,
        cell_w: f32,
        cell_h: f32,
        beam_w: f32,
        scrollback_len: usize,
        view_offset: usize,
    ) -> Option<ScrollThumb> {
        let total = scrollback_len + rows;
        if total <= rows {
            return None;
        }
        let track_h = rows as f32 * cell_h;
        let track_w = (2.0 * beam_w).max(2.0);
        // keep the thumb in the pane's right padding, after the final terminal
        // cell. TUIs repaint that cell constantly, so an overlaid thumb visibly
        // fights their bottom-right status and cursor area
        let track_x = ox + cols as f32 * cell_w;
        let thumb_h = (track_h * rows as f32 / total as f32).max(cell_h);
        let top_line = (total - rows - view_offset) as f32;
        let thumb_y = oy + (track_h - thumb_h) * (top_line / (total - rows) as f32);
        Some(ScrollThumb { track_x, track_y: oy, track_w, track_h, thumb_y, thumb_h })
    }

    /// public scroll-thumb geometry for the pane at `rect` given its grid's
    /// history depth and current scroll offset (used by the input hit-test)
    pub fn scrollbar_for(
        &self,
        rect: (f32, f32, f32, f32),
        scrollback_len: usize,
        view_offset: usize,
    ) -> Option<ScrollThumb> {
        let m = self.atlas.metrics(FontId::Content);
        let (ox, oy, cols, rows) = self.pane_metrics(rect);
        Self::scrollbar_geom(ox, oy, cols, rows, m.cell_w, m.cell_h, self.beam_w(), scrollback_len, view_offset)
    }

    /// map a window-y to the view_offset that puts the thumb top there, for a
    /// pane at `rect` with `scrollback_len` lines of history. clamps to range
    pub fn scroll_offset_at(
        &self,
        rect: (f32, f32, f32, f32),
        scrollback_len: usize,
        thumb_top_y: f32,
    ) -> usize {
        let (_, _, _, rows) = self.pane_metrics(rect);
        // geom at offset 0 gives the track/thumb size (independent of offset)
        let Some(g) = self.scrollbar_for(rect, scrollback_len, 0) else {
            return 0;
        };
        let span = (g.track_h - g.thumb_h).max(1.0);
        let frac = ((thumb_top_y - g.track_y) / span).clamp(0.0, 1.0);
        let scroll_span = scrollback_len; // total - rows == scrollback_len
        let top_line = (frac * scroll_span as f32).round() as usize;
        let _ = rows;
        scroll_span.saturating_sub(top_line)
    }

    pub fn set_hovered(&mut self, h: Option<Hot>) -> bool {
        let changed = self.hovered != h;
        if changed {
            let now = Instant::now();
            // the target being left fades out from wherever its fade-in got to
            self.hover_prev = self.hovered.map(|old| {
                let shown = self.hover_since.map_or(1.0, |t| motion::progress(t, motion::HOVER_IN));
                let skip = (1.0 - shown) * motion::HOVER_OUT;
                (old, now.checked_sub(std::time::Duration::from_secs_f32(skip)).unwrap_or(now))
            });
            self.hover_since = h.is_some().then_some(now);
        }
        self.hovered = h;
        changed
    }

    /// hover strength (0..1) of one chrome target: eased in while hovered,
    /// eased out over a longer tail once the pointer leaves
    fn hover_a(&self, hot: Hot) -> f32 {
        if self.hovered == Some(hot) {
            return self.hover_since.map_or(1.0, |t| motion::ease_out(motion::progress(t, motion::HOVER_IN)));
        }
        match self.hover_prev {
            Some((prev, t)) if prev == hot => 1.0 - motion::ease_out(motion::progress(t, motion::HOVER_OUT)),
            _ => 0.0,
        }
    }

    fn hover_animating(&self) -> bool {
        self.hover_since.is_some_and(|t| t.elapsed().as_secs_f32() < motion::HOVER_IN)
            || self.hover_prev.is_some_and(|(_, t)| t.elapsed().as_secs_f32() < motion::HOVER_OUT)
    }

    pub fn set_pane_mode(&mut self, on: bool) {
        self.pane_mode = on;
    }

    pub fn set_pane_drop(&mut self, drop: Option<((f32, f32, f32, f32), PaneDropSide)>) {
        self.pane_drop = drop;
    }

    pub fn set_tab_drop(&mut self, index: Option<usize>) {
        self.tab_drop = index;
    }

    pub fn set_drag_preview(&mut self, preview: Option<(f32, f32, String, &'static str)>) {
        self.drag_preview = preview;
    }

    pub fn set_elevated(&mut self, on: bool) {
        self.elevated = on;
    }

    pub fn set_mark_mode(&mut self, on: bool) {
        self.mark_mode = on;
    }

    pub fn set_broadcast(&mut self, on: bool) {
        self.broadcast = on;
    }

    pub fn cycle_cursor(&mut self) {
        self.cursor_style = match self.cursor_style {
            CursorShape::Bar => CursorShape::Block,
            CursorShape::Block => CursorShape::Underline,
            CursorShape::Underline => CursorShape::Bar,
        };
    }

    pub fn cursor_style_name(&self) -> &'static str {
        match self.cursor_style {
            CursorShape::Bar => "beam",
            CursorShape::Block => "block",
            CursorShape::Underline => "underline",
        }
    }

    pub fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        self.palette = self.themed_palette();
        self.atlas.dirty = true;
    }

    fn themed_palette(&self) -> Palette {
        let mut p = Palette::from_theme(self.theme);
        p.apply_overrides(&self.color_overrides);
        p
    }

    /// install user color overrides (from disk) and rebuild the active palette
    pub fn set_color_overrides(&mut self, overrides: Vec<(String, Rgb)>) {
        self.color_overrides = overrides;
        self.palette = self.themed_palette();
        // per-profile palettes bake the overrides in too — rebuild on demand
        self.pane_palettes.clear();
        // overrides can arrive live (conf watcher), not just at boot: drop the
        // cached bg wash too — its (size, theme) key can't see a bg override
        self.atlas.dirty = true;
        self.gradient_cache.clear();
    }

    pub fn set_theme(&mut self, id: ThemeId) {
        if self.theme != id {
            self.theme = id;
            self.palette = self.themed_palette();
            self.atlas.dirty = true;
        }
    }

    pub fn toggle_cursor_blink(&mut self) {
        self.cursor_blink = !self.cursor_blink;
    }

    /// nudge the inner pane padding (px, pre-scale); returns true if it changed
    pub fn set_pane_pad(&mut self, delta: f32) -> bool {
        let next = (self.pane_pad_px + delta).clamp(0.0, 20.0);
        let changed = next != self.pane_pad_px;
        self.pane_pad_px = next;
        changed
    }

    pub fn set_settings(&mut self, v: SettingsView) {
        self.settings_view = v;
    }

    /// the installed plugins (display name, enabled) shown in the settings panel
    pub fn set_plugins(&mut self, list: Vec<(String, bool)>) {
        self.plugins_installed = list;
    }

    /// drive the slide-in panel: `open` = interactive, `p` = docked fraction (0..1)
    pub fn set_settings_panel(&mut self, open: bool, p: f32) {
        self.settings_open = open;
        self.settings_p = p;
    }

    pub fn reset_settings_scroll(&mut self) {
        self.settings_scroll = 0.0;
    }

    pub fn scroll_settings(&mut self, delta: f32) {
        let g = self.settings_geom();
        let max = (g.content_h - (g.body_bottom - g.body_top)).max(0.0);
        self.settings_scroll = (self.settings_scroll + delta).clamp(0.0, max);
    }

    pub fn in_settings_panel(&self, x: f32, y: f32) -> bool {
        let g = self.settings_geom();
        x >= g.panel_x && x < g.panel_x + g.panel_w && y >= g.panel_top && y < g.panel_top + g.panel_h
    }

    pub fn content_pt(&self) -> f32 {
        self.content_pt
    }

    /// re-measure the glyph atlas at a new content point size; returns new (cols, rows)
    pub fn set_content_pt(&mut self, pt: f32) -> (usize, usize) {
        let pt = pt.clamp(8.0, 32.0);
        // re-rasterizing the atlas is the expensive part (two cosmic-text shape
        // passes + clearing ~5MB of atlas buffers); skip it when the size is
        // unchanged — notably at boot, where the worker already built the atlas
        // at exactly this size. still recompute the grid (cols/rows start at 0)
        if pt != self.content_pt {
            self.content_pt = pt;
            self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height, self.content_weight);
        }
        self.recompute_grid_size();
        (self.cols, self.rows)
    }

    /// re-raster the atlas and recompute chrome/grid metrics for a new device
    /// scale (per-monitor dpi change), so a window dragged between monitors of
    /// different dpi stays crisp; mirrors the scale-derived geometry in from_parts
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        self.scale = scale;
        // re-raster glyphs at the new device scale (clears + repacks the atlas)
        self.atlas.reconfigure(self.content_pt, self.chrome_pt, scale, self.content_font, self.content_line_height, self.content_weight);
        self.pad = tokens::px(tokens::WINDOW_PAD, scale);
        self.title_bar_h = tokens::px(tokens::TITLE_BAR, scale);
        self.status_bar_h = tokens::px(tokens::STATUS_BAR, scale);
        self.recompute_grid_size();
    }

    pub fn pane_pad_px(&self) -> f32 {
        self.pane_pad_px
    }

    pub fn set_pane_pad_px(&mut self, v: f32) {
        self.pane_pad_px = v.clamp(0.0, 20.0);
    }

    /// window opacity as a percentage (50..100) for the settings UI + persistence
    pub fn opacity_pct(&self) -> i32 {
        (self.opacity * 100.0).round() as i32
    }

    /// set window opacity from a percentage; only takes visible effect when the
    /// surface supports translucency (otherwise the window stays opaque)
    pub fn set_opacity_pct(&mut self, pct: i32) {
        self.opacity = (pct as f32 / 100.0).clamp(0.5, 1.0);
        self.bg_alpha = if self.transparent { self.opacity } else { 1.0 };
    }

    /// nudge opacity by a percentage delta; returns true if it changed
    pub fn nudge_opacity(&mut self, d: i32) -> bool {
        let before = self.opacity_pct();
        self.set_opacity_pct(before + d);
        self.opacity_pct() != before
    }

    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    pub fn set_cursor_blink(&mut self, on: bool) {
        self.cursor_blink = on;
    }

    pub fn set_bold_as_bright(&mut self, on: bool) {
        self.bold_as_bright = on;
    }

    /// minimum WCAG contrast ratio text is lifted to against its cell bg;
    /// 1.0 leaves colors untouched (the default)
    pub fn set_min_contrast(&mut self, ratio: f32) {
        self.min_contrast = ratio.clamp(1.0, 21.0);
    }

    pub fn min_contrast(&self) -> f32 {
        self.min_contrast
    }

    pub fn set_ligatures(&mut self, on: bool) {
        self.ligatures = on;
    }

    pub fn ligatures(&self) -> bool {
        self.ligatures
    }

    /// set (or clear) the decoded window background image; opacity in 0..1
    pub fn set_background_image(&mut self, img: Option<(Vec<u8>, u32, u32)>, opacity: f32) {
        self.bg_image = img.map(|(rgba, w, h)| (crate::image::alloc_key(), rgba, w, h));
        self.bg_image_alpha = opacity.clamp(0.0, 1.0);
    }

    pub fn bold_as_bright(&self) -> bool {
        self.bold_as_bright
    }

    /// set the content line-height multiplier; re-rasters the atlas and recomputes
    /// the grid since cell height (and thus the row count) changes
    pub fn set_line_height(&mut self, lh: f32) {
        let lh = lh.clamp(0.8, 3.0);
        if (lh - self.content_line_height).abs() < f32::EPSILON {
            return;
        }
        self.content_line_height = lh;
        self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, lh, self.content_weight);
        self.recompute_grid_size();
    }

    pub fn line_height(&self) -> f32 {
        self.content_line_height
    }

    /// set the base font weight regular text shapes at (100-900); re-rasters
    /// the atlas when it changes. returns new (cols, rows) like set_content_pt
    pub fn set_font_weight(&mut self, w: u16) -> (usize, usize) {
        let w = w.clamp(100, 900);
        if w != self.content_weight {
            self.content_weight = w;
            self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height, w);
        }
        self.recompute_grid_size();
        (self.cols, self.rows)
    }

    pub fn font_weight(&self) -> u16 {
        self.content_weight
    }

    pub fn set_cursor_style(&mut self, s: CursorShape) {
        self.cursor_style = s;
    }

    pub fn theme(&self) -> ThemeId {
        self.theme
    }

    pub fn font_name(&self) -> &'static str {
        self.fonts[self.font_idx]
    }

    /// every installed monospace family, for the font picker (empty until the
    /// system-font scan has run)
    pub fn monospace_families(&mut self) -> Vec<String> {
        self.atlas.monospace_families()
    }

    /// the bundled default plus any common monospace families present in the db
    fn detect_fonts(atlas: &mut GlyphAtlas) -> Vec<&'static str> {
        let mut fonts: Vec<&'static str> = vec![atlas.content_family()];
        for cand in [
            "Cascadia Code",
            "Cascadia Mono",
            "JetBrains Mono",
            "Consolas",
            "Lucida Console",
            "Courier New",
        ] {
            if !fonts.iter().any(|f| f.eq_ignore_ascii_case(cand))
                && atlas.has_family(cand)
                && atlas.terminal_ready_family(cand)
            {
                fonts.push(cand);
            }
        }
        fonts
    }

    /// rasterize printable ASCII into the atlas ahead of first content paint
    /// (deferred off the critical startup path) so shell output renders from a
    /// warm cache. the changed atlas rows upload on the next render
    pub fn prewarm_glyphs(&mut self) {
        self.atlas.prewarm_ascii();
    }

    /// scan system fonts once (deferred off startup) so the font picker can
    /// offer them and so non-Latin glyphs have fallbacks. cheap no-op after the
    /// first call. returns true if it scanned now
    pub fn ensure_system_fonts(&mut self) -> bool {
        if !self.atlas.load_system_fonts() {
            return false;
        }
        // any glyph cached as missing before the scan can now resolve via a
        // newly loaded fallback font, so drop those tofu entries
        self.atlas.invalidate_missing();
        let cur = self.fonts[self.font_idx];
        self.fonts = Self::detect_fonts(&mut self.atlas);
        self.font_idx = self.fonts.iter().position(|f| *f == cur).unwrap_or(0);
        true
    }

    /// switch to a content font by family name. resolves against the known list
    /// first; if the name isn't a built-in candidate but the family is actually
    /// installed, inject it so any user-configured font resolves (not just the
    /// six hardcoded ones). the leak is bounded: font switches are rare and only
    /// a handful of distinct families are ever set in a session
    pub fn set_font_by_name(&mut self, name: &str) -> (usize, usize) {
        let idx = match self.fonts.iter().position(|f| f.eq_ignore_ascii_case(name)) {
            Some(i) => Some(i),
            None if self.atlas.has_family(name) => {
                let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
                self.fonts.push(leaked);
                Some(self.fonts.len() - 1)
            }
            None => None,
        };
        if let Some(i) = idx
            && i != self.font_idx {
                self.font_idx = i;
                self.content_font = if i == 0 { None } else { Some(self.fonts[i]) };
                self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height, self.content_weight);
                self.recompute_grid_size();
            }
        (self.cols, self.rows)
    }

    /// per-tab badge severities, parallel to `tabs`; a background tab with a
    /// bell / running / finished command shows a colored dot where its close
    /// icon would sit (0 none, 1 running, 2 done, 3 bell, 4 failed)
    pub fn set_tab_status(&mut self, status: Vec<u8>) {
        if self.tab_signal.len() != status.len() {
            self.tab_signal = vec![None; status.len()];
        }
        // a background tab's command just finished: its rail draws in
        if self.tab_status.len() == status.len() {
            for (i, &s) in status.iter().enumerate() {
                if (s == 2 || s == 4) && self.tab_status[i] != s && i != self.active_tab {
                    self.tab_signal[i] = Some(Instant::now());
                }
            }
        }
        self.tab_status = status;
    }

    /// per-tab user tints, parallel to `tabs` (ansi palette indices)
    pub fn set_tab_colors(&mut self, colors: Vec<Option<u8>>) {
        self.tab_colors = colors;
    }

    /// transient status-bar notification text (None clears the readout)
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    pub fn set_link_preview(&mut self, target: Option<&str>) {
        self.link_preview = target.map(LinkPreview::new);
    }

    /// a pending update's version for the status-bar chip (None clears it)
    pub fn set_update(&mut self, version: Option<String>) {
        self.update_chip = version;
    }

    /// whether a point is on the UPDATE chip (click = install flow)
    pub fn update_chip_at(&self, x: f32, y: f32) -> bool {
        self.update_hit.map(|r| in_rect(x, y, r)).unwrap_or(false)
    }

    pub fn set_tabs(&mut self, tabs: Vec<String>, active: usize) {
        if tabs.len() != self.tabs.len() {
            // a tab opened/closed — every rect shifts, so don't slide across it
            self.tab_slide = None;
            self.tab_signal = vec![None; tabs.len()];
        } else if active != self.active_tab && !self.tabs.is_empty() {
            // a pure switch: slide the accent rail from the old tab to the new
            self.tab_slide = Some((self.active_tab, Instant::now()));
        }
        self.tabs = tabs;
        self.active_tab = active;
    }

    /// spring progress of the active-tab rail slide, with the source index;
    /// None once settled (the rail just sits on the active tab)
    fn tab_slide_p(&self) -> Option<(usize, f32)> {
        let (old, t) = self.tab_slide?;
        let p = motion::spring(t);
        (p < 1.0).then_some((old, p))
    }

    /// a background tab whose command just finished: (rail width 0..1, rail
    /// alpha, dot alpha) as the rail draws in, holds, then settles into the dot
    fn tab_signal_phase(&self, i: usize) -> Option<(f32, f32, f32)> {
        let t = self.tab_signal.get(i).copied().flatten()?.elapsed().as_secs_f32();
        let held = motion::SIGNAL_DRAW + motion::SIGNAL_HOLD;
        if t < motion::SIGNAL_DRAW {
            Some((motion::travel(t / motion::SIGNAL_DRAW), 1.0, 0.0))
        } else if t < held {
            Some((1.0, 1.0, 0.0))
        } else if t < held + motion::SIGNAL_SETTLE {
            let q = motion::ease_out((t - held) / motion::SIGNAL_SETTLE);
            Some((1.0, 1.0 - q, q))
        } else {
            None
        }
    }

    /// any renderer-owned animation is in flight, so the caller keeps a ~60fps
    /// tick alive until everything settles
    pub fn animating(&self) -> bool {
        let live = |since: Option<Instant>, dur: f32| since.is_some_and(|t| t.elapsed().as_secs_f32() < dur);
        self.startup_fading()
            || self.hover_animating()
            || live(self.tab_slide.map(|(_, t)| t), motion::TAB_RAIL)
            || (self.overlay_shown && live(self.overlay_since, motion::OVERLAY))
            || live(self.focus_prev.map(|(_, t)| t), motion::FOCUS)
            || live(self.size_flash, 1.0)
            || (self.pane_menu_view.is_some() && live(self.menu_hover.map(|(_, t)| t), motion::HOVER_IN))
            || self
                .tab_signal
                .iter()
                .any(|s| live(*s, motion::SIGNAL_DRAW + motion::SIGNAL_HOLD + motion::SIGNAL_SETTLE))
    }

    /// slow ambient wakeups the idle loop still owes: the running dot's breath
    /// and the command timer. None when nothing on screen moves on its own
    pub fn ambient_wake_ms(&self) -> Option<u64> {
        let running = self.tab_status.contains(&1) || matches!(self.status.command, CommandState::Running(_));
        if !running {
            None
        } else if motion::reduced() {
            Some(1000)
        } else {
            Some(50)
        }
    }

    /// the soft cursor blink only needs frames while it fades; between fades
    /// it can sleep until the next one starts
    pub fn cursor_wake_ms(&self) -> u64 {
        let p = (self.start.elapsed().as_secs_f32() % motion::CURSOR_PERIOD) / motion::CURSOR_PERIOD;
        let fading = (0.50..0.62).contains(&p) || p >= 0.88;
        if fading && !motion::reduced() {
            16
        } else {
            let next = [0.50, 0.88, 1.50].into_iter().find(|&edge| edge > p).unwrap_or(1.50);
            (((next - p) * motion::CURSOR_PERIOD * 1000.0) as u64).max(1)
        }
    }

    pub fn set_status(&mut self, status: StatusView) {
        self.status = status;
    }

    pub fn set_palette(&mut self, p: Option<PaletteView>) {
        self.palette_view = p;
    }

    /// mark the overlay as already shown so the next build skips the bloom-in and
    /// renders it at full opacity — used by the headless capture harness so an
    /// overlay scene isn't caught mid-fade
    #[cfg(any(test, debug_assertions))]
    pub fn settle_overlay(&mut self) {
        self.overlay_shown = true;
        let past = Instant::now().checked_sub(std::time::Duration::from_secs(1)).unwrap_or_else(Instant::now);
        self.menu_hover = self.pane_menu_view.as_ref().and_then(|m| m.hovered).map(|i| (i, past));
    }

    pub fn set_pane_menu(&mut self, m: Option<PaneMenuView>) {
        self.pane_menu_view = m;
    }

    /// clamped (x, y, width, row_h, pad) of the context menu, shared by the
    /// renderer and the hit-test so the two never drift
    fn pane_menu_geom(&self, mx: f32, my: f32, items: &[String], swatches: bool) -> (f32, f32, f32, f32, f32) {
        let t = |pt| tokens::px(pt, self.scale);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let row_h = chrome_h + t(tokens::SPACE_M);
        let pad = t(tokens::SPACE_XS) + t(tokens::SPACE_2XS);
        let track = self.track_for(FontId::Chrome);
        let widest = items.iter().map(|i| self.text_w(FontId::Chrome, i, track)).fold(0.0, f32::max);
        let swatch = if swatches { t(tokens::SPACE_M) + t(tokens::SPACE_S) } else { 0.0 };
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let mw = (widest + swatch + t(tokens::SPACE_M) * 2.0 + pad * 2.0).clamp(t(tokens::MENU_W), t(360.0).min(w - t(tokens::SPACE_S) * 2.0)).round();
        let mh = row_h * items.len() as f32 + pad * 2.0;
        let edge = t(tokens::SPACE_XS);
        let bx = mx.min(w - mw - edge).max(edge).round();
        let by = my.min(h - mh - edge).max(self.title_bar_h).round();
        (bx, by, mw, row_h, pad)
    }

    /// the menu item under (px, py), or None if outside the menu's rows
    pub fn pane_menu_item_at(&self, px: f32, py: f32) -> Option<usize> {
        let v = self.pane_menu_view.as_ref()?;
        let rows = v.items.len();
        let (bx, by, mw, row_h, pad) = self.pane_menu_geom(v.x, v.y, &v.items, !v.accents.is_empty());
        if px < bx || px >= bx + mw || py < by + pad || py >= by + pad + row_h * rows as f32 {
            return None;
        }
        Some((((py - by - pad) / row_h) as usize).min(rows - 1))
    }

    pub fn set_market(&mut self, m: Option<MarketView>) {
        // a closed market draws nothing, so drop its stale clickable regions now
        if m.is_none() {
            self.market_hits.clear();
        }
        self.market_view = m;
    }

    /// what a click at (x, y) lands on in the open market overlay, if anything
    pub fn market_hit_at(&self, x: f32, y: f32) -> Option<MarketHit> {
        self.market_hits
            .iter()
            .find(|((rx, ry, rw, rh), _)| x >= *rx && x < *rx + *rw && y >= *ry && y < *ry + *rh)
            .map(|(_, h)| *h)
    }

    pub fn set_find(&mut self, f: Option<FindView>) {
        self.find_view = f;
    }

    pub fn set_confirm(&mut self, c: Option<ConfirmView>) {
        self.confirm_view = c;
    }

    pub fn set_rename(&mut self, r: Option<RenameView>) {
        self.rename_view = r;
    }

    /// title-bar buttons, left→right: splitV, splitH, pane mode, gear, minimize, maximize, close
    fn control_rects(&self) -> [(Hot, f32, f32); 7] {
        let cw = tokens::px(tokens::CONTROL_W, self.scale);
        let w = self.config.width as f32;
        [
            (Hot::SplitV, w - cw * 7.0, w - cw * 6.0),
            (Hot::SplitH, w - cw * 6.0, w - cw * 5.0),
            (Hot::PaneMode, w - cw * 5.0, w - cw * 4.0),
            (Hot::Gear, w - cw * 4.0, w - cw * 3.0),
            (Hot::Minimize, w - cw * 3.0, w - cw * 2.0),
            (Hot::Maximize, w - cw * 2.0, w - cw),
            (Hot::Close, w - cw, w),
        ]
    }

    /// chrome text tracking for a font: a hair of air between monospace cells
    fn track_for(&self, font: FontId) -> f32 {
        (0.04 * self.atlas.metrics(font).cell_w).max(0.5)
    }

    /// wide tracking for small-caps key labels (status bar keys, section heads)
    fn key_track(&self) -> f32 {
        (tokens::KEY_TRACK_EM * self.atlas.metrics(FontId::Small).px).max(1.0)
    }

    fn badge_size(&self) -> f32 {
        tokens::px(20.0, self.scale)
    }

    /// where the wordmark ends and tabs begin
    fn tabs_start_x(&self) -> f32 {
        let t = |pt| tokens::px(pt, self.scale);
        self.pad
            + self.badge_size()
            + t(tokens::SPACE_S)
            + self.text_w(FontId::Chrome, "termie", self.track_for(FontId::Chrome))
            + t(tokens::SPACE_M)
    }

    fn window_tabs_rect(&self) -> Rect {
        (0.0, 0.0, self.pad + self.badge_size() + tokens::px(tokens::SPACE_XS, self.scale), self.title_bar_h)
    }

    fn tab_layout(&self) -> TabLayout {
        let s = self.scale;
        let t = |pt| tokens::px(pt, s);
        let h = self.title_bar_h;
        let cw = t(tokens::CONTROL_W);
        let newtab_w = t(tokens::SPACE_2XL);
        let chev_w = t(20.0);
        let start = self.tabs_start_x();
        // reserve all 7 title-bar control buttons (control_rects starts at w-7cw,
        // the SplitV slot) so the new-tab '+' and its chevron never overrun the
        // split icon
        let controls_start = self.config.width as f32 - cw * 7.0;
        let avail = (controls_start - start - newtab_w - chev_w - t(tokens::SPACE_S)).max(0.0);

        // each tab sizes to its label plus the insets and the close slot,
        // clamped; when the strip overflows the widest tabs give way first, and
        // past that every tab shares the room evenly (labels truncate) so all
        // stay visible and clickable
        let cc = t(tokens::SPACE_L);
        let track = self.track_for(FontId::Chrome);
        let natural: Vec<f32> = self
            .tabs
            .iter()
            .map(|label| {
                let label_w = self.text_w(FontId::Chrome, label, track);
                (t(tokens::TAB_INSET) + label_w + t(tokens::SPACE_XS) + cc + t(tokens::SPACE_S))
                    .clamp(t(tokens::TAB_MIN), t(tokens::TAB_MAX))
            })
            .collect();
        let widths = fit_widths(&natural, avail);
        let mut tabs = Vec::new();
        let mut x = start;
        for (i, &tab_w) in widths.iter().enumerate() {
            // below this the close target would eat the whole tab; drop it and
            // give the label the full width (middle-click still closes)
            let c = if tab_w >= cc * 2.5 { cc } else { 0.0 };
            let rect = (x.round(), 0.0, tab_w.round(), h);
            let close = ((x + tab_w - c - t(tokens::SPACE_S)).round(), ((h - c) / 2.0).round(), c, c);
            tabs.push((i, rect, close));
            x += tab_w;
        }
        let newtab_x = (x + t(tokens::SPACE_XS)).round();
        TabLayout {
            tabs,
            newtab: (newtab_x, 0.0, newtab_w, h),
            newtab_menu: (newtab_x + newtab_w, 0.0, chev_w, h),
        }
    }

    /// the new-tab '+' button rect, so the profile menu can anchor under it
    pub fn newtab_rect(&self) -> (f32, f32, f32, f32) {
        self.tab_layout().newtab
    }

    pub(crate) fn a11y_chrome_layout(&self) -> A11yChromeLayout {
        let tabs = self.tab_layout();
        A11yChromeLayout {
            title_bar: (0.0, 0.0, self.config.width as f32, self.title_bar_h),
            tabs: tabs.tabs.into_iter().map(|(index, rect, _)| (index, rect)).collect(),
            new_tab: tabs.newtab,
            new_tab_menu: tabs.newtab_menu,
            controls: self
                .control_rects()
                .into_iter()
                .map(|(hot, x0, x1)| (hot, (x0, 0.0, x1 - x0, self.title_bar_h)))
                .collect(),
        }
    }

    /// single-column geometry for the slide-in settings panel. body baselines
    /// are absolute and scroll-adjusted, so `build_settings` and `hit_test`
    /// share them; the body is clipped to [body_top, body_bottom] when drawn
    fn settings_geom(&self) -> SettingsGeom {
        let s = self.scale;
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let scroll = self.settings_scroll;

        let panel_w = (0.6 * w).clamp(460.0 * s, 760.0 * s).min((w - 160.0 * s).max(360.0 * s));
        let panel_top = self.title_bar_h;
        let panel_h = (h - self.title_bar_h - self.status_bar_h).max(1.0);
        let panel_x = (w - panel_w * self.settings_p).round();

        let pad = 22.0 * s;
        let content_x = panel_x + pad;
        let content_w = panel_w - pad * 2.0;
        let val_x = content_x + 152.0 * s;
        let bw = (30.0 * s).round();
        let bh = (26.0 * s).round();
        let val_w = (60.0 * s).round();
        let cluster = bw * 2.0 + val_w;

        let head_y = panel_top + 22.0 * s;
        let close_sz = 26.0 * s;
        let close_btn = (panel_x + panel_w - pad - close_sz, head_y - 3.0 * s, close_sz, close_sz);
        let body_top = panel_top + 60.0 * s;
        let body_bottom = panel_top + panel_h - 14.0 * s;

        // body laid out in local space (y from 0), converted to absolute below
        let row = 38.0 * s;
        let lh = 30.0 * s;
        let sec_gap = 24.0 * s;
        let hdr_adv = 32.0 * s;
        let chip_h = 42.0 * s;
        let key_row = 22.0 * s;
        // top headroom so the first row isn't clipped by the body scissor at
        // body_top — the browse button sits a few px above its label row
        let mut y = 8.0 * s;
        // PLUGINS first so the marketplace is the first thing in the gear menu
        let sec_plugins = y;
        y += hdr_adv;
        let plugins_n = self.plugins_installed.len();
        let plugin_first_l = y;
        y += if plugins_n == 0 { row } else { row * plugins_n as f32 };
        y += sec_gap;
        let sec_app = y;
        y += hdr_adv;
        let font_l = y;
        y += row;
        let fontfam_l = y;
        y += row;
        let pad_l = y;
        y += row;
        let cursor_l = y;
        y += row;
        let blink_l = y;
        y += row;
        let opacity_l = y;
        y += row;
        let line_height_l = y;
        y += row;
        let boldbright_l = y;
        y += row;
        let mica_l = y;
        y += row;
        let theme_label_l = y;
        y += lh;
        let theme_chip_l = y;
        // two rows of four chips
        y += (chip_h + 8.0 * s) * 2.0 + 2.0 * s;
        y += sec_gap;
        let sec_beh = y;
        y += hdr_adv;
        let scrollback_l = y;
        y += row;
        let copysel_l = y;
        y += row;
        y += sec_gap;
        let sec_shell = y;
        y += hdr_adv;
        let shell_l = y;
        y += row;
        let profile_l = y;
        y += row;
        let close_l = y;
        y += row;
        let backend_l = y;
        y += row;
        y += sec_gap;
        let sec_keys = y;
        y += hdr_adv;
        let keys_start_l = y;
        y += key_row * 6.0;
        y += sec_gap;
        let sec_about = y;
        y += hdr_adv;
        let about_start_l = y;
        y += 28.0 * s * 3.0;
        let content_h = y + 12.0 * s;

        let ay = |yl: f32| body_top - scroll + yl;
        let stepper = |x: f32, yl: f32| {
            let yb = ay(yl);
            ((x, yb, bw, bh), (x + bw + val_w, yb, bw, bh))
        };
        let (font_dec, font_inc) = stepper(val_x, font_l);
        let (pad_dec, pad_inc) = stepper(val_x, pad_l);
        let (op_dec, op_inc) = stepper(val_x, opacity_l);
        let (lh_dec, lh_inc) = stepper(val_x, line_height_l);
        let (sb_dec, sb_inc) = stepper(val_x, scrollback_l);
        // font names run long; this field gets more room than the steppers
        let fontfam_btn = (val_x, ay(fontfam_l), (cluster * 1.6).round(), bh);
        let cursor_btn = (val_x, ay(cursor_l), cluster, bh);
        let blink_btn = (val_x, ay(blink_l), cluster, bh);
        let boldbright_btn = (val_x, ay(boldbright_l), cluster, bh);
        let mica_btn = (val_x, ay(mica_l), cluster, bh);
        let copysel_btn = (val_x, ay(copysel_l), cluster, bh);
        let shell_btn = (val_x, ay(shell_l), cluster, bh);
        let profile_btn = (val_x, ay(profile_l), cluster, bh);
        let close_action_btn = (val_x, ay(close_l), cluster, bh);
        let backend_btn = (val_x, ay(backend_l), cluster, bh);
        // "browse" store button right-aligned in the PLUGINS header row
        let plugins_btn = (content_x + content_w - cluster, ay(sec_plugins) - 4.0 * s, cluster, bh);
        // one row per installed plugin: (name, enabled, toggle rect, row baseline)
        let plugin_rows: Vec<(String, bool, Rect, f32)> = self
            .plugins_installed
            .iter()
            .enumerate()
            .map(|(i, (name, on))| {
                let ry = ay(plugin_first_l + i as f32 * row);
                (name.clone(), *on, (val_x, ry, cluster, bh), ry)
            })
            .collect();

        let chip_gap = 8.0 * s;
        let chip_w = ((content_w - chip_gap * 3.0) / 4.0).floor();
        let mut theme_chips = [(0.0f32, 0.0f32, 0.0f32, 0.0f32); ThemeId::ALL.len() + 1];
        for (i, chip) in theme_chips.iter_mut().enumerate() {
            let col = (i % 4) as f32;
            let row_i = (i / 4) as f32;
            *chip = (
                content_x + (chip_w + chip_gap) * col,
                ay(theme_chip_l) + (chip_h + chip_gap) * row_i,
                chip_w,
                chip_h,
            );
        }

        let mut controls = vec![
            (Hot::FontDec, font_dec),
            (Hot::FontInc, font_inc),
            (Hot::FontCycle, fontfam_btn),
            (Hot::PadDec, pad_dec),
            (Hot::PadInc, pad_inc),
            (Hot::OpacityDec, op_dec),
            (Hot::OpacityInc, op_inc),
            (Hot::LineHeightDec, lh_dec),
            (Hot::LineHeightInc, lh_inc),
            (Hot::BoldBright, boldbright_btn),
            (Hot::Mica, mica_btn),
            (Hot::CursorCycle, cursor_btn),
            (Hot::CursorBlink, blink_btn),
            (Hot::ScrollbackDec, sb_dec),
            (Hot::ScrollbackInc, sb_inc),
            (Hot::CopyOnSelect, copysel_btn),
            (Hot::ShellCycle, shell_btn),
            (Hot::LoadProfile, profile_btn),
            (Hot::CloseActionCycle, close_action_btn),
            (Hot::BackendCycle, backend_btn),
            (Hot::OpenPlugins, plugins_btn),
        ];
        for (i, (_, _, rect, _)) in plugin_rows.iter().enumerate() {
            controls.push((Hot::PluginToggle(i), *rect));
        }
        for (i, id) in ThemeId::ALL.into_iter().enumerate() {
            controls.push((Hot::ThemeSet(id), theme_chips[i]));
        }
        controls.push((Hot::ThemeAuto, theme_chips[ThemeId::ALL.len()]));

        SettingsGeom {
            panel_x,
            panel_w,
            panel_top,
            panel_h,
            body_top,
            body_bottom,
            content_h,
            content_x,
            content_w,
            bh,
            val_w,
            head_y,
            close_btn,
            fontfam_y: ay(fontfam_l),
            fontfam_btn,
            sec_app_y: ay(sec_app),
            sec_beh_y: ay(sec_beh),
            sec_shell_y: ay(sec_shell),
            sec_plugins_y: ay(sec_plugins),
            sec_keys_y: ay(sec_keys),
            sec_about_y: ay(sec_about),
            font_y: ay(font_l),
            pad_y: ay(pad_l),
            opacity_y: ay(opacity_l),
            line_height_y: ay(line_height_l),
            boldbright_y: ay(boldbright_l),
            mica_y: ay(mica_l),
            cursor_y: ay(cursor_l),
            blink_y: ay(blink_l),
            theme_label_y: ay(theme_label_l),
            scrollback_y: ay(scrollback_l),
            copysel_y: ay(copysel_l),
            shell_y: ay(shell_l),
            profile_y: ay(profile_l),
            close_y: ay(close_l),
            backend_y: ay(backend_l),
            plugin_rows,
            keys_start_y: ay(keys_start_l),
            about_start_y: ay(about_start_l),
            font_dec,
            font_inc,
            pad_dec,
            pad_inc,
            op_dec,
            op_inc,
            lh_dec,
            lh_inc,
            boldbright_btn,
            mica_btn,
            cursor_btn,
            blink_btn,
            theme_chips,
            sb_dec,
            sb_inc,
            copysel_btn,
            shell_btn,
            profile_btn,
            close_action_btn,
            backend_btn,
            plugins_btn,
            controls,
        }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Hit {
        // the open settings panel takes priority over the chrome beneath it
        if self.settings_open && self.settings_p > 0.99 {
            let g = self.settings_geom();
            if in_rect(x, y, g.close_btn) {
                return Hit::Button(Hot::PanelClose);
            }
            // body controls are only hittable within the scroll viewport
            if y >= g.body_top && y < g.body_bottom {
                for (hot, rect) in g.controls {
                    if in_rect(x, y, rect) {
                        return Hit::Button(hot);
                    }
                }
            }
        }

        let w = self.config.width as f32;
        let h = self.config.height as f32;

        // chrome buttons sit flush against the top/corner resize border, so they
        // must win over it — otherwise clicking the top-right X (or the top edge
        // of any control) grabs a resize handle instead of closing the window
        if y < self.title_bar_h {
            for (c, x0, x1) in self.control_rects() {
                if x >= x0 && x < x1 {
                    return Hit::Button(c);
                }
            }
            let tl = self.tab_layout();
            if in_rect(x, y, self.window_tabs_rect()) {
                return Hit::Button(Hot::WindowTabs);
            }
            if in_rect(x, y, tl.newtab) {
                return Hit::Button(Hot::NewTab);
            }
            if in_rect(x, y, tl.newtab_menu) {
                return Hit::Button(Hot::NewTabMenu);
            }
            for (i, rect, close) in &tl.tabs {
                if in_rect(x, y, *close) {
                    return Hit::Button(Hot::TabClose(*i));
                }
                if in_rect(x, y, *rect) {
                    return Hit::Button(Hot::Tab(*i));
                }
            }
        }

        let e = (6.0 * self.scale).max(4.0);
        let left = x <= e;
        let right = x >= w - e;
        let top = y <= e;
        let bottom = y >= h - e;
        let dir = match (top, bottom, left, right) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (true, _, _, true) => Some(ResizeDirection::NorthEast),
            (_, true, true, _) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, _, _, _) => Some(ResizeDirection::North),
            (_, true, _, _) => Some(ResizeDirection::South),
            (_, _, true, _) => Some(ResizeDirection::West),
            (_, _, _, true) => Some(ResizeDirection::East),
            _ => None,
        };
        if let Some(d) = dir {
            return Hit::Resize(d);
        }

        if y < self.title_bar_h {
            return Hit::TitleBar;
        }
        Hit::Content
    }

    fn upload_atlas(&mut self) {
        // the atlas grew (1024 -> 2048): recreate the gpu textures + bind group
        // at the new dim before uploading. repack_at already flagged a full
        // re-upload, so the bands below repopulate the fresh textures this call
        if self.atlas.dim != self.atlas_gpu_dim {
            let (at, ct, bg) = make_atlas_bind_group(
                &self.device,
                self.atlas.dim,
                &self.sampler,
                &self.icon_view,
                &self.icon_sampler,
                &self.color_sampler,
            );
            self.atlas_texture = at;
            self.color_texture = ct;
            self.atlas_bind_group = bg;
            self.atlas_gpu_dim = self.atlas.dim;
        }
        let dim = self.atlas.dim;
        // upload only the row band that changed; a freshly repacked atlas has no
        // band and uploads in full. width is the full atlas width (R8, so
        // bytes_per_row == dim, already 256-aligned for dim=1024)
        if self.atlas.dirty {
            let (y0, y1) = self.atlas.dirty_y.unwrap_or((0, dim));
            let (y0, y1) = (y0.min(dim), y1.min(dim));
            if y1 > y0 {
                let off = (y0 * dim) as usize;
                let end = (y1 * dim) as usize;
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &self.atlas.data[off..end],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(dim),
                        rows_per_image: Some(y1 - y0),
                    },
                    wgpu::Extent3d {
                        width: dim,
                        height: y1 - y0,
                        depth_or_array_layers: 1,
                    },
                );
            }
            self.atlas.dirty = false;
            self.atlas.dirty_y = None;
        }
        // the color (emoji) atlas uploads independently; RGBA so bytes_per_row
        // is dim*4 (16384 for dim=1024, still 256-aligned)
        if self.atlas.color_dirty {
            let (y0, y1) = self.atlas.color_dirty_y.unwrap_or((0, dim));
            let (y0, y1) = (y0.min(dim), y1.min(dim));
            if y1 > y0 {
                let off = (y0 * dim * 4) as usize;
                let end = (y1 * dim * 4) as usize;
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.color_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &self.atlas.color_data[off..end],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(dim * 4),
                        rows_per_image: Some(y1 - y0),
                    },
                    wgpu::Extent3d {
                        width: dim,
                        height: y1 - y0,
                        depth_or_array_layers: 1,
                    },
                );
            }
            self.atlas.color_dirty = false;
            self.atlas.color_dirty_y = None;
        }
    }

    fn push_rect(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, rgb: Rgb, alpha: f32) {
        let lin = rgb.to_linear_f32();
        out.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            color: [lin[0], lin[1], lin[2], alpha],
            kind: 0,
            shape: [0.0; 3],
        });
    }

    // a flat geometry helper like push_rect; a struct would only add noise
    #[allow(clippy::too_many_arguments)]
    fn push_round(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, r: f32, rgb: Rgb, alpha: f32) {
        Self::push_shape(out, (x, y, w, h), [r, 0.0, 0.0], rgb, alpha);
    }

    /// rounded outline `t` px wide, drawn inside the rect
    #[allow(clippy::too_many_arguments)]
    fn push_ring(out: &mut Vec<Instance>, rect: Rect, r: f32, t: f32, rgb: Rgb, alpha: f32) {
        Self::push_shape(out, rect, [r, 0.0, t], rgb, alpha);
    }

    /// soft shadow under a rounded rect: the rect dropped by `dy` and blurred
    /// over `blur` px on every side
    #[allow(clippy::too_many_arguments)]
    fn push_shadow(out: &mut Vec<Instance>, rect: Rect, r: f32, blur: f32, dy: f32, rgb: Rgb, alpha: f32) {
        let (x, y, w, h) = rect;
        let blur = blur.max(0.5);
        Self::push_shape(out, (x - blur, y + dy - blur, w + blur * 2.0, h + blur * 2.0), [r, blur, 0.0], rgb, alpha);
    }

    fn push_shape(out: &mut Vec<Instance>, rect: Rect, shape: [f32; 3], rgb: Rgb, alpha: f32) {
        let (x, y, w, h) = rect;
        let lin = rgb.to_linear_f32();
        out.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv_min: [0.0, 0.0],
            uv_max: [w, h],
            color: [lin[0], lin[1], lin[2], alpha],
            kind: 4,
            shape,
        });
    }

    /// a chrome icon centered in `rect`, `size` px square
    #[allow(clippy::too_many_arguments)]
    fn draw_icon(atlas: &mut GlyphAtlas, out: &mut Vec<Instance>, icon: Icon, rect: Rect, size: f32, stroke: f32, rgb: Rgb, alpha: f32) {
        let size = size.round().max(1.0);
        let Some(g) = atlas.get_icon(icon, size as u32, stroke) else {
            return;
        };
        let (x, y, w, h) = rect;
        let lin = rgb.to_linear_f32();
        out.push(Instance {
            pos: [(x + (w - size) / 2.0).round(), (y + (h - size) / 2.0).round()],
            size: [g.width, g.height],
            uv_min: g.uv_min,
            uv_max: g.uv_max,
            color: [lin[0], lin[1], lin[2], alpha],
            kind: 1,
            shape: [0.0; 3],
        });
    }

    /// draw the full-color app-icon badge (kind 2) at (x,y), size x size px;
    /// `alpha` fades it for the startup reveal
    fn push_icon(out: &mut Vec<Instance>, x: f32, y: f32, size: f32, alpha: f32) {
        out.push(Instance {
            pos: [x, y],
            size: [size, size],
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            color: [1.0, 1.0, 1.0, alpha],
            kind: 2,
            shape: [0.0; 3],
        });
    }

    /// banded vertical gradient from `top` (at y) to `bottom` (at y+h)
    // a flat geometry helper; bundling the rect+colors into a struct would only
    // obscure the call sites
    #[allow(clippy::too_many_arguments)]
    fn push_vgradient(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, top: Rgb, bottom: Rgb, bands: usize) {
        let n = bands.max(1);
        let band_h = h / n as f32;
        let lerp = |a: u8, b: u8, t: f32| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        for i in 0..n {
            let t = (i as f32 + 0.5) / n as f32;
            let c = Rgb::new(lerp(top.r, bottom.r, t), lerp(top.g, bottom.g, t), lerp(top.b, bottom.b, t));
            // overlap by 1px so seams never show
            Self::push_rect(out, x, (y + i as f32 * band_h).floor(), w, band_h.ceil() + 1.0, c, 1.0);
        }
    }

    /// 1px outline around a rect
    fn stroke_rect(out: &mut Vec<Instance>, r: (f32, f32, f32, f32), t: f32, c: Rgb) {
        Self::stroke_rect_a(out, r, t, c, 1.0);
    }

    /// outline around a rect at a given opacity
    fn stroke_rect_a(out: &mut Vec<Instance>, r: (f32, f32, f32, f32), t: f32, c: Rgb, a: f32) {
        let (x, y, w, h) = r;
        Self::push_rect(out, x, y, w, t, c, a);
        Self::push_rect(out, x, y + h - t, w, t, c, a);
        Self::push_rect(out, x, y, t, h, c, a);
        Self::push_rect(out, x + w - t, y, t, h, c, a);
    }

    /// procedurally render a box-drawing / block char filling the exact cell so
    /// it tiles seamlessly with neighbours. returns false if `c` isn't one we draw
    fn draw_box(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, c: char, col: Rgb) -> bool {
        // strokes toward (left, right, up, down): 0 none, 1 light, 2 heavy, 3 double
        let (l, r, u, d): (u8, u8, u8, u8) = match c {
            '\u{2500}' => (1, 1, 0, 0),
            '\u{2501}' => (2, 2, 0, 0),
            '\u{2502}' => (0, 0, 1, 1),
            '\u{2503}' => (0, 0, 2, 2),
            // the rounded corners ╭╮╰╯ (U+256D-2570) are deliberately NOT here:
            // this procedural path can only draw square rects, so they fall
            // through to the atlas where boxdraw's quarter-arc coverage keeps
            // them round while still filling the exact cell
            '\u{250C}' => (0, 1, 0, 1),
            '\u{2510}' => (1, 0, 0, 1),
            '\u{2514}' => (0, 1, 1, 0),
            '\u{2518}' => (1, 0, 1, 0),
            '\u{251C}' => (0, 1, 1, 1),
            '\u{2524}' => (1, 0, 1, 1),
            '\u{252C}' => (1, 1, 0, 1),
            '\u{2534}' => (1, 1, 1, 0),
            '\u{253C}' => (1, 1, 1, 1),
            '\u{2550}' => (3, 3, 0, 0),
            '\u{2551}' => (0, 0, 3, 3),
            '\u{2554}' => (0, 3, 0, 3),
            '\u{2557}' => (3, 0, 0, 3),
            '\u{255A}' => (0, 3, 3, 0),
            '\u{255D}' => (3, 0, 3, 0),
            '\u{2560}' => (0, 3, 3, 3),
            '\u{2563}' => (3, 0, 3, 3),
            '\u{2566}' => (3, 3, 0, 3),
            '\u{2569}' => (3, 3, 3, 0),
            '\u{256C}' => (3, 3, 3, 3),
            _ => return Self::draw_block(out, x, y, w, h, c, col),
        };
        let thin = (h * 0.07).round().max(1.0);
        let mx = x + w / 2.0;
        let my = y + h / 2.0;
        let gap = thin + 1.0;
        // spans stay exact (x..mx..x+w, y..my..y+h) so adjacent cells join cleanly
        let hbar = |out: &mut Vec<Instance>, xa: f32, xb: f32, wt: u8| match wt {
            1 => Self::push_rect(out, xa, my - thin / 2.0, xb - xa, thin, col, 1.0),
            2 => Self::push_rect(out, xa, my - thin, xb - xa, thin * 2.0, col, 1.0),
            3 => {
                Self::push_rect(out, xa, my - gap - thin / 2.0, xb - xa, thin, col, 1.0);
                Self::push_rect(out, xa, my + gap - thin / 2.0, xb - xa, thin, col, 1.0);
            }
            _ => {}
        };
        let vbar = |out: &mut Vec<Instance>, ya: f32, yb: f32, wt: u8| match wt {
            1 => Self::push_rect(out, mx - thin / 2.0, ya, thin, yb - ya, col, 1.0),
            2 => Self::push_rect(out, mx - thin, ya, thin * 2.0, yb - ya, col, 1.0),
            3 => {
                Self::push_rect(out, mx - gap - thin / 2.0, ya, thin, yb - ya, col, 1.0);
                Self::push_rect(out, mx + gap - thin / 2.0, ya, thin, yb - ya, col, 1.0);
            }
            _ => {}
        };
        if l > 0 {
            hbar(out, x, mx, l);
        }
        if r > 0 {
            hbar(out, mx, x + w, r);
        }
        if u > 0 {
            vbar(out, y, my, u);
        }
        if d > 0 {
            vbar(out, my, y + h, d);
        }
        true
    }

    /// block elements (U+2580..U+259F): full/half/eighth blocks + shade fills
    fn draw_block(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, c: char, col: Rgb) -> bool {
        match c {
            '\u{2588}' => Self::push_rect(out, x, y, w, h, col, 1.0),
            '\u{2580}' => Self::push_rect(out, x, y, w, h / 2.0, col, 1.0), // upper half
            '\u{2584}' => Self::push_rect(out, x, y + h / 2.0, w, h / 2.0, col, 1.0), // lower half
            '\u{258C}' => Self::push_rect(out, x, y, w / 2.0, h, col, 1.0), // left half
            '\u{2590}' => Self::push_rect(out, x + w / 2.0, y, w / 2.0, h, col, 1.0), // right half
            '\u{2591}' => Self::push_rect(out, x, y, w, h, col, 0.25),
            '\u{2592}' => Self::push_rect(out, x, y, w, h, col, 0.5),
            '\u{2593}' => Self::push_rect(out, x, y, w, h, col, 0.75),
            // lower eighths ▁..▇
            '\u{2581}'..='\u{2587}' => {
                let frac = (c as u32 - 0x2580) as f32 / 8.0;
                let bh = h * frac;
                Self::push_rect(out, x, y + h - bh, w, bh, col, 1.0);
            }
            // left eighths ▉..▏ (▌ handled above as left half)
            '\u{2589}'..='\u{258F}' => {
                let frac = (8 - (c as u32 - 0x2588)) as f32 / 8.0;
                Self::push_rect(out, x, y, w * frac, h, col, 1.0);
            }
            '\u{2594}' => Self::push_rect(out, x, y, w, h / 8.0, col, 1.0), // upper eighth
            '\u{2595}' => Self::push_rect(out, x + w * 7.0 / 8.0, y, w / 8.0, h, col, 1.0), // right eighth
            // quadrant blocks (▖▗▘▙▚▛▜▝▞▟): 2x2 sub-cell fills, so low-res block
            // art and mosaic logos tile solid instead of leaving gaps
            '\u{2596}'..='\u{259F}' => {
                let (hw, hh) = (w / 2.0, h / 2.0);
                // bits: 1=upper-left 2=upper-right 4=lower-left 8=lower-right
                let mask: u8 = match c {
                    '\u{2598}' => 0b0001,
                    '\u{259D}' => 0b0010,
                    '\u{2596}' => 0b0100,
                    '\u{2597}' => 0b1000,
                    '\u{2599}' => 0b1101,
                    '\u{259A}' => 0b1001,
                    '\u{259B}' => 0b0111,
                    '\u{259C}' => 0b1011,
                    '\u{259E}' => 0b0110,
                    '\u{259F}' => 0b1110,
                    _ => 0,
                };
                if mask & 1 != 0 { Self::push_rect(out, x, y, hw, hh, col, 1.0); }
                if mask & 2 != 0 { Self::push_rect(out, x + hw, y, hw, hh, col, 1.0); }
                if mask & 4 != 0 { Self::push_rect(out, x, y + hh, hw, hh, col, 1.0); }
                if mask & 8 != 0 { Self::push_rect(out, x + hw, y + hh, hw, hh, col, 1.0); }
            }
            _ => return false,
        }
        true
    }

    /// emit one z-layer of a pane's kitty placements as color-atlas quads,
    /// anchored to their absolute lines so they scroll with the text. `under`
    /// selects the z < 0 layer (drawn before any cell content); placements
    /// stack by z within the layer, arrival order breaking ties
    #[allow(clippy::too_many_arguments)]
    fn push_image_placements(
        atlas: &mut GlyphAtlas,
        out: &mut Vec<Instance>,
        term: &Terminal,
        ox: f32,
        oy: f32,
        cell_w: f32,
        cell_h: f32,
        under: bool,
    ) {
        let grid = &term.grid;
        let content_h = grid.rows as f32 * cell_h;
        let mut layer: Vec<&crate::grid::Placement> =
            grid.placements().iter().filter(|p| (p.z < 0) == under).collect();
        layer.sort_by_key(|p| p.z);
        for p in layer {
            let Some(img) = term.images.get(p.image_id) else {
                continue;
            };
            let Some(g) = atlas.get_image(img.key, &img.rgba, img.width, img.height) else {
                continue;
            };
            // a client-requested cell box (kitty c=/r=) scales the image; one
            // axis alone keeps the aspect ratio, per the protocol
            let (disp_w, disp_h) = match (p.cols, p.rows) {
                (0, 0) => (g.width, g.height),
                (c, 0) => {
                    let w = c as f32 * cell_w;
                    (w, w * g.height / g.width.max(1.0))
                }
                (0, r) => {
                    let h = r as f32 * cell_h;
                    (h * g.width / g.height.max(1.0), h)
                }
                (c, r) => (c as f32 * cell_w, r as f32 * cell_h),
            };
            // crop the quad to the pane's visible rows so an image taller than the
            // remaining space doesn't bleed into a sibling pane or the status bar,
            // and a placement scrolled partly above the top shows its lower rows
            let top_y = grid.screen_row_signed(p.abs_line) as f32 * cell_h;
            let Some((vis_top, vis_h, uf0, uf1)) = clip_image_v(top_y, disp_h, content_h) else {
                continue;
            };
            let uspan = g.uv_max[1] - g.uv_min[1];
            out.push(Instance {
                pos: [ox + p.col as f32 * cell_w, oy + vis_top],
                size: [disp_w, vis_h],
                uv_min: [g.uv_min[0], g.uv_min[1] + uspan * uf0],
                uv_max: [g.uv_max[0], g.uv_min[1] + uspan * uf1],
                color: [0.0, 0.0, 0.0, 1.0],
                kind: 3,
                shape: [0.0; 3],
            });
        }
    }

    /// paint the (row, col) cell tile of an image's virtual (U=1) placement:
    /// the image aspect-fits inside the placement's cell box, centered, and a
    /// placeholder cell shows its intersection with the fitted rect
    #[allow(clippy::too_many_arguments)]
    fn push_placeholder_tile(
        atlas: &mut GlyphAtlas,
        out: &mut Vec<Instance>,
        term: &Terminal,
        image_id: u32,
        row: u16,
        col: u16,
        x: f32,
        y: f32,
        cell_w: f32,
        cell_h: f32,
    ) {
        let Some((vc, vr)) = term.grid.virtual_placement(image_id) else {
            return;
        };
        let Some(img) = term.images.get(image_id) else {
            return;
        };
        let Some(g) = atlas.get_image(img.key, &img.rgba, img.width, img.height) else {
            return;
        };
        // U=1 wants c=/r=; a zero axis falls back to the image's natural cell size
        let vc = if vc > 0 { vc as f32 } else { (g.width / cell_w).ceil().max(1.0) };
        let vr = if vr > 0 { vr as f32 } else { (g.height / cell_h).ceil().max(1.0) };
        let (row, col) = (row as f32, col as f32);
        if row >= vr || col >= vc {
            return;
        }
        let (bw, bh) = (vc * cell_w, vr * cell_h);
        let scale = (bw / g.width.max(1.0)).min(bh / g.height.max(1.0));
        let (fw, fh) = (g.width * scale, g.height * scale);
        let (fx, fy) = ((bw - fw) / 2.0, (bh - fh) / 2.0);
        // this cell's rect in box coordinates, clipped to the fitted image
        let (cx0, cy0) = (col * cell_w, row * cell_h);
        let ix0 = cx0.max(fx);
        let iy0 = cy0.max(fy);
        let ix1 = (cx0 + cell_w).min(fx + fw);
        let iy1 = (cy0 + cell_h).min(fy + fh);
        if ix1 <= ix0 || iy1 <= iy0 {
            return;
        }
        let (us, vs) = (g.uv_max[0] - g.uv_min[0], g.uv_max[1] - g.uv_min[1]);
        out.push(Instance {
            pos: [x + ix0 - cx0, y + iy0 - cy0],
            size: [ix1 - ix0, iy1 - iy0],
            uv_min: [g.uv_min[0] + us * (ix0 - fx) / fw, g.uv_min[1] + vs * (iy0 - fy) / fh],
            uv_max: [g.uv_min[0] + us * (ix1 - fx) / fw, g.uv_min[1] + vs * (iy1 - fy) / fh],
            color: [0.0, 0.0, 0.0, 1.0],
            kind: 3,
            shape: [0.0; 3],
        });
    }

    /// draw one terminal grid at a pixel origin
    #[allow(clippy::too_many_arguments)]
    fn draw_grid(
        atlas: &mut GlyphAtlas,
        palette: &Palette,
        out: &mut Vec<Instance>,
        term: &Terminal,
        ox: f32,
        oy: f32,
        focused: bool,
        blink_on: f32,
        blink_phase: f32,
        beam_w: f32,
        style: CursorShape,
        sel: Option<SelSpan>,
        link: Option<(usize, usize, usize)>,
        matches: &[(usize, usize, usize, bool)],
        bold_as_bright: bool,
        min_contrast: f32,
        ligatures: bool,
    ) {
        let sel_col = palette.sel;
        let m = atlas.metrics(FontId::Content);
        // whole-pixel baseline so glyph rasters land on the same subpixel
        // phase in every row
        let (cell_w, cell_h, ascent, em_px) = (m.cell_w, m.cell_h, m.ascent.round(), m.px);
        let grid = &term.grid;
        let scrolled = grid.view_offset > 0;
        let cur = grid.cursor;
        let show_cursor = cur.visible && !scrolled;
        let (crow, ccol) = (cur.row, cur.col.min(grid.cols.saturating_sub(1)));
        let sel_norm = sel.map(|(a, b, block)| if a <= b { (a, b, block) } else { (b, a, block) });

        // kitty placements with z < 0 draw beneath everything the cells paint,
        // so the image shows wherever a cell has no ink or colored background
        Self::push_image_placements(atlas, out, term, ox, oy, cell_w, cell_h, true);

        for r in 0..grid.rows {
            let line = grid.line_at(r);
            let mut run_start = 0usize;
            let mut run_bg = None;
            for c in 0..=grid.cols {
                let bg = if c == grid.cols {
                    None
                } else {
                    let cell = line.get(c).copied().unwrap_or_default();
                    if cell.attrs.hidden() {
                        None
                    } else {
                        let color = if cell.attrs.inverse() { cell.fg } else { cell.bg };
                        let color = palette.resolve_bg(color);
                        (color != palette.bg).then_some(color)
                    }
                };
                if bg == run_bg {
                    continue;
                }
                if let Some(color) = run_bg {
                    Self::push_rect(
                        out,
                        ox + run_start as f32 * cell_w,
                        oy + r as f32 * cell_h,
                        (c - run_start) as f32 * cell_w,
                        cell_h,
                        color,
                        1.0,
                    );
                }
                run_start = c;
                run_bg = bg;
            }
        }

        for &(row, start, len, current) in matches {
            if row >= grid.rows {
                continue;
            }
            let line = grid.line_at(row);
            let end = start.saturating_add(len).min(grid.cols);
            let (color, alpha) =
                if current { (palette.cursor, 0.75) } else { (palette.sel, 0.45) };
            let mut col = start.min(end);
            while col < end {
                while col < end
                    && line.get(col).is_some_and(|cell| cell.attrs.hidden())
                {
                    col += 1;
                }
                let visible_start = col;
                while col < end
                    && !line.get(col).is_some_and(|cell| cell.attrs.hidden())
                {
                    col += 1;
                }
                if visible_start < col {
                    Self::push_rect(
                        out,
                        ox + visible_start as f32 * cell_w,
                        oy + row as f32 * cell_h,
                        (col - visible_start) as f32 * cell_w,
                        cell_h,
                        color,
                        alpha,
                    );
                }
            }
        }

        // the placeholder cell drawn immediately to the left, for kitty's
        // omitted-diacritic inheritance; the `at` column guards against gaps
        struct PhPrev {
            fg: crate::color::Color,
            ul: crate::color::Color,
            row: u16,
            col: u16,
            msb: u16,
            at: usize,
        }
        let mut run_buf = String::new();
        for r in 0..grid.rows {
            let line = grid.line_at(r);
            // hold a ligature strip until its cells have painted selection
            let mut liga_until = 0usize;
            let mut liga_strip: Option<Instance> = None;
            let mut ph_prev: Option<PhPrev> = None;
            for c in 0..grid.cols {
                let cell = line.get(c).copied().unwrap_or_default();
                if cell.attrs.hidden() {
                    continue;
                }
                let (mut fg_c, mut bg_c) = (cell.fg, cell.bg);
                if cell.attrs.inverse() {
                    std::mem::swap(&mut fg_c, &mut bg_c);
                }
                let fg_c = Palette::bold_bright(fg_c, cell.attrs.bold(), bold_as_bright);
                let mut fg = palette.resolve_fg(fg_c);
                let bg = palette.resolve_bg(bg_c);
                if cell.attrs.dim() {
                    // 2/3 in sRGB ≈ 0.42 linear — reads as dim but stays
                    // legible; a straight half was ~0.2 linear, near-invisible
                    fg = Rgb::new(
                        (fg.r as u16 * 2 / 3) as u8,
                        (fg.g as u16 * 2 / 3) as u8,
                        (fg.b as u16 * 2 / 3) as u8,
                    );
                }
                // accessibility floor last, so it also rescues dimmed text
                if min_contrast > 1.0 {
                    fg = crate::color::apply_min_contrast(fg, bg, min_contrast);
                }
                // blinking cells hide their glyph + decorations on the off phase
                let blink_hidden = cell.attrs.blink() && blink_on < 0.5;

                let x = ox + c as f32 * cell_w;
                let y = oy + r as f32 * cell_h;
                let is_cursor = show_cursor && r == crow && c == ccol;
                let selected = sel_norm
                    .map(|(s, e, block)| {
                        if block {
                            // rectangle: rows and cols span independently
                            r >= s.0
                                && r <= e.0
                                && c >= s.1.min(e.1)
                                && c <= s.1.max(e.1)
                        } else {
                            (r, c) >= s && (r, c) <= e
                        }
                    })
                    .unwrap_or(false);

                if selected {
                    Self::push_rect(out, x, y, cell_w, cell_h, sel_col, 0.9);
                }
                if is_cursor {
                    // DECSCUSR's steady variants force a solid cursor; its
                    // blinking variants follow the clock even when the
                    // configured default is steady
                    let cur_on = match cur.shape_blink {
                        Some(true) => blink_phase,
                        Some(false) => 1.0,
                        None => blink_on,
                    };
                    let alpha = if focused { cur_on } else { 0.4 };
                    // an app's DECSCUSR shape overrides the configured default
                    let shape = if cur.shape_set { cur.shape } else { style };
                    if alpha > 0.0 {
                        match shape {
                            CursorShape::Bar => {
                                Self::push_rect(out, x, y, beam_w, cell_h, palette.cursor, alpha);
                            }
                            CursorShape::Underline => {
                                let t = beam_w.max(2.0);
                                Self::push_rect(out, x, y + cell_h - t, cell_w, t, palette.cursor, alpha);
                            }
                            CursorShape::Block => {
                                // cover both halves when sitting on a wide glyph
                                let cw = if line.get(c + 1).map(|n| n.c == '\0').unwrap_or(false) {
                                    cell_w * 2.0
                                } else {
                                    cell_w
                                };
                                if focused {
                                    Self::push_rect(out, x, y, cw, cell_h, palette.cursor, alpha);
                                    fg = fg.lerp(palette.bg, alpha);
                                } else {
                                    // an unfocused pane shows a hollow block so
                                    // the glyph under it stays readable
                                    let t = beam_w.max(1.0);
                                    Self::stroke_rect(out, (x, y, cw, cell_h), t, palette.cursor);
                                }
                            }
                        }
                    }
                }
                // underline a hovered (ctrl) url so it reads as clickable
                if link.map(|(lr, a, b)| r == lr && c >= a && c < b).unwrap_or(false) {
                    let t = (cell_h * 0.06).max(1.0);
                    Self::push_rect(out, x, y + cell_h - t, cell_w, t, palette.cursor, 1.0);
                }
                // '\0' marks the second cell of a wide glyph — the lead cell's
                // glyph already covers it, so skip drawing here
                if !blink_hidden && cell.c != ' ' && cell.c != '\0' {
                    // box-drawing / block glyphs are drawn procedurally so they
                    // tile seamlessly (font glyphs leave gaps at cell edges)
                    if c < liga_until {
                        // inside a run: the strip flushes at the last covered
                        // cell, after its bg/selection; per-cell decorations
                        // still draw in their own iterations
                        if c + 1 == liga_until
                            && let Some(strip) = liga_strip.take()
                        {
                            out.push(strip);
                        }
                    } else if cell.c == crate::image::PLACEHOLDER {
                        // kitty unicode placeholder: the cell paints one tile
                        // of its image's virtual (U=1) placement instead of a
                        // glyph; missing row/col diacritics inherit from the
                        // placeholder cell to its left, per the protocol
                        let inherit = ph_prev
                            .as_ref()
                            .filter(|q| q.at + 1 == c && q.fg == cell.fg && q.ul == cell.attrs.ul);
                        let marks =
                            if cell.cluster != 0 { grid.cluster_str(cell.cluster) } else { "" };
                        let mut placed = None;
                        if let Some(p) = crate::image::decode_placeholder(cell.fg, marks) {
                            let row = p.row.or(inherit.map(|q| q.row));
                            let col = p.col.or_else(|| {
                                inherit
                                    .filter(|q| p.row.is_none() || p.row == Some(q.row))
                                    .map(|q| q.col + 1)
                            });
                            // msb only inherits along a contiguous run: same
                            // row and the very next column (spec rule 3) —
                            // adjacency alone would smear it across explicit
                            // row/col jumps
                            let msb = p
                                .msb
                                .or_else(|| {
                                    inherit
                                        .filter(|q| {
                                            (p.row.is_none() || p.row == Some(q.row))
                                                && (p.col.is_none() || p.col == Some(q.col + 1))
                                        })
                                        .map(|q| q.msb)
                                })
                                .unwrap_or(0);
                            if let (Some(prow), Some(pcol)) = (row, col) {
                                let id = (msb as u32) << 24 | p.id_low;
                                Self::push_placeholder_tile(
                                    atlas, out, term, id, prow, pcol, x, y, cell_w, cell_h,
                                );
                                placed = Some(PhPrev {
                                    fg: cell.fg,
                                    ul: cell.attrs.ul,
                                    row: prow,
                                    col: pcol,
                                    msb,
                                    at: c,
                                });
                            }
                        }
                        ph_prev = placed;
                    } else if Self::draw_box(out, x, y, cell_w, cell_h, cell.c, fg) {
                        // handled
                    } else {
                        // a chain of shapeable punctuation with identical
                        // style opens a ligature run; the cursor cell never
                        // joins one (a block cursor recolors only its own fg)
                        let mut run_len = 1;
                        if ligatures && !is_cursor && cell.cluster == 0 && liga_char(cell.c) {
                            while run_len < 32 {
                                let Some(nc) = line.get(c + run_len) else { break };
                                if (show_cursor && r == crow && c + run_len == ccol)
                                    || nc.cluster != 0
                                    || !liga_char(nc.c)
                                    || nc.fg != cell.fg
                                    || nc.bg != cell.bg
                                    || nc.attrs.bold() != cell.attrs.bold()
                                    || nc.attrs.italic() != cell.attrs.italic()
                                    || nc.attrs.dim() != cell.attrs.dim()
                                    || nc.attrs.inverse() != cell.attrs.inverse()
                                    || nc.attrs.hidden()
                                    || nc.attrs.blink() != cell.attrs.blink()
                                {
                                    break;
                                }
                                run_len += 1;
                            }
                        }
                        let mut ran = false;
                        if run_len >= 2 {
                            run_buf.clear();
                            run_buf.extend(line[c..c + run_len].iter().map(|n| n.c));
                            if let Some(g) = atlas.get_run(&run_buf, cell.attrs.bold(), cell.attrs.italic()) {
                                let lin = fg.to_linear_f32();
                                liga_strip = Some(Instance {
                                    pos: [x + g.left, y + ascent - g.top],
                                    size: [g.width, g.height],
                                    uv_min: g.uv_min,
                                    uv_max: g.uv_max,
                                    color: [lin[0], lin[1], lin[2], 1.0],
                                    kind: 1,
                                    shape: [0.0; 3],
                                });
                                liga_until = c + run_len;
                                ran = true;
                            }
                        }
                        if !ran {
                            let gk = GlyphKey {
                                font: FontId::Content,
                                c: cell.c,
                                bold: cell.attrs.bold(),
                                italic: cell.attrs.italic(),
                            };
                            // a cell carrying combining marks or an emoji ZWJ
                            // sequence composites/ligates its whole grapheme
                            // cluster; fall back to the base char if that yields
                            // nothing (e.g. the font can't ligate the sequence)
                            let glyph = if cell.cluster != 0 {
                                let cg = atlas.get_cluster(
                                    grid.cluster_str(cell.cluster),
                                    cell.attrs.bold(),
                                    cell.attrs.italic(),
                                );
                                if cg.is_some() { cg } else { atlas.get(gk) }
                            } else {
                                atlas.get(gk)
                            };
                            if let Some(g) = glyph {
                                let lin = fg.to_linear_f32();
                                out.push(Instance {
                                    pos: [x + g.left, y + ascent - g.top],
                                    size: [g.width, g.height],
                                    uv_min: g.uv_min,
                                    uv_max: g.uv_max,
                                    color: [lin[0], lin[1], lin[2], 1.0],
                                    kind: if g.color { 3 } else { 1 },
                                    shape: [0.0; 3],
                                });
                            }
                        }
                    }
                }
                // underline / overline / strikethrough decorations, anchored
                // to the font baseline and drawn in the SGR 58 underline color
                // when one is set (LSP squiggles color independently of text)
                if !blink_hidden {
                    let t = (cell_h * 0.06).max(1.0);
                    let deco = if cell.attrs.ul == crate::color::Color::Default {
                        fg
                    } else {
                        palette.resolve_fg(cell.attrs.ul)
                    };
                    underline_rects(cell.attrs.underline(), cell_w, cell_h, ascent, em_px, t, |rx, ry, rw, rh| {
                        Self::push_rect(out, x + rx, y + ry, rw, rh, deco, 1.0);
                    });
                    if cell.attrs.strike() {
                        // through the middle of the x-height, not the cell box
                        let sy = (ascent - em_px * 0.26).round().max(0.0);
                        Self::push_rect(out, x, y + sy, cell_w, t, deco, 1.0);
                    }
                    if cell.attrs.overline() {
                        let oy_l = (ascent - em_px * 0.88).round().max(0.0);
                        Self::push_rect(out, x, y + oy_l, cell_w, t, deco, 1.0);
                    }
                }
            }
        }

        // kitty image placements with z >= 0 draw on top of the cells they
        // overlap (the z < 0 pass ran before any cell content)
        Self::push_image_placements(atlas, out, term, ox, oy, cell_w, cell_h, false);

        // scroll thumb on the pane's right edge, sized + positioned to the
        // viewport's slice of total (scrollback + screen) lines. keep this rail
        // stable whenever history exists, rather than making it appear only on
        // hover at the live bottom
        if !grid.scrollback.is_empty()
            && let Some(t) =
                Self::scrollbar_geom(ox, oy, grid.cols, grid.rows, cell_w, cell_h, beam_w, grid.scrollback.len(), grid.view_offset)
        {
            Self::push_rect(out, t.track_x, t.track_y, t.track_w, t.track_h, palette.mute, 0.18);
            Self::push_rect(out, t.track_x, t.thumb_y, t.track_w, t.thumb_h, palette.mute, 0.8);
            let total = grid.total_lines().max(1) as f32;
            let tick_h = beam_w.min(cell_h).max(1.0);
            let max_y = t.track_y + t.track_h - tick_h;
            let mut last_y = None;
            for (row, exit) in grid.prompt_rows() {
                let y = (t.track_y + t.track_h * (row as f32 + 0.5) / total).round().min(max_y);
                // a failed command's tick paints in the theme's red and wins
                // over a coincident plain tick
                let failed = exit.is_some_and(|c| c != 0);
                if last_y == Some(y) && !failed {
                    continue;
                }
                let (color, alpha) = if failed {
                    (palette.ansi_color(1), 0.95)
                } else if y + tick_h > t.thumb_y && y < t.thumb_y + t.thumb_h {
                    // a paper tick over the thumb blends into it (both light);
                    // cut a dark notch instead so a prompt at the live bottom
                    // stays marked
                    (palette.bg, 0.9)
                } else {
                    (palette.paper, 0.6)
                };
                Self::push_rect(out, t.track_x, y, t.track_w, tick_h, color, alpha);
                last_y = Some(y);
            }
        }
    }

    /// draw an uncommitted IME composition inline at the cursor cell, over the
    /// grid content (the string reaches the pty only on commit). the run is
    /// bg-filled and underlined, the active clause gets a double-thick line,
    /// and a caret bar marks the composition cursor. a run that would overflow
    /// the right edge slides left; wider than the pane, it keeps its tail —
    /// that end is where the caret almost always is
    #[allow(clippy::too_many_arguments)] // pane geometry + composition state
    fn draw_preedit(
        atlas: &mut GlyphAtlas,
        palette: &Palette,
        out: &mut Vec<Instance>,
        term: &Terminal,
        ox: f32,
        oy: f32,
        pre: PreeditView,
        beam_w: f32,
    ) {
        let grid = &term.grid;
        // scrolled into history the cursor cell isn't on screen; nothing to anchor to
        if pre.text.is_empty() || grid.view_offset > 0 || grid.cols == 0 {
            return;
        }
        let m = atlas.metrics(FontId::Content);
        let (cell_w, cell_h, ascent, em_px) = (m.cell_w, m.cell_h, m.ascent.round(), m.px);
        let cols = grid.cols;
        let total = preedit_cell_offset(pre.text, pre.text.len());
        if total == 0 {
            return;
        }
        let crow = grid.cursor.row.min(grid.rows.saturating_sub(1));
        let ccol = grid.cursor.col.min(cols.saturating_sub(1));
        let start_col = ccol.min(cols.saturating_sub(total.min(cols)));
        // head cells clipped away when the run is wider than the pane
        let skip = total.saturating_sub(cols);
        let y = oy + crow as f32 * cell_h;
        let cell_x = |cells: usize| ox + (start_col + cells - skip) as f32 * cell_w;

        let vis_w = (total - skip) as f32 * cell_w;
        Self::push_rect(out, cell_x(skip), y, vis_w, cell_h, palette.bg, 1.0);

        let fg_lin = palette.fg.to_linear_f32();
        let mut off = 0usize;
        // start cell of the last spacing char: combining marks (w == 0) land there
        let mut mark_cell = 0usize;
        for c in pre.text.chars() {
            let w = crate::grid::char_width(c);
            if w > 0 {
                mark_cell = off;
            }
            let cell = if w == 0 { mark_cell } else { off };
            // a wide char straddling the clip boundary skips its glyph; the
            // bg and underline still cover the sliver
            if cell >= skip
                && c != ' '
                && let Some(g) = atlas.get(GlyphKey { font: FontId::Content, c, bold: false, italic: false })
            {
                let gx = cell_x(cell);
                out.push(Instance {
                    pos: [gx + g.left, y + ascent - g.top],
                    size: [g.width, g.height],
                    uv_min: g.uv_min,
                    uv_max: g.uv_max,
                    color: [fg_lin[0], fg_lin[1], fg_lin[2], 1.0],
                    kind: if g.color { 3 } else { 1 },
                    shape: [0.0; 3],
                });
            }
            off += w;
        }

        // thin line under the whole run, baseline-anchored like SGR underline
        let t = (cell_h * 0.06).max(1.0);
        let yb = y + (ascent + 0.10 * em_px).round().min(cell_h - t).max(0.0);
        Self::push_rect(out, cell_x(skip), yb, vis_w, t, palette.fg, 0.9);

        match pre.caret {
            // the IME's converting clause reads as "active": double thickness
            Some((s, e)) if s < e => {
                let c0 = preedit_cell_offset(pre.text, s).clamp(skip, total);
                let c1 = preedit_cell_offset(pre.text, e).clamp(skip, total);
                if c1 > c0 {
                    Self::push_rect(out, cell_x(c0), yb, (c1 - c0) as f32 * cell_w, t * 2.0, palette.cursor, 1.0);
                }
            }
            Some((s, _)) => {
                let cc = preedit_cell_offset(pre.text, s).clamp(skip, total);
                Self::push_rect(out, cell_x(cc), y, beam_w, cell_h, palette.cursor, 1.0);
            }
            None => {}
        }
    }

    /// lay out a monospace string at a pixel baseline with optional tracking;
    /// returns the pen end-x
    // a low-level text helper called ~50 times; a params struct would add noise
    // at every call site without making any of them clearer
    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        atlas: &mut GlyphAtlas,
        out: &mut Vec<Instance>,
        font: FontId,
        mut x: f32,
        y_top: f32,
        text: &str,
        rgb: Rgb,
        alpha: f32,
        track: f32,
    ) -> f32 {
        let m = atlas.metrics(font);
        let lin = rgb.to_linear_f32();
        for c in text.chars() {
            if c != ' '
                && let Some(g) = atlas.get(GlyphKey {
                    font,
                    c,
                    bold: false,
                    italic: false,
                }) {
                    out.push(Instance {
                        pos: [x + g.left, y_top + m.ascent - g.top],
                        size: [g.width, g.height],
                        uv_min: g.uv_min,
                        uv_max: g.uv_max,
                        color: [lin[0], lin[1], lin[2], alpha],
                        kind: if g.color { 3 } else { 1 },
                        shape: [0.0; 3],
                    });
                }
            x += m.cell_w + track;
        }
        x
    }

    /// pixel width of a tracked monospace string in the given font
    fn text_w(&self, font: FontId, text: &str, track: f32) -> f32 {
        let m = self.atlas.metrics(font);
        text.chars().count() as f32 * (m.cell_w + track)
    }

    #[allow(non_snake_case)]
    fn build(&mut self, panes: &[PaneView], focused: bool, maximized: bool, bare: bool) -> Vec<Instance> {
        self.atlas.begin_frame();
        // chrome colors come from the active theme's palette
        let INK_0 = self.palette.ink0;
        let INK_3 = self.palette.ink3;
        let INK_4 = self.palette.ink4;
        let RULE = self.palette.rule;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let BG = self.palette.bg;

        let pad = self.pad;
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let hair = self.scale.max(1.0);
        let track = self.track_for(FontId::Chrome);
        // soft blink: the cursor eases out and back in once per period. the raw
        // phase is kept separate so a DECSCUSR blinking-variant cursor can
        // follow the clock even when the configured default is steady
        let blink_phase = motion::cursor_blink(self.start.elapsed().as_secs_f32());
        let blink_on = if self.cursor_blink { blink_phase } else { 1.0 };
        let beam_w = (2.0 * self.scale).round().max(1.0);

        // reuse the persistent scratch buffer: keeps its capacity across frames
        // so a steady-state paint does no heap allocation for the instance list
        let mut out: Vec<Instance> = std::mem::take(&mut self.scratch);
        out.clear();
        out.reserve(self.cols * self.rows + 256);

        // subtle per-theme vertical wash behind everything (bg → bg2); cached and
        // rebuilt only when the size or theme changes (not every frame)
        let grad_key = (self.config.width, self.config.height, self.theme);
        if self.gradient_key != grad_key || self.gradient_cache.is_empty() {
            self.gradient_cache.clear();
            Self::push_vgradient(&mut self.gradient_cache, 0.0, 0.0, w, h, self.palette.bg, self.palette.bg2, 48);
            self.gradient_key = grad_key;
        }
        out.extend_from_slice(&self.gradient_cache);

        // background image over the wash, under everything else: scaled to
        // cover the window and centered, the viewport cropping the overflow
        if let Some((key, rgba, iw, ih)) = &self.bg_image
            && let Some(g) = self.atlas.get_image(*key, rgba, *iw, *ih)
        {
            let scale = (w / g.width).max(h / g.height);
            let (dw, dh) = (g.width * scale, g.height * scale);
            out.push(Instance {
                pos: [((w - dw) / 2.0).round(), ((h - dh) / 2.0).round()],
                size: [dw, dh],
                uv_min: g.uv_min,
                uv_max: g.uv_max,
                color: [0.0, 0.0, 0.0, self.bg_image_alpha],
                kind: 3,
                shape: [0.0; 3],
            });
        }

        // pre-resolve pane grid origins (immutable self) before borrowing the
        // atlas; reuse a persistent buffer like scratch to avoid a per-frame alloc
        let mut pane_info = std::mem::take(&mut self.pane_scratch);
        pane_info.clear();
        pane_info.extend(panes.iter().map(|p| {
            let (ox, oy, _, _) = self.pane_metrics(p.rect);
            (ox, oy, p.focused, p.rect, self.palette.bg)
        }));

        // ---- terminal content (one grid per pane) ----
        // resolve per-profile palettes (theme override + user color overrides)
        // before the immutable borrows below; cached across frames and dropped
        // when the overrides change
        for pv in panes {
            if let Some(id) = pv.theme
                && id != self.theme
                && !self.pane_palettes.iter().any(|(t, _)| *t == id)
            {
                let mut p = Palette::from_theme(id);
                p.apply_overrides(&self.color_overrides);
                self.pane_palettes.push((id, p));
            }
        }
        let cursor_style = self.cursor_style;
        let bold_as_bright = self.bold_as_bright;
        let min_contrast = self.min_contrast;
        let ligatures = self.ligatures;
        {
            let palette = &self.palette;
            let pane_palettes = &self.pane_palettes;
            let theme = self.theme;
            let atlas = &mut self.atlas;
            let find_view = self.find_view.as_ref();
            for (pv, info) in panes.iter().zip(pane_info.iter_mut()) {
                let themed = pv.theme.filter(|&id| id != theme);
                let mut pal = themed
                    .and_then(|id| pane_palettes.iter().find(|(t, _)| *t == id))
                    .map(|(_, p)| p)
                    .unwrap_or(palette);
                // dynamic OSC 4/10/11/12 colors layer over whatever palette
                // the pane paints with (cheap: one ~1KB clone, only for panes
                // whose program actually set colors)
                let dyn_pal;
                if pv.term.colors.any() {
                    dyn_pal = pal.with_dyn(&pv.term.colors);
                    pal = &dyn_pal;
                }
                info.4 = pal.bg;
                if themed.is_some() || pv.term.colors.bg.is_some() {
                    // an overridden pane owns its background: draw_grid skips
                    // default-bg cells, which would otherwise show the window
                    // theme's wash behind this pane's content
                    let (x, y, pw, ph) = pv.rect;
                    Self::push_rect(&mut out, x, y, pw, ph, pal.bg, 1.0);
                }
                let fmatches: &[(usize, usize, usize, bool)] = if pv.focused {
                    find_view.map(|f| f.matches.as_slice()).unwrap_or(&[])
                } else {
                    &[]
                };
                Self::draw_grid(
                    atlas,
                    pal,
                    &mut out,
                    pv.term,
                    info.0,
                    info.1,
                    pv.focused && focused,
                    blink_on,
                    blink_phase,
                    beam_w,
                    cursor_style,
                    pv.sel,
                    pv.link,
                    fmatches,
                    bold_as_bright,
                    min_contrast,
                    ligatures,
                );
                if let Some(pre) = pv.preedit
                    && pv.focused
                    && focused
                {
                    Self::draw_preedit(atlas, pal, &mut out, pv.term, info.0, info.1, pre, beam_w);
                }
            }
        }

        // panes meet on one shared hairline, and focus reads as the other
        // panes settling back (a dim toward their background) rather than as
        // a box drawn around the focused one
        if panes.len() > 1 {
            let focus_now = pane_info.iter().find(|i| i.2).map(|i| i.3);
            if focus_now != self.focus_rect {
                // crossfade only between panes that both still exist; a split,
                // close, or resize moves the rects and just snaps
                self.focus_prev = self
                    .focus_rect
                    .filter(|r| pane_info.iter().any(|i| i.3 == *r))
                    .map(|r| (r, Instant::now()));
                self.focus_rect = focus_now;
            }
            let swap = self.focus_prev.and_then(|(r, t)| motion::fade(Some(t), motion::FOCUS).map(|e| (r, e)));
            if swap.is_none() {
                self.focus_prev = None;
            }
            for info in &pane_info {
                let dim = match swap {
                    Some((_, e)) if info.2 => 1.0 - e,
                    Some((r, e)) if r == info.3 => e,
                    _ if info.2 => 0.0,
                    _ => 1.0,
                };
                if dim > 0.0 {
                    let (x, y, pw, ph) = info.3;
                    Self::push_rect(&mut out, x, y, pw, ph, info.4, tokens::PANE_DIM * dim);
                }
            }
            for (i, a) in pane_info.iter().enumerate() {
                for b in &pane_info[i + 1..] {
                    if let Some((x, y, sw, sh)) = pane_seam(a.3, b.3, pad * 2.0 + 1.0, hair) {
                        Self::push_rect(&mut out, x, y, sw, sh, RULE, 1.0);
                    }
                }
            }
        }
        if self.pane_mode {
            for (_, _, focused, (x, y, pw, ph), _) in &pane_info {
                Self::stroke_rect_a(&mut out, (*x, *y, *pw, *ph), hair, PAPER, if *focused { 0.9 } else { 0.35 });
                let grip_w = tokens::px(tokens::SPACE_2XL, self.scale).min((*pw - hair * 4.0).max(0.0));
                let grip_h = tokens::px(tokens::SPACE_XS, self.scale);
                if grip_w >= hair * 8.0 {
                    let gx = (*x + (*pw - grip_w) / 2.0).round();
                    let gy = *y + tokens::px(tokens::SPACE_XS, self.scale);
                    Self::push_round(&mut out, gx, gy, grip_w, grip_h, grip_h / 2.0, PAPER, if *focused { 0.9 } else { 0.45 });
                }
            }
        }
        let brass = self.palette.ansi_color(3);
        let dot_d = tokens::px(tokens::DOT, self.scale).max(4.0);
        let dot_inset = tokens::px(tokens::SPACE_S, self.scale);
        for (pv, info) in panes.iter().zip(&pane_info) {
            // bell: a warm glow pooled along the pane's inner edge that decays,
            // never a border, so it can't be mistaken for focus
            if pv.flash > 0.0 {
                for (width, a) in [(2.0, 0.22), (6.0, 0.12), (14.0, 0.06)] {
                    Self::push_ring(&mut out, info.3, 0.0, tokens::px(width, self.scale), brass, a * pv.flash);
                }
            }
            // command badge: a corner dot answering "which agents are still
            // working, which finished" at a glance across splits
            if let Some((dot, da)) = self.status_dot(pv.status) {
                let (rx, ry, rw, _rh) = info.3;
                Self::push_round(&mut out, rx + rw - dot_d - dot_inset, ry + dot_inset, dot_d, dot_d, dot_d / 2.0, dot, da);
            }
        }
        if let Some(((x, y, w, h), side)) = self.pane_drop {
            let target = match side {
                PaneDropSide::Left => (x, y, w / 2.0, h),
                PaneDropSide::Right => (x + w / 2.0, y, w / 2.0, h),
                PaneDropSide::Top => (x, y, w, h / 2.0),
                PaneDropSide::Bottom => (x, y + h / 2.0, w, h / 2.0),
            };
            let r = tokens::px(tokens::RADIUS_ROW, self.scale);
            let inset = tokens::px(tokens::SPACE_XS, self.scale);
            let target = (target.0 + inset, target.1 + inset, target.2 - inset * 2.0, target.3 - inset * 2.0);
            Self::push_shape(&mut out, target, [r, 0.0, 0.0], PAPER, 0.14);
            Self::push_ring(&mut out, target, r, hair * 1.5, PAPER, 0.85);
        }
        // last use of pane_info — hand the buffer back so its capacity persists
        self.pane_scratch = pane_info;

        // a torn-off satellite window renders just its pane (the OS supplies the
        // title bar / close / move), so skip all the chrome and overlays below
        if bare {
            return out;
        }

        // ---- plugin dock (Tier-1 widgets) on the right of the content area ----
        if !self.dock.is_empty() {
            self.draw_dock(&mut out, track);
        }

        let s = self.scale;
        let t = |pt: f32| tokens::px(pt, s);
        let tbh = self.title_bar_h;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let text_top = ((tbh - chrome_h) / 2.0).round();
        let icon = t(tokens::ICON);
        let stroke = hair;

        // ---- title bar (flat opaque instrument) ----
        // fill the bar with the content background, not a darker ink, so the bar
        // never reads as a dark band above the content (the two-tone trim
        // still divides chrome from content)
        Self::push_rect(&mut out, 0.0, 0.0, w, tbh, BG, 1.0);
        // two-tone trim under the bar: a brighter seam over a darker shadow line
        // reads as a machined edge between chrome and content (instrument depth)
        Self::push_rect(&mut out, 0.0, tbh - hair * 2.0, w, hair, RULE_2, 1.0);
        Self::push_rect(&mut out, 0.0, tbh - hair, w, hair, RULE, 1.0);

        let wa = self.hover_a(Hot::WindowTabs);
        if wa > 0.0 {
            let (x, _, width, _) = self.window_tabs_rect();
            Self::push_rect(&mut out, x, 0.0, width, tbh - hair * 2.0, INK_3, wa);
        }

        // app-icon badge (the ">_<" mark) + wordmark
        let badge = self.badge_size();
        let badge_y = ((tbh - badge) / 2.0).round();
        Self::push_icon(&mut out, pad, badge_y, badge, 1.0);
        let wx = pad + badge + t(tokens::SPACE_S);
        let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Chrome, wx, text_top, "termie", TEXT_2, 1.0, track);

        // tabs: snapshot to owned data so the atlas can be borrowed mutably
        let tl = self.tab_layout();
        let active_tab = self.active_tab;
        let tab_items: Vec<TabItem> = tl
            .tabs
            .iter()
            .map(|(i, rect, close)| {
                (
                    *i,
                    *rect,
                    *close,
                    self.tabs.get(*i).cloned().unwrap_or_default(),
                    *i == active_tab,
                    self.hover_a(Hot::Tab(*i)).max(self.hover_a(Hot::TabClose(*i))),
                    self.hover_a(Hot::TabClose(*i)),
                    self.tab_status.get(*i).copied().unwrap_or(0),
                    self.tab_colors.get(*i).copied().flatten(),
                )
            })
            .collect();
        let bar_y = hair;
        let bar_h = tbh - hair * 3.0;
        let rail_h = t(tokens::TAB_RAIL).max(hair * 2.0);
        let rail_y = tbh - hair * 2.0 - rail_h;
        let rail_inset = t(tokens::SPACE_S);
        let cw_c = self.atlas.metrics(FontId::Chrome).cell_w + track;
        for (i, rect, close, label, active, ha, close_ha, attn, tint) in &tab_items {
            let (tx, _ty, tw, _th) = *rect;
            if *active {
                Self::push_rect(&mut out, tx, bar_y, tw, bar_h, INK_4, 1.0);
            } else if *ha > 0.0 {
                Self::push_rect(&mut out, tx, bar_y, tw, bar_h, INK_3, *ha);
            }
            // a user-picked tab color washes over the base fill, stronger on
            // the active tab so the tint reads without drowning the label
            if let Some(ci) = tint {
                let c = self.palette.ansi_color(*ci);
                Self::push_rect(&mut out, tx, bar_y, tw, bar_h, c, if *active { 0.28 } else { 0.14 });
            }

            // the close slot is always reserved so a tab never changes width on
            // hover; the close icon shows in it on the active or hovered tab,
            // and a status dot holds it otherwise
            let show_close = (*active || *ha > 0.0) && close.2 > 0.0;
            let label_x = tx + t(tokens::TAB_INSET);
            let label_end = if close.2 > 0.0 { close.0 - t(tokens::SPACE_XS) } else { tx + tw - t(tokens::SPACE_S) };
            let maxc = ((label_end - label_x).max(0.0) / cw_c).floor() as usize;
            // borrow the already-owned snapshot label; only allocate when the
            // label actually needs truncating (the common case fits as-is)
            let truncated;
            let lab: &str = if label.chars().count() > maxc {
                truncated = if maxc > 1 {
                    label.chars().take(maxc - 1).collect::<String>() + "\u{2026}"
                } else {
                    label.chars().take(maxc).collect()
                };
                &truncated
            } else {
                label
            };
            let la = if *active { 1.0 } else { tokens::SECONDARY + (0.85 - tokens::SECONDARY) * ha };
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Chrome, label_x, text_top, lab, TEXT_2, la, track);

            let signal = if *active { None } else { self.tab_signal_phase(*i) };
            if let Some((reach, rail_a, _)) = signal
                && (*attn == 2 || *attn == 4)
            {
                let c = self.palette.ansi_color(if *attn == 4 { 1 } else { 2 });
                let full = tw - rail_inset * 2.0;
                Self::push_round(&mut out, tx + rail_inset, rail_y, full * reach, rail_h, rail_h / 2.0, c, rail_a);
            }
            if show_close {
                let (cx, cy, ccw, cch) = *close;
                if *close_ha > 0.0 {
                    Self::push_round(&mut out, cx, cy, ccw, cch, t(tokens::RADIUS_CHIP), TEXT_2, 0.12 * close_ha);
                }
                let base = if *active { MUTE } else { MUTE.lerp(BG, 0.3) };
                let c = base.lerp(TEXT_2, *close_ha);
                Self::draw_icon(&mut self.atlas, &mut out, Icon::Close, *close, t(8.0), stroke, c, 1.0);
            } else if let Some((dot, da)) = self.status_dot(*attn) {
                let dot_a = signal.map_or(1.0, |(_, _, d)| d);
                if dot_a > 0.0 {
                    let d = t(tokens::DOT).max(4.0);
                    let (cx, _cy, ccw, _cch) = *close;
                    let slot = if ccw > 0.0 { cx + (ccw - d) / 2.0 } else { tx + tw - d - t(tokens::SPACE_S) };
                    Self::push_round(&mut out, slot.round(), ((tbh - d) / 2.0).round(), d, d, d / 2.0, dot, da * dot_a);
                }
            }
        }
        // hairline dividers between neighboring inactive tabs; hidden beside
        // the active or hovered tab so the lit tab stands on clean ground
        let div_h = t(tokens::TAB_DIVIDER);
        for pair in tab_items.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let quiet = 1.0 - a.5.max(b.5);
            if !a.4 && !b.4 && quiet > 0.0 {
                let x = (b.1.0 - hair / 2.0).round();
                Self::push_rect(&mut out, x, ((tbh - div_h) / 2.0).round(), hair, div_h, RULE_2, 0.5 * quiet);
            }
        }

        // active-tab accent rail: a short rounded bar inset from the tab edges
        // that springs from the old tab to the new on a switch
        if let Some(item) = tab_items.iter().find(|t| t.0 == active_tab) {
            let (ax, _, aw, _) = item.1;
            let (ux, uw) = match self
                .tab_slide_p()
                .and_then(|(old, e)| tab_items.iter().find(|t| t.0 == old).map(|o| (o.1, e)))
            {
                Some(((ox, _, ow, _), e)) => (ox + (ax - ox) * e, ow + (aw - ow) * e),
                None => (ax, aw),
            };
            // a tinted active tab colors its rail too
            let rail = item.8.map(|ci| self.palette.ansi_color(ci)).unwrap_or(PAPER);
            Self::push_round(&mut out, ux + rail_inset, rail_y, uw - rail_inset * 2.0, rail_h, rail_h / 2.0, rail, 1.0);
        }
        if let Some(index) = self.tab_drop {
            let x = tab_items
                .get(index)
                .map(|tab| tab.1.0)
                .or_else(|| tab_items.last().map(|tab| tab.1.0 + tab.1.2))
                .unwrap_or(tl.newtab.0);
            let marker_w = t(3.0).max(hair * 2.0);
            let my = t(tokens::SPACE_S);
            Self::push_round(&mut out, x - marker_w / 2.0, my, marker_w, tbh - my * 2.0, marker_w / 2.0, PAPER, 1.0);
        }

        // new-tab '+' and its profile chevron: rounded chips that hover apart
        // so '+' keeps opening a default tab while the chevron drops the list
        for (rect, hot, glyph) in [(tl.newtab, Hot::NewTab, Icon::Plus), (tl.newtab_menu, Hot::NewTabMenu, Icon::Chevron)] {
            let a = self.hover_a(hot);
            let (x, _, bw, _) = rect;
            let chip_h = t(26.0);
            let chip = (x + t(tokens::SPACE_2XS), ((tbh - chip_h) / 2.0).round(), bw - t(tokens::SPACE_XS), chip_h);
            if a > 0.0 {
                Self::push_shape(&mut out, chip, [t(tokens::RADIUS_ROW), 0.0, 0.0], INK_3, a);
            }
            Self::draw_icon(&mut self.atlas, &mut out, glyph, chip, icon, stroke, MUTE.lerp(TEXT_2, a), 1.0);
        }

        // title-bar buttons: split right|left, split top/bottom, pane mode,
        // gear, min, max, close. full-height hover fills, the platform's
        // convention; close goes red
        let close_red = Rgb::new(tokens::CLOSE_RED.0, tokens::CLOSE_RED.1, tokens::CLOSE_RED.2);
        for (c, x0, x1) in self.control_rects() {
            let a = self.hover_a(c);
            let pinned = (c == Hot::Gear && self.settings_open) || (c == Hot::PaneMode && self.pane_mode);
            let fill = if c == Hot::Close { close_red } else { INK_4 };
            let fa = if pinned { 1.0 } else { a };
            if fa > 0.0 {
                Self::push_rect(&mut out, x0, 0.0, x1 - x0, tbh - hair * 2.0, fill, fa);
            }
            let glyph = match c {
                Hot::SplitV => Icon::SplitV,
                Hot::SplitH => Icon::SplitH,
                Hot::PaneMode => Icon::Grid,
                Hot::Gear => Icon::Gear,
                Hot::Minimize => Icon::Minimize,
                Hot::Maximize if maximized => Icon::Restore,
                Hot::Maximize => Icon::Maximize,
                _ => Icon::Close,
            };
            let color = if c == Hot::Close {
                MUTE.lerp(Rgb::new(255, 255, 255), a)
            } else if c == Hot::PaneMode && self.pane_mode {
                PAPER
            } else {
                MUTE.lerp(TEXT_2, fa)
            };
            let size = icon;
            Self::draw_icon(&mut self.atlas, &mut out, glyph, (x0, 0.0, x1 - x0, tbh - hair * 2.0), size, stroke, color, 1.0);
        }

        // ---- status bar (flat) ----
        let sb_y = h - self.status_bar_h;
        Self::push_rect(&mut out, 0.0, sb_y, w, self.status_bar_h, INK_0, 1.0);
        Self::push_rect(&mut out, 0.0, sb_y, w, hair, RULE, 1.0);
        let small = self.atlas.metrics(FontId::Small);
        let st_top = (sb_y + (self.status_bar_h - small.cell_h) / 2.0).round();
        let st_track = self.track_for(FontId::Small);
        let key_track = self.key_track();
        let sw = small.cell_w + st_track;
        let gap = t(tokens::SPACE_L);
        let ig = t(tokens::SPACE_XS) + t(tokens::SPACE_2XS);
        let sicon = t(9.0);
        let sbar = (0.0, sb_y, 0.0, self.status_bar_h);
        let keyed_w = |r: &Self, key: &str, val: &str| r.text_w(FontId::Small, key, key_track) + t(tokens::SPACE_S) + r.text_w(FontId::Small, val, st_track);

        if self.status_size.0 != self.cols || self.status_size.1 != self.rows {
            self.status_size = (self.cols, self.rows, format!("{}\u{00d7}{}", self.cols, self.rows));
        }

        // left cluster: where you are (admin, cwd), or the pane-mode keys
        let left_end = if self.pane_mode {
            let actions = [
                ("DRAG", "dock"),
                ("SHIFT+ARROWS", "resize"),
                ("ARROWS", "focus"),
                ("V/S", "split"),
                ("X", "close"),
                ("Z", "zoom"),
                ("O", "popout"),
            ];
            let done_w = keyed_w(self, "ESC", "done");
            let limit = w * 0.64;
            let mut sx = pad;
            for (key, value) in actions {
                let action_w = keyed_w(self, key, value);
                let action_gap = if sx > pad { gap } else { 0.0 };
                if sx + action_gap + action_w + gap + done_w > limit {
                    continue;
                }
                sx += action_gap;
                sx = self.status_keyed(&mut out, sx, st_top, key, value, PAPER, MUTE);
            }
            if sx > pad {
                sx += gap;
            }
            self.status_keyed(&mut out, sx, st_top, "ESC", "done", PAPER, MUTE)
        } else {
            let mut sx = pad;
            if self.elevated {
                Self::draw_icon(&mut self.atlas, &mut out, Icon::Shield, (sx, sbar.1, sicon, sbar.3), sicon, stroke, PAPER, 1.0);
                sx += sicon + ig;
                sx = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, sx, st_top, "admin", PAPER, 1.0, st_track) + gap;
            }
            let max_cwd = ((w * 0.42) / sw).floor() as usize;
            if let Some(cwd) = self.status.cwd.clone() {
                let shown = truncate_head(&cwd, max_cwd.max(8));
                sx = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, sx, st_top, &shown, TEXT_2, 0.85, st_track) + gap;
            }
            sx - gap
        };

        // right cluster, laid right→left: UPDATE chip, mode, command state.
        // a live notice, link preview, or drag hint takes the mode and
        // command slots for its text
        self.update_hit = None;
        let mut rx = w - pad;
        if let Some(v) = self.update_chip.clone() {
            let label = format!("UPDATE {v}");
            let lw = sicon + ig + self.text_w(FontId::Small, &label, key_track);
            let chip_pad = t(tokens::SPACE_S);
            let chip_h = small.cell_h.min(self.status_bar_h - t(tokens::SPACE_XS));
            let chip = (rx - lw - chip_pad * 2.0, (sb_y + (self.status_bar_h - chip_h) / 2.0).round(), lw + chip_pad * 2.0, chip_h);
            Self::push_shape(&mut out, chip, [t(tokens::RADIUS_CHIP), 0.0, 0.0], PAPER, 0.14);
            let ix = chip.0 + chip_pad;
            Self::draw_icon(&mut self.atlas, &mut out, Icon::Arrow, (ix, sbar.1, sicon, sbar.3), sicon, stroke, PAPER, 1.0);
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, ix + sicon + ig, st_top, &label, PAPER, 1.0, key_track);
            self.update_hit = Some((chip.0, sb_y, chip.2, self.status_bar_h));
            rx = chip.0 - gap;
        }
        let room = (rx - left_end - gap).max(0.0);
        if let Some(n) = self.notice.clone() {
            let maxc = ((room - sicon - ig) / sw).floor().max(0.0) as usize;
            let shown = truncate_tail(&n, maxc);
            let nw = self.text_w(FontId::Small, &shown, st_track);
            let nx = (rx - nw).max(left_end + gap + sicon + ig);
            Self::draw_icon(&mut self.atlas, &mut out, Icon::Bell, (nx - sicon - ig, sbar.1, sicon, sbar.3), sicon, stroke, PAPER, 1.0);
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, nx, st_top, &shown, PAPER, 1.0, st_track);
        } else if self.broadcast {
            let bw = self.text_w(FontId::Small, "BROADCAST", key_track);
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, rx - bw, st_top, "BROADCAST", PAPER, 1.0, key_track);
        } else if self.link_preview.is_some() {
            let full_key = "CTRL+CLICK";
            let short_key = "LINK";
            let value_gap = t(tokens::SPACE_S);
            let useful_chars =
                self.link_preview.as_ref().map(|preview| preview.host_chars.min(24)).unwrap_or(0) as f32;
            if sw <= room {
                let (key, key_w, key_gap) = [full_key, short_key]
                    .into_iter()
                    .map(|k| (k, self.text_w(FontId::Small, k, key_track), value_gap))
                    .find(|(_, kw, kg)| kw + kg + useful_chars * sw <= room)
                    .unwrap_or(("", 0.0, 0.0));
                let maxc = ((room - key_w - key_gap).max(0.0) / sw).floor() as usize;
                if let Some(preview) = self.link_preview.as_mut() {
                    preview.fit(maxc);
                }
                let preview = self.link_preview.as_ref().expect("link preview exists");
                let value = preview.fitted.clone();
                let value_w = preview.fitted_chars as f32 * sw;
                let x = rx - key_w - key_gap - value_w;
                if key.is_empty() {
                    let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, x, st_top, &value, MUTE, 1.0, st_track);
                } else {
                    let _ = self.status_keyed(&mut out, x, st_top, key, &value, PAPER, MUTE);
                }
            }
        } else if let Some(&(key, value)) =
            drag_hint_variants(self.hovered).iter().find(|&&(key, value)| keyed_w(self, key, value) <= room)
        {
            let x = rx - keyed_w(self, key, value);
            let _ = self.status_keyed(&mut out, x, st_top, key, value, PAPER, MUTE);
        } else {
            let (mode, mode_c, mode_a) = if self.pane_mode {
                ("PANE MODE", PAPER, 1.0)
            } else if self.mark_mode {
                ("MARK MODE", PAPER, 1.0)
            } else {
                ("READY", TEXT_2, tokens::KEY_LABEL)
            };
            let mw = self.text_w(FontId::Small, mode, key_track);
            rx -= mw;
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, rx, st_top, mode, mode_c, mode_a, key_track);
            rx -= gap;
            // a resize shows the grid size for a beat; otherwise the focused
            // pane's command: a breathing dot and timer while it runs, then
            // its result and how long it took
            let sizing = self.size_flash.is_some_and(|t| t.elapsed().as_secs_f32() < 1.0);
            if sizing {
                let text = self.status_size.2.clone();
                let tw = self.text_w(FontId::Small, &text, st_track);
                let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, rx - tw, st_top, &text, TEXT_2, 1.0, st_track);
            } else if let Some((text, mark)) = self.command_readout() {
                let tw = self.text_w(FontId::Small, &text, st_track);
                let x = rx - tw;
                if x - sicon - ig > left_end + gap {
                    let (c, a) = match mark {
                        1 => (MUTE, motion::breathe(self.start.elapsed().as_secs_f32())),
                        4 => (self.palette.ansi_color(1), 1.0),
                        _ => (self.palette.ansi_color(2), 1.0),
                    };
                    let mark_rect = (x - sicon - ig, sbar.1, sicon, sbar.3);
                    if mark == 2 {
                        Self::draw_icon(&mut self.atlas, &mut out, Icon::Check, mark_rect, sicon, stroke, c, a);
                    } else {
                        let d = t(tokens::DOT).max(4.0);
                        let dy = (sb_y + (self.status_bar_h - d) / 2.0).round();
                        Self::push_round(&mut out, (mark_rect.0 + (sicon - d) / 2.0).round(), dy, d, d, d / 2.0, c, a);
                    }
                    let tc = if mark == 4 { self.palette.ansi_color(1) } else { MUTE };
                    let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, x, st_top, &text, tc, 1.0, st_track);
                }
            }
        }

        // ---- overlays ---- (bloom in: stamp the open, then scale the whole
        // overlay instance range's alpha by an eased 0→1 so scrim + box + text
        // fade together without threading the factor through each draw fn)
        let overlay_now = self.pane_menu_view.is_some()
            || self.palette_view.is_some()
            || self.market_view.is_some()
            || self.find_view.is_some()
            || self.confirm_view.is_some()
            || self.rename_view.is_some();
        if overlay_now && !self.overlay_shown {
            self.overlay_since = Some(Instant::now());
        }
        self.overlay_shown = overlay_now;
        let overlay_start = out.len();
        if self.pane_menu_view.is_some() {
            self.build_pane_menu(&mut out, track);
        }
        if self.palette_view.is_some() {
            self.build_palette(&mut out, track);
        }
        if self.market_view.is_some() {
            self.build_market(&mut out, track);
        }
        if self.find_view.is_some() {
            self.build_find(&mut out, track);
        }
        if self.confirm_view.is_some() {
            self.build_confirm(&mut out, track);
        }
        if self.rename_view.is_some() {
            self.build_rename(&mut out, track);
        }
        if overlay_now {
            let p = self
                .overlay_since
                .map(|t| motion::ease_out(motion::progress(t, motion::OVERLAY)))
                .unwrap_or(1.0);
            if p < 1.0 {
                bloom(&mut out[overlay_start..], p, w);
            }
        }
        // the settings panel draws last so its scrollable body is the final
        // instance range (clipped via scissor in render); build_settings sets
        // panel_clip when it draws the body
        self.panel_clip = None;
        if self.settings_p > 0.001 {
            self.build_settings(&mut out, track);
        }

        // startup reveal: a dim scrim eases up from the background and the
        // title-rule seam firms once — a quiet settle, not a power-on. purely
        // visual — input is live underneath the whole time. skipped while the
        // settings panel is open so these rects can't land in the scissored
        // panel range (they'd be clipped to it)
        if self.panel_clip.is_none() {
            let t = self.startup_t();
            if t < 1.0 {
                let fade = self.startup_fade();
                if fade > 0.0 {
                    Self::push_rect(&mut out, 0.0, 0.0, w, h, INK_0, fade);
                }
                // a single in-place hairline firms the title-rule seam as the
                // scrim clears, then fades — the chrome's own machined edge
                // reading a touch crisper for a beat, not a laser sweep
                let rise = t * (2.0 - t);
                let settle = 1.0 - (1.0 - rise).powi(3);
                let a = settle * (1.0 - t);
                if a > 0.0 {
                    let ay = self.title_bar_h - hair * 2.0;
                    Self::push_rect(&mut out, 0.0, ay, w, hair, RULE_2, 0.6 * a);
                }
            }
        }

        // the dragged tab or pane rides under the pointer as a small card
        if let Some((pointer_x, pointer_y, label, tag)) = self.drag_preview.clone() {
            let t = |pt: f32| tokens::px(pt, self.scale);
            let edge = t(tokens::SPACE_S);
            let inset = t(tokens::SPACE_M);
            let gap = t(tokens::SPACE_S);
            let key_track = self.key_track();
            let tag_w = self.text_w(FontId::Small, tag, key_track);
            let cw_c = self.atlas.metrics(FontId::Chrome).cell_w + track;
            let room = (w - edge * 2.0 - inset * 2.0 - tag_w - gap).max(cw_c * 4.0);
            let shown = truncate_tail(if label.is_empty() { "untitled" } else { &label }, (room / cw_c).floor().max(4.0) as usize);
            let label_w = self.text_w(FontId::Chrome, &shown, track);
            let box_w = inset * 2.0 + tag_w + gap + label_w;
            let box_h = t(tokens::SPACE_2XL);
            let offset = t(tokens::SPACE_L);
            let box_x = if pointer_x + offset + box_w <= w - edge { pointer_x + offset } else { pointer_x - offset - box_w }
                .clamp(edge, (w - box_w - edge).max(edge))
                .round();
            let box_y = if pointer_y + offset + box_h <= h - edge { pointer_y + offset } else { pointer_y - offset - box_h }
                .clamp(edge, (h - box_h - edge).max(edge))
                .round();
            let card = (box_x, box_y, box_w, box_h);
            let r = t(tokens::RADIUS_ROW + 2.0);
            Self::push_shadow(&mut out, card, r, t(14.0), t(6.0), INK_0, 0.45);
            Self::push_shape(&mut out, card, [r, 0.0, 0.0], INK_3, 0.98);
            Self::push_ring(&mut out, card, r, hair, RULE_2, 1.0);
            let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
            let small_h = self.atlas.metrics(FontId::Small).cell_h;
            let tag_x = box_x + inset;
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Small, tag_x, (box_y + (box_h - small_h) / 2.0).round(), tag, PAPER, 1.0, key_track);
            let _ = Self::draw_text(&mut self.atlas, &mut out, FontId::Chrome, tag_x + tag_w + gap, (box_y + (box_h - chrome_h) / 2.0).round(), &shown, TEXT_2, 1.0, track);
        }

        // latency hud (diagnostic) drawn last so it stays on top
        self.draw_latency_hud(&mut out, track);

        out
    }

    /// restart the power-on reveal; called the instant the window becomes
    /// visible so the whole animation plays in view rather than during gpu init
    pub fn begin_reveal(&mut self) {
        self.reveal_start = Instant::now();
    }

    /// normalized startup-reveal progress: 0 → 1 over motion::STARTUP, then ≥ 1
    fn startup_t(&self) -> f32 {
        (self.reveal_start.elapsed().as_secs_f32() / motion::STARTUP).min(1.0)
    }

    /// dim-overlay alpha for the reveal: 1 → 0 over motion::STARTUP (ease-out)
    fn startup_fade(&self) -> f32 {
        let t = self.startup_t();
        if t >= 1.0 {
            return 0.0;
        }
        let e = 1.0 - t;
        e * e * e
    }

    pub fn startup_fading(&self) -> bool {
        self.reveal_start.elapsed().as_secs_f32() < motion::STARTUP
    }

    #[allow(non_snake_case)]
    fn build_settings(&mut self, out: &mut Vec<Instance>, track: f32) {
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let RULE = self.palette.rule;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        // snapshot Copy values so we can borrow self.atlas freely while drawing
        let sv = self.settings_view;
        let blink = self.cursor_blink;
        let theme = self.theme;
        let cur_name = self.cursor_style_name();
        let font_name = self.font_name();
        let font_size = self.content_pt as i32;
        let pad_px = self.pane_pad_px as i32;
        let opacity_pct = self.opacity_pct();
        let p = self.settings_p;
        let g = self.settings_geom();
        let bh = g.bh;
        let cx = g.content_x;
        let cw = g.content_w;
        let lbl = |y: f32| (y + (bh - chrome_h) / 2.0).round();

        // ── fixed chrome: scrim + panel body + header (drawn unclipped) ──
        // the drawer casts a soft shadow left onto the terminal as it slides in
        let t = |pt| tokens::px(pt, s);
        Self::push_rect(out, 0.0, g.panel_top, self.config.width as f32, g.panel_h, INK_0, 0.32 * p);
        let panel = (g.panel_x, g.panel_top, g.panel_w + t(tokens::SPACE_XL), g.panel_h);
        Self::push_shadow(out, panel, 0.0, t(tokens::SPACE_XL), 0.0, INK_0, 0.45 * p);
        Self::push_rect(out, g.panel_x, g.panel_top, g.panel_w, g.panel_h, INK_1, 1.0);
        Self::push_rect(out, g.panel_x, g.panel_top, hair, g.panel_h, RULE, 1.0); // left edge

        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, g.head_y, "Settings", PAPER, 1.0, track);
        let qa = self.hover_a(Hot::PanelClose);
        if qa > 0.0 {
            Self::push_shape(out, g.close_btn, [t(tokens::RADIUS_CHIP + 2.0), 0.0, 0.0], self.palette.ink3, qa);
        }
        Self::draw_icon(&mut self.atlas, out, Icon::Close, g.close_btn, t(9.0), hair, MUTE.lerp(TEXT_2, qa), 1.0);
        Self::push_rect(out, cx, g.body_top - t(tokens::SPACE_M), cw, hair, RULE, 1.0);

        // ── scrollable body: everything after this index is scissor-clipped ──
        let body_start = out.len() as u32;
        self.panel_clip = Some((
            body_start,
            [g.panel_x, g.body_top, g.panel_w, g.body_bottom - g.body_top],
        ));

        // PLUGINS (top of the panel: installed list with on/off + browse store).
        // stop the header rule short of the browse button so it doesn't run under it
        let plug_rule_w = (g.plugins_btn.0 - cx - t(tokens::SPACE_M)).max(0.0);
        self.section_label(out, cx, g.sec_plugins_y, plug_rule_w, "PLUGINS");
        self.cycle_btn(out, g.plugins_btn, "browse", Hot::OpenPlugins, track);
        if g.plugin_rows.is_empty() {
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.sec_plugins_y + t(tokens::SPACE_2XL)), "no plugins installed", MUTE, 1.0, track);
        } else {
            for (i, (name, on, rect, ry)) in g.plugin_rows.iter().enumerate() {
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(*ry), name, TEXT_2, 1.0, track);
                self.toggle_btn(out, *rect, *on, Hot::PluginToggle(i), track);
            }
        }

        // APPEARANCE
        self.section_label(out, cx, g.sec_app_y, cw, "APPEARANCE");
        self.setting_label(out, cx, g.font_y, bh, "FONT SIZE");
        self.stepper(out, g.font_dec, g.font_inc, &format!("{font_size}"), Hot::FontDec, Hot::FontInc, g.val_w, track);
        self.setting_label(out, cx, g.fontfam_y, bh, "FONT");
        self.cycle_btn(out, g.fontfam_btn, font_name, Hot::FontCycle, track);
        self.setting_label(out, cx, g.pad_y, bh, "PADDING");
        self.stepper(out, g.pad_dec, g.pad_inc, &format!("{pad_px}"), Hot::PadDec, Hot::PadInc, g.val_w, track);
        self.setting_label(out, cx, g.opacity_y, bh, "OPACITY");
        self.stepper(out, g.op_dec, g.op_inc, &format!("{opacity_pct}%"), Hot::OpacityDec, Hot::OpacityInc, g.val_w, track);
        let lh_label = format!("{:.2}", self.content_line_height);
        self.setting_label(out, cx, g.line_height_y, bh, "LINE HEIGHT");
        self.stepper(out, g.lh_dec, g.lh_inc, &lh_label, Hot::LineHeightDec, Hot::LineHeightInc, g.val_w, track);
        let bb = self.bold_as_bright;
        self.setting_label(out, cx, g.boldbright_y, bh, "BOLD AS BRIGHT");
        self.toggle_btn(out, g.boldbright_btn, bb, Hot::BoldBright, track);
        let backdrop_label = if cfg!(windows) {
            "MICA BACKDROP"
        } else {
            "SYSTEM BLUR"
        };
        self.setting_label(out, cx, g.mica_y, bh, backdrop_label);
        self.toggle_btn(out, g.mica_btn, sv.acrylic, Hot::Mica, track);
        self.setting_label(out, cx, g.cursor_y, bh, "CURSOR");
        self.cycle_btn(out, g.cursor_btn, cur_name, Hot::CursorCycle, track);
        self.setting_label(out, cx, g.blink_y, bh, "CURSOR BLINK");
        self.toggle_btn(out, g.blink_btn, blink, Hot::CursorBlink, track);
        self.setting_label(out, cx, g.theme_label_y, bh, "THEME");
        for (i, id) in ThemeId::ALL.into_iter().enumerate() {
            self.theme_chip(out, g.theme_chips[i], id, theme == id && !sv.theme_auto, track);
        }
        self.auto_chip(out, g.theme_chips[ThemeId::ALL.len()], sv.theme_auto, track);

        // BEHAVIOR
        self.section_label(out, cx, g.sec_beh_y, cw, "BEHAVIOR");
        self.setting_label(out, cx, g.scrollback_y, bh, "SCROLLBACK");
        self.stepper(out, g.sb_dec, g.sb_inc, &format!("{}", sv.scrollback), Hot::ScrollbackDec, Hot::ScrollbackInc, g.val_w, track);
        self.setting_label(out, cx, g.copysel_y, bh, "COPY ON SELECT");
        self.toggle_btn(out, g.copysel_btn, sv.copy_on_select, Hot::CopyOnSelect, track);

        // SHELL
        self.section_label(out, cx, g.sec_shell_y, cw, "SHELL");
        self.setting_label(out, cx, g.shell_y, bh, "DEFAULT SHELL");
        self.cycle_btn(out, g.shell_btn, sv.shell_name, Hot::ShellCycle, track);
        self.setting_label(out, cx, g.profile_y, bh, "LOAD PROFILE");
        self.toggle_btn(out, g.profile_btn, sv.load_profile, Hot::LoadProfile, track);
        self.setting_label(out, cx, g.close_y, bh, "CLOSE BUTTON");
        self.cycle_btn(out, g.close_action_btn, sv.close_action_name, Hot::CloseActionCycle, track);
        self.setting_label(out, cx, g.backend_y, bh, "BACKEND");
        self.cycle_btn(out, g.backend_btn, sv.backend_name, Hot::BackendCycle, track);
        // backend can't swap live; hint that it applies next launch
        let (bbx, _, bbw, _) = g.backend_btn;
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let small_track = self.track_for(FontId::Small);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, bbx + bbw + t(tokens::SPACE_M), (g.backend_y + (bh - small_h) / 2.0).round(), "applies on restart", MUTE, 1.0, small_track);

        // KEYBINDINGS (two sub-columns)
        self.section_label(out, cx, g.sec_keys_y, cw, "KEYBINDINGS");
        let keys: [(&str, &str); 12] = [
            ("Ctrl+T", "new tab"),
            ("Ctrl+P", "palette"),
            ("Ctrl+Shift+P", "pane mode"),
            ("Ctrl+Shift+M", "mark mode"),
            ("Ctrl+Shift+E", "split V"),
            ("Ctrl+Shift+O", "split H"),
            ("Ctrl+Shift+W", "close pane"),
            ("Ctrl+Shift+C", "copy"),
            ("Ctrl+Shift+V", "paste"),
            ("Ctrl+Tab", "next tab"),
            ("Ctrl+,", "settings"),
            ("Esc", "close"),
        ];
        let key_row = t(22.0);
        let sub2 = cx + cw / 2.0;
        let desc_dx = t(108.0);
        for (i, (k, d)) in keys.into_iter().enumerate() {
            let (kx, ky) = if i < 6 {
                (cx, g.keys_start_y + i as f32 * key_row)
            } else {
                (sub2, g.keys_start_y + (i - 6) as f32 * key_row)
            };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, kx, ky + (chrome_h - small_h) / 2.0, k, TEXT_2, 1.0, small_track);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, kx + desc_dx, ky, d, MUTE, 1.0, track);
        }

        // ABOUT
        self.section_label(out, cx, g.sec_about_y, cw, "ABOUT");
        let about: [(&str, &str); 3] = [
            ("FONT", font_name),
            ("VERSION", concat!("termie ", env!("CARGO_PKG_VERSION"))),
            ("RENDERER", self.backend_label),
        ];
        let about_dx = t(120.0);
        let key_track = self.key_track();
        for (i, (k, v)) in about.into_iter().enumerate() {
            let ya = g.about_start_y + i as f32 * t(28.0);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, cx, ya + (chrome_h - small_h) / 2.0, k, MUTE, 1.0, key_track);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx + about_dx, ya, v, TEXT_2, 1.0, track);
        }
    }

    /// the right-side plugin dock: a flat instrument panel listing each Tier-1
    /// widget (title in the accent color, then its text lines). drawn in the
    /// content band, to the right of the reflowed panes
    /// draw the input-to-photon latency line bottom-left on a faint backing rect
    fn draw_latency_hud(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(line) = self.latency_hud.clone() else {
            return;
        };
        let s = self.scale;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let pad = (8.0 * s).round();
        let tw = self.text_w(FontId::Chrome, &line, track);
        let (bw, bh) = (tw + pad * 2.0, chrome_h + pad);
        let bx = pad;
        let by = (self.config.height as f32 - bh - pad).max(0.0);
        Self::push_rect(out, bx, by, bw, bh, self.palette.ink0, 0.72);
        Self::stroke_rect(out, (bx, by, bw, bh), s.max(1.0), self.palette.rule2);
        let ty = (by + (bh - chrome_h) / 2.0).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, &line, self.palette.paper, 1.0, track);
    }

    #[allow(non_snake_case)]
    fn draw_dock(&mut self, out: &mut Vec<Instance>, track: f32) {
        let s = self.scale;
        let hair = s.max(1.0);
        let dock_w = self.dock_w();
        let (cx, cy, cw, ch) = self.content_rect();
        // the dock sits just right of the content rect, filling the carved band
        let dx = (cx + cw).round();
        let dw = (dock_w - self.pad).max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        let INK_1 = self.palette.ink1;
        let RULE = self.palette.rule;
        let TEXT_2 = self.palette.text2;
        let MUTE = self.palette.mute;

        // panel ground + a left hairline that reads as the seam to the terminal
        Self::push_rect(out, dx, cy, dw, ch, INK_1, 1.0);
        Self::push_rect(out, dx, cy, hair, ch, RULE, 1.0);

        let pad = tokens::px(tokens::SPACE_M, s);
        let row = (chrome_h + tokens::px(tokens::SPACE_XS + tokens::SPACE_2XS, s)).round();
        let mut y = cy + pad;
        self.dock_hitboxes.clear();
        // snapshot widget data so the atlas can be borrowed mutably while drawing
        let widgets: Vec<DockWidget> = self.dock.clone();
        for (i, w) in widgets.into_iter().enumerate() {
            let DockWidget { title, lines, draw, canvas_h } = w;
            let band_top = if i == 0 { cy } else { y - pad * 0.5 };
            if i > 0 {
                let ry = (y - pad * 0.5).round();
                Self::push_rect(out, dx + pad, ry, dw - pad * 2.0, hair, RULE, 1.0);
            }
            // widget titles read like the settings section heads: small caps
            let small_h = self.atlas.metrics(FontId::Small).cell_h;
            let key_track = self.key_track();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, dx + pad, (y + (chrome_h - small_h) / 2.0).round(), &title.to_uppercase(), MUTE, 1.0, key_track);
            y += row;
            // Tier-2 canvas: paint the plugin's immediate-mode primitives in a
            // bounded box, each clipped so a plugin can't paint outside its widget
            if !draw.is_empty() {
                let avail = (cy + ch - y - pad * 0.5).max(0.0);
                let bh = (canvas_h.unwrap_or(72.0) * s).min(avail);
                if bh >= 1.0 {
                    self.draw_canvas(out, &draw, (dx + pad, y.round(), (dw - pad * 2.0).max(1.0), bh), track);
                    y += bh + pad * 0.5;
                }
            }
            for line in &lines {
                if y > cy + ch - row {
                    break; // clip to dock height; no scroll in v1
                }
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, dx + pad, y.round(), line, TEXT_2, 1.0, track);
                y += row;
            }
            y += pad * 0.5;
            self.dock_hitboxes.push((dx, band_top, dw, (y - band_top).max(0.0)));
        }
    }

    /// paint a Tier-2 draw list into the pixel box (bx,by,bw,bh); each primitive's
    /// normalized coords map into the box and are clipped to it
    fn draw_canvas(&mut self, out: &mut Vec<Instance>, draw: &[DockDraw], rect: (f32, f32, f32, f32), track: f32) {
        let (bx, by, bw, bh) = rect;
        let m = self.atlas.metrics(FontId::Chrome);
        let adv = m.cell_w + track;
        for cmd in draw {
            match cmd {
                DockDraw::Rect { x, y, w, h, color } => {
                    if let Some((rx, ry, rw, rh)) = canvas_rect_px((bx, by, bw, bh), *x, *y, *w, *h) {
                        let c = self.dock_color(color);
                        Self::push_rect(out, rx.round(), ry.round(), rw, rh, c, 1.0);
                    }
                }
                DockDraw::Text { x, y, text, color } => {
                    let tx = bx + x * bw;
                    let ty = (by + y * bh).round();
                    // clip vertically; truncate horizontally to the box width
                    if ty >= by && ty + m.cell_h <= by + bh + 0.5 {
                        let room = ((bx + bw - tx) / adv).floor().max(0.0) as usize;
                        if room > 0 {
                            let clipped: String = text.chars().take(room).collect();
                            let c = self.dock_color(color);
                            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, ty, &clipped, c, 1.0, track);
                        }
                    }
                }
            }
        }
    }

    /// resolve a Tier-2 color spec (a palette role name or a #hex string) to an
    /// Rgb, falling back to the dock body color for anything unrecognized
    fn dock_color(&self, spec: &str) -> Rgb {
        resolve_dock_color(&self.palette, spec)
    }

    /// parse "#rgb" or "#rrggbb" (leading # already stripped) into an Rgb
    fn parse_hex_rgb(hex: &str) -> Option<Rgb> {
        let h = hex.as_bytes();
        let nib = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        };
        match h.len() {
            3 => Some(Rgb::new(nib(h[0])? * 17, nib(h[1])? * 17, nib(h[2])? * 17)),
            6 => Some(Rgb::new(
                nib(h[0])? * 16 + nib(h[1])?,
                nib(h[2])? * 16 + nib(h[3])?,
                nib(h[4])? * 16 + nib(h[5])?,
            )),
            _ => None,
        }
    }

    /// right-click pane context menu: a small panel of pane actions at the click
    #[allow(non_snake_case)]
    /// the shared overlay surface: a layered soft shadow, rounded fill,
    /// hairline edge, and a faint highlight along the top lip
    fn overlay_frame(&self, out: &mut Vec<Instance>, rect: Rect, r: f32) {
        let t = |pt| tokens::px(pt, self.scale);
        let hair = self.scale.max(1.0);
        let p = &self.palette;
        Self::push_shadow(out, rect, r, t(tokens::SPACE_XL), t(tokens::SPACE_S), p.ink0, 0.35);
        Self::push_shadow(out, rect, r, t(tokens::SPACE_2XS), t(1.0), p.ink0, 0.25);
        Self::push_shape(out, rect, [r, 0.0, 0.0], p.ink1, 1.0);
        Self::push_ring(out, rect, r, hair, p.rule2, 1.0);
        let (x, y, w, _) = rect;
        Self::push_rect(out, x + r, y + hair, (w - r * 2.0).max(0.0), hair, Rgb::new(255, 255, 255), 0.06);
    }

    /// a row highlight inset from the overlay edge
    fn row_fill(&self, out: &mut Vec<Instance>, rect: Rect, alpha: f32) {
        let t = |pt| tokens::px(pt, self.scale);
        let (x, y, w, h) = rect;
        let inset = t(tokens::SPACE_XS) + t(tokens::SPACE_2XS);
        Self::push_shape(
            out,
            (x + inset, y + t(1.0), w - inset * 2.0, h - t(2.0)),
            [t(tokens::RADIUS_ROW), 0.0, 0.0],
            self.palette.ink3,
            alpha,
        );
    }

    #[allow(non_snake_case)]
    fn build_pane_menu(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(v) = self.pane_menu_view.as_ref() else {
            return;
        };
        let (mx, my, hovered) = (v.x, v.y, v.hovered);
        // owned so the pane_menu_view borrow ends before the atlas is drawn into
        let items = v.items.clone();
        let accents = v.accents.clone();
        let (bx, by, mw, row_h, pad) = self.pane_menu_geom(mx, my, &items, !accents.is_empty());
        if self.menu_hover.map(|(i, _)| i) != hovered {
            self.menu_hover = hovered.map(|i| (i, Instant::now()));
        }
        let lit = self.menu_hover.map(|(i, t)| (i, motion::ease_out(motion::progress(t, motion::HOVER_IN))));
        let t = |pt| tokens::px(pt, self.scale);
        let TEXT_2 = self.palette.text2;
        let RULE_2 = self.palette.rule2;
        let hair = self.scale.max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let mh = row_h * items.len() as f32 + pad * 2.0;
        self.overlay_frame(out, (bx, by, mw, mh), t(tokens::RADIUS_ROW + 4.0));
        for (i, lbl) in items.iter().enumerate() {
            let ry = by + pad + row_h * i as f32;
            let a = lit.filter(|(li, _)| *li == i).map_or(0.0, |(_, a)| a);
            if a > 0.0 {
                self.row_fill(out, (bx, ry, mw, row_h), a);
            }
            let ty = (ry + (row_h - chrome_h) / 2.0).round();
            let mut lx = bx + pad + t(tokens::SPACE_M) - t(tokens::SPACE_XS);
            // color-menu rows lead with their swatch; "none" gets a hollow ring
            if !accents.is_empty() {
                let d = t(tokens::SPACE_M);
                let sy = (ry + (row_h - d) / 2.0).round();
                match accents.get(i).copied().flatten() {
                    Some(ci) => Self::push_round(out, lx, sy, d, d, d / 2.0, self.palette.ansi_color(ci), 1.0),
                    None => Self::push_ring(out, (lx, sy, d, d), d / 2.0, hair, RULE_2, 1.0),
                }
                lx += d + t(tokens::SPACE_S);
            }
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, lx, ty, lbl, TEXT_2, 0.85 + 0.15 * a, track);
        }
    }

    /// command-palette overlay: search input + filtered action list with each
    /// row's shortcut on the right
    #[allow(non_snake_case)]
    fn build_palette(&mut self, out: &mut Vec<Instance>, track: f32) {
        self.palette_geom = None;
        let Some(pv) = self.palette_view.as_ref() else {
            return;
        };
        let query = pv.query.clone();
        // window the list around the selection so keyboard nav past the ninth
        // row scrolls instead of highlighting off-screen (fonts lists easily
        // exceed nine); clicks map back through `first` to absolute indices
        let total = pv.items.len();
        let first = pv.selected.saturating_sub(8).min(total.saturating_sub(9));
        let items: Vec<(String, String)> = pv
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(9)
            .map(|(i, label)| (label.clone(), pv.hints.get(i).cloned().unwrap_or_default()))
            .collect();
        let selected = pv.selected;
        let scope = pv.scope;
        let INK_0 = self.palette.ink0;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let t = |pt| tokens::px(pt, s);
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let key_track = self.key_track();
        let small_track = self.track_for(FontId::Small);

        Self::push_rect(out, 0.0, 0.0, w, h, INK_0, 0.45);
        let bw = t(tokens::PALETTE_W).min(w - t(tokens::SPACE_2XL) * 2.0);
        let bx = ((w - bw) / 2.0).round();
        let by = (h * 0.15).max(self.title_bar_h + t(tokens::SPACE_L)).round();
        let row_h = chrome_h + t(tokens::SPACE_M) + t(tokens::SPACE_XS);
        let font_preview = scope == "fonts";
        let rows = items.len().max(1) as f32 + 1.0 + f32::from(font_preview);
        let bh = row_h * rows + t(tokens::SPACE_S);
        // remember the box + row pitch + window offset so clicks can land on
        // entries and resolve to absolute list indices
        self.palette_geom = Some(((bx, by, bw, bh), row_h, items.len(), first));
        self.overlay_frame(out, (bx, by, bw, bh), t(tokens::RADIUS_OVERLAY));

        let pad = t(tokens::SPACE_L);
        // search input row: magnifier, the scope as a small-caps tag, the query
        let iy = (by + (row_h - chrome_h) / 2.0).round();
        let icon = t(tokens::ICON + 2.0);
        Self::draw_icon(&mut self.atlas, out, Icon::Search, (bx + pad, by, icon, row_h), icon, hair, MUTE, 1.0);
        let tag = scope.to_ascii_uppercase();
        let tag_x = bx + pad + icon + t(tokens::SPACE_M);
        let qx = Self::draw_text(&mut self.atlas, out, FontId::Small, tag_x, (by + (row_h - small_h) / 2.0).round(), &tag, MUTE, 1.0, key_track)
            + t(tokens::SPACE_M);
        let qend = if query.is_empty() {
            let hint = match scope {
                "tabs" => "switch to a tab",
                "fonts" => "pick a font",
                _ => "type a command",
            };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, qx + t(tokens::SPACE_XS), iy, hint, MUTE, 0.7, track);
            qx
        } else {
            Self::draw_text(&mut self.atlas, out, FontId::Chrome, qx, iy, &query, TEXT_2, 1.0, track)
        };
        Self::push_round(out, qend + t(1.0), iy + t(1.0), t(tokens::SPACE_2XS), chrome_h - t(2.0), t(1.0), PAPER, 1.0);
        Self::push_rect(out, bx + hair, by + row_h, bw - hair * 2.0, hair, RULE_2, 0.7);

        if items.is_empty() {
            let ty = (by + row_h + (row_h - chrome_h) / 2.0).round();
            let noun = match scope {
                "tabs" => "tabs",
                "fonts" => "fonts",
                _ => "commands",
            };
            let msg = format!("No {noun} match \"{}\"", truncate_tail(&query, 32));
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, &msg, MUTE, 1.0, track);
        }
        for (idx, (lbl, hint)) in items.iter().enumerate() {
            let ry = by + row_h * (idx as f32 + 1.0) + t(tokens::SPACE_XS);
            let is_sel = first + idx == selected;
            if is_sel {
                self.row_fill(out, (bx, ry, bw, row_h), 1.0);
            }
            let ty = (ry + (row_h - chrome_h) / 2.0).round();
            let hint_w = if hint.is_empty() { 0.0 } else { self.text_w(FontId::Small, hint, small_track) };
            let room = bw - pad * 2.0 - hint_w - if hint_w > 0.0 { t(tokens::SPACE_L) } else { 0.0 };
            let cw = self.atlas.metrics(FontId::Chrome).cell_w + track;
            let shown = truncate_tail(lbl, (room / cw).floor().max(4.0) as usize);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, &shown, TEXT_2, if is_sel { 1.0 } else { 0.8 }, track);
            if hint_w > 0.0 {
                let hy = (ry + (row_h - small_h) / 2.0).round();
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, bx + bw - pad - hint_w, hy, hint, MUTE, if is_sel { 1.0 } else { 0.8 }, small_track);
            }
        }
        if font_preview {
            let py = by + row_h * (items.len().max(1) as f32 + 1.0) + t(tokens::SPACE_XS);
            Self::push_rect(out, bx + hair, py, bw - hair * 2.0, hair, RULE_2, 0.7);
            let sample = "Aa  0O  1l  {} []  -> !=";
            let sample_w = self.text_w(FontId::Content, sample, 0.0);
            let content_h = self.atlas.metrics(FontId::Content).cell_h;
            let sx = (bx + (bw - sample_w) / 2.0).round();
            let sy = (py + (row_h - content_h) / 2.0).round();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Content, sx, sy, sample, PAPER, 1.0, 0.0);
        }
        // more rows exist above/below the window: a dim position in the corner
        if total > items.len() {
            let hint = format!("{}/{}", selected + 1, total);
            let hw = self.text_w(FontId::Small, &hint, small_track);
            let ty = (by + (row_h - small_h) / 2.0).round();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, bx + bw - pad - hw, ty, &hint, MUTE, 1.0, small_track);
        }
    }

    /// find-in-scrollback overlay: a single search box pinned below the title
    /// bar showing the query and match position; matches are highlighted in the
    /// grid by draw_grid, this only draws the input box
    #[allow(non_snake_case)]
    fn build_find(&mut self, out: &mut Vec<Instance>, track: f32) {
        self.find_btns = None;
        let Some(fv) = self.find_view.as_ref() else {
            return;
        };
        let query = fv.query.clone();
        let count = fv.count;
        let current = fv.current;
        let capped = fv.capped;
        let regex_on = fv.regex_on;
        let bad = fv.bad;
        let INK_1 = self.palette.ink1;
        let TEXT_2 = self.palette.text2;
        let MUTE = self.palette.mute;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let t = |pt| tokens::px(pt, s);
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let small_track = self.track_for(FontId::Small);
        let bw = t(560.0).min(w - t(tokens::SPACE_2XL) * 2.0);
        let bx = ((w - bw) / 2.0).round();
        let by = (self.title_bar_h + t(tokens::SPACE_M)).round();
        let bh = chrome_h + t(tokens::SPACE_L) + t(tokens::SPACE_XS);
        self.overlay_frame(out, (bx, by, bw, bh), t(tokens::RADIUS_ROW + 4.0));
        let pad = t(tokens::SPACE_M);
        let iy = (by + (bh - chrome_h) / 2.0).round();
        // right cluster: regex toggle / prev / next / close buttons, with the
        // counter to their left. rects are remembered so clicks land on them
        let btn = (bh - t(tokens::SPACE_S)).round();
        let bgap = t(tokens::SPACE_2XS);
        let byy = (by + (bh - btn) / 2.0).round();
        let close_r = (bx + bw - t(tokens::SPACE_XS) - btn, byy, btn, btn);
        let next_r = (close_r.0 - btn - bgap, byy, btn, btn);
        let prev_r = (next_r.0 - btn - bgap, byy, btn, btn);
        let rx_r = (prev_r.0 - btn - bgap - t(tokens::SPACE_XS), byy, btn, btn);
        self.find_btns = Some((rx_r, prev_r, next_r, close_r));
        let chip_r = t(tokens::RADIUS_CHIP + 2.0);
        if regex_on {
            Self::push_shape(out, rx_r, [chip_r, 0.0, 0.0], PAPER, 0.9);
        }
        let rx_text = ".*";
        let rw = self.text_w(FontId::Small, rx_text, small_track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, (rx_r.0 + (rx_r.2 - rw) / 2.0).round(), (rx_r.1 + (rx_r.3 - small_h) / 2.0).round(), rx_text, if regex_on { INK_1 } else { MUTE }, 1.0, small_track);
        let icon = t(tokens::ICON - 1.0);
        Self::draw_icon(&mut self.atlas, out, Icon::ChevronUp, prev_r, icon, hair, MUTE, 1.0);
        Self::draw_icon(&mut self.atlas, out, Icon::Chevron, next_r, icon, hair, MUTE, 1.0);
        Self::draw_icon(&mut self.atlas, out, Icon::Close, close_r, t(8.0), hair, MUTE, 1.0);
        let info = if bad {
            "bad pattern".to_string()
        } else if count == 0 {
            if query.is_empty() {
                String::new()
            } else if capped {
                "search incomplete".to_string()
            } else {
                "no matches".to_string()
            }
        } else {
            format!("{}/{}{}", current + 1, count, if capped { "+" } else { "" })
        };
        let iw = if info.is_empty() { 0.0 } else { self.text_w(FontId::Small, &info, small_track) };
        let right_edge = rx_r.0 - t(tokens::SPACE_S);
        let search = t(tokens::ICON + 1.0);
        Self::draw_icon(&mut self.atlas, out, Icon::Search, (bx + pad, by, search, bh), search, hair, MUTE, 1.0);
        let qx = bx + pad + search + t(tokens::SPACE_S);
        // elide the head of an overlong query so the tail being typed stays
        // visible and never collides with the counter or the buttons
        let cw = self.atlas.metrics(FontId::Chrome).cell_w + track;
        let avail = (right_edge - iw - t(tokens::SPACE_M)) - qx;
        let shown = truncate_head(&query, (avail / cw).floor().max(2.0) as usize);
        let qend = Self::draw_text(&mut self.atlas, out, FontId::Chrome, qx, iy, &shown, if bad { self.palette.ansi_color(1) } else { TEXT_2 }, 1.0, track);
        Self::push_round(out, qend + t(1.0), iy + t(1.0), t(tokens::SPACE_2XS), chrome_h - t(2.0), t(1.0), PAPER, 1.0);
        if !info.is_empty() {
            let c = if bad { self.palette.ansi_color(1) } else { MUTE };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, right_edge - iw, (by + (bh - small_h) / 2.0).round(), &info, c, 1.0, small_track);
        }
    }

    /// absolute item index under a point (None outside the item rows); the
    /// visible rows are a window into the full list, so the offset is added back
    pub fn palette_row_at(&self, x: f32, y: f32) -> Option<usize> {
        let ((bx, by, bw, _), row_h, n, first) = self.palette_geom?;
        if x < bx || x >= bx + bw {
            return None;
        }
        // rows start a small gap below the input row's separator
        let rel = y - (by + row_h + tokens::px(tokens::SPACE_XS, self.scale));
        if rel < 0.0 {
            return None;
        }
        let i = (rel / row_h) as usize;
        (i < n).then_some(first + i)
    }

    /// whether a point is inside the open palette's box
    pub fn palette_contains(&self, x: f32, y: f32) -> bool {
        self.palette_geom.map(|(r, ..)| in_rect(x, y, r)).unwrap_or(false)
    }

    /// find-bar button under a point: 0 regex toggle, 1 prev, 2 next, 3 close
    pub fn find_btn_at(&self, x: f32, y: f32) -> Option<u8> {
        let (rx, prev, next, close) = self.find_btns?;
        if in_rect(x, y, rx) {
            Some(0)
        } else if in_rect(x, y, prev) {
            Some(1)
        } else if in_rect(x, y, next) {
            Some(2)
        } else if in_rect(x, y, close) {
            Some(3)
        } else {
            None
        }
    }

    /// modal confirm box: centered panel with the prompt on top and a key hint
    /// below. blocking is enforced by the app's key-capture, not here; shared by
    /// the paste guard and other yes/no prompts
    fn build_confirm(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(cv) = self.confirm_view.as_ref() else {
            return;
        };
        let (prompt, hint) = (cv.prompt.clone(), cv.hint.clone());
        self.dialog(out, track, &prompt, None, &hint);
    }

    /// the tab-rename field: a centered box with the editable name + a caret
    fn build_rename(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(rv) = self.rename_view.as_ref() else {
            return;
        };
        let buf = rv.buf.clone();
        self.dialog(out, track, "Rename tab", Some(&buf), "enter rename  \u{b7}  esc cancel");
    }

    /// a small centered dialog: a title line, an optional text field with a
    /// caret, and a key hint, sized to its content
    fn dialog(&mut self, out: &mut Vec<Instance>, track: f32, title: &str, field: Option<&str>, hint: &str) {
        let s = self.scale;
        let t = |pt| tokens::px(pt, s);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let small_track = self.track_for(FontId::Small);
        let pad = t(tokens::SPACE_XL);
        let field_h = chrome_h + t(tokens::SPACE_M);
        let gap = t(tokens::SPACE_M);
        let content_w = self
            .text_w(FontId::Chrome, title, track)
            .max(self.text_w(FontId::Small, hint, small_track))
            .max(field.map_or(0.0, |f| self.text_w(FontId::Chrome, f, track) + t(tokens::SPACE_L) * 2.0));
        let bw = (content_w + pad * 2.0).clamp(t(300.0), t(520.0).min(w - t(tokens::SPACE_2XL) * 2.0)).round();
        let bh = (pad * 2.0 + chrome_h + gap + small_h + field.map_or(0.0, |_| field_h + gap)).round();
        let bx = ((w - bw) / 2.0).round();
        let by = ((h - bh) / 2.0).round().max(self.title_bar_h + t(tokens::SPACE_M));
        Self::push_rect(out, 0.0, 0.0, w, h, self.palette.ink0, 0.35);
        self.overlay_frame(out, (bx, by, bw, bh), t(tokens::RADIUS_OVERLAY));
        let mut y = by + pad;
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, y, title, self.palette.paper, 1.0, track);
        y += chrome_h + gap;
        if let Some(text) = field {
            let fr = (bx + pad, y, bw - pad * 2.0, field_h);
            Self::push_shape(out, fr, [t(tokens::RADIUS_ROW), 0.0, 0.0], self.palette.ink0, 0.8);
            Self::push_ring(out, fr, t(tokens::RADIUS_ROW), s.max(1.0), self.palette.rule2, 1.0);
            let cw = self.atlas.metrics(FontId::Chrome).cell_w + track;
            let shown = truncate_head(text, ((fr.2 - t(tokens::SPACE_L) * 2.0) / cw).floor().max(2.0) as usize);
            let ty = (y + (field_h - chrome_h) / 2.0).round();
            let end = Self::draw_text(&mut self.atlas, out, FontId::Chrome, fr.0 + t(tokens::SPACE_M), ty, &shown, self.palette.text2, 1.0, track);
            Self::push_round(out, end + t(1.0), ty + t(1.0), t(tokens::SPACE_2XS), chrome_h - t(2.0), t(1.0), self.palette.paper, 1.0);
            y += field_h + gap;
        }
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, bx + pad, y, hint, self.palette.mute, 1.0, small_track);
    }

    /// plugins marketplace overlay: a centered panel listing installed + catalog
    /// plugins, each as name+version with a state tag and a permissions subline,
    /// plus a status footer. modeled on the command palette
    #[allow(non_snake_case)]
    fn build_market(&mut self, out: &mut Vec<Instance>, track: f32) {
        self.market_hits.clear();
        let Some(mv) = self.market_view.as_ref() else {
            return;
        };
        // snapshot so the atlas can be borrowed mutably while drawing
        struct Row { name: String, version: String, desc: String, perms: Vec<String>, installed: bool, enabled: bool }
        let rows: Vec<Row> = mv
            .rows
            .iter()
            .map(|r| Row {
                name: r.name.clone(),
                version: r.version.clone(),
                desc: r.description.clone(),
                perms: r.permissions.clone(),
                installed: r.installed,
                enabled: r.enabled,
            })
            .collect();
        let selected = mv.selected;
        let status = mv.status.clone();
        let loading = mv.loading;
        let fetch_failed = mv.fetch_failed;
        let n_installed = rows.iter().filter(|r| r.installed).count();
        let n_avail = rows.len() - n_installed;

        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let INK_3 = self.palette.ink3;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let char_w = self.text_w(FontId::Chrome, "M", track).max(1.0);

        // full-window page (its own screen under the tab strip), not a popup
        let top = self.title_bar_h;
        let bottom = (h - self.status_bar_h).max(top + 1.0);
        Self::push_rect(out, 0.0, top, w, bottom - top, INK_1, 1.0);
        let t = |pt| tokens::px(pt, s);
        let card_r = t(tokens::RADIUS_ROW + 4.0);
        let chip_r = t(tokens::RADIUS_CHIP);
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let small_track = self.track_for(FontId::Small);
        let key_track = self.key_track();

        // centered content column
        let col_w = (940.0 * s).min(w - 96.0 * s);
        let col_x = ((w - col_w) / 2.0).round();
        let pad = 24.0 * s;
        let line = chrome_h + 6.0 * s; // text line advance within a card

        // ---- header band: title + counts + close, subtitle, rule ----
        let title_y = (top + t(tokens::SPACE_2XL)).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, col_x, title_y, "Plugins", PAPER, 1.0, track);
        let close_sz = (chrome_h + t(tokens::SPACE_S)).round();
        let close_r = (col_x + col_w - close_sz, (title_y + (chrome_h - close_sz) / 2.0).round(), close_sz, close_sz);
        Self::push_shape(out, close_r, [chip_r, 0.0, 0.0], INK_3, 0.6);
        Self::draw_icon(&mut self.atlas, out, Icon::Close, close_r, t(8.0), hair, MUTE, 1.0);
        self.market_hits.push((close_r, MarketHit::Close));
        let count = format!("{n_installed} installed \u{b7} {n_avail} available");
        let count_w = self.text_w(FontId::Small, &count, small_track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, close_r.0 - t(tokens::SPACE_L) - count_w, (title_y + (chrome_h - small_h) / 2.0).round(), &count, MUTE, 1.0, small_track);
        let sub_y = (title_y + chrome_h + t(tokens::SPACE_S)).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, col_x, sub_y, "Browse and manage plugins. Each runs as its own sandboxed process.", MUTE, 1.0, track);
        let rule_y = (sub_y + chrome_h + 14.0 * s).round();
        Self::push_rect(out, col_x, rule_y, col_w, hair, RULE_2, 1.0);

        // ---- body region (scroll-capped cards) ----
        let foot_h = chrome_h + 22.0 * s;
        let body_top = rule_y + 20.0 * s;
        let body_bottom = bottom - foot_h;
        let card_h = (chrome_h * 3.0 + 34.0 * s).round();
        let gap = 12.0 * s;
        let stride = card_h + gap;
        let max_visible = (((body_bottom - body_top + gap) / stride).floor().max(1.0)) as usize;
        let visible = rows.len().clamp(1, max_visible);
        let first = if selected >= visible { selected + 1 - visible } else { 0 };

        // empty / loading / error state, centered in the body
        if rows.is_empty() {
            let (msg, hint) = if loading {
                ("Fetching the catalog\u{2026}", "")
            } else if fetch_failed {
                ("Couldn't reach the plugin catalog.", "If the catalog repo is private, install the GitHub CLI and run `gh auth login`.")
            } else {
                ("No plugins in the catalog yet.", "")
            };
            let my = ((body_top + body_bottom) / 2.0 - chrome_h).round();
            let mw = self.text_w(FontId::Chrome, msg, track);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, col_x + (col_w - mw) / 2.0, my, msg, TEXT_2, 1.0, track);
            if !hint.is_empty() {
                let hw = self.text_w(FontId::Chrome, hint, track);
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, col_x + (col_w - hw) / 2.0, my + line, hint, MUTE, 1.0, track);
            }
        }

        // ---- plugin cards ----
        for vi in 0..visible {
            let idx = first + vi;
            let Some(r) = rows.get(idx) else {
                break;
            };
            let ry = body_top + stride * vi as f32;
            let sel = idx == selected;
            // each card is a rounded surface; the selected one lifts and its
            // edge brightens
            let card = (col_x, ry, col_w, card_h);
            if sel {
                Self::push_shadow(out, card, card_r, t(tokens::SPACE_M), t(tokens::SPACE_XS), INK_0, 0.3);
            }
            Self::push_shape(out, card, [card_r, 0.0, 0.0], if sel { INK_3 } else { INK_0 }, if sel { 1.0 } else { 0.35 });
            Self::push_ring(out, card, card_r, hair, if sel { MUTE } else { RULE_2 }, 1.0);
            let inner = col_x + pad;
            // line 1: name + version (left), action chip (right)
            let l1 = (ry + 13.0 * s).round();
            let name_col = if sel { PAPER } else { TEXT_2 };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, inner, l1, &r.name, name_col, 1.0, track);
            let nw = self.text_w(FontId::Chrome, &r.name, track);
            let ver = format!("v{}", r.version);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, inner + nw + 10.0 * s, l1, &ver, MUTE, 1.0, track);
            let (chip, chip_fill, chip_fg, chip_border) = if !r.installed {
                ("INSTALL", true, INK_0, PAPER)
            } else if r.enabled {
                ("ENABLED", false, PAPER, PAPER)
            } else {
                ("DISABLED", false, MUTE, RULE_2)
            };
            let mark = if r.installed && r.enabled { t(tokens::ICON - 1.0) + t(tokens::SPACE_XS) } else { 0.0 };
            let cw_chip = self.text_w(FontId::Small, chip, key_track) + mark;
            let chip_w = cw_chip + t(tokens::SPACE_L) + t(tokens::SPACE_XS);
            let chip_x = col_x + col_w - pad - chip_w;
            let chip_h = small_h + t(tokens::SPACE_S);
            let chip_y = (l1 + (chrome_h - chip_h) / 2.0).round();
            let chip_rect = (chip_x, chip_y, chip_w, chip_h);
            self.market_hits.push((chip_rect, MarketHit::Chip(idx)));
            if chip_fill {
                Self::push_shape(out, chip_rect, [chip_r, 0.0, 0.0], PAPER, 1.0);
            } else {
                Self::push_ring(out, chip_rect, chip_r, hair, chip_border, 1.0);
            }
            let cx = chip_x + (chip_w - cw_chip) / 2.0;
            if mark > 0.0 {
                Self::draw_icon(&mut self.atlas, out, Icon::Check, (cx, chip_y, t(tokens::ICON - 1.0), chip_h), t(tokens::ICON - 1.0), hair, chip_fg, 1.0);
            }
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, cx + mark, (chip_y + (chip_h - small_h) / 2.0).round(), chip, chip_fg, 1.0, key_track);
            // line 2: description (truncated to the card width)
            let l2 = (l1 + line).round();
            let dmax = ((col_w - 2.0 * pad) / char_w).floor() as usize;
            let desc = if r.desc.is_empty() {
                "no description".to_string()
            } else if r.desc.chars().count() > dmax {
                let mut t: String = r.desc.chars().take(dmax.saturating_sub(1)).collect();
                t.push('\u{2026}');
                t
            } else {
                r.desc.clone()
            };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, inner, l2, &desc, MUTE, 1.0, track);
            // line 3: permission badges, or a note when there are none
            let l3 = (l2 + line).round();
            if r.perms.is_empty() {
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, inner, (l3 + (chrome_h - small_h) / 2.0).round(), "no special permissions", MUTE, 0.7, small_track);
            } else {
                let mut px = inner;
                let ph = small_h + t(tokens::SPACE_XS);
                for p in &r.perms {
                    let pw = self.text_w(FontId::Small, p, small_track) + t(tokens::SPACE_M);
                    if px + pw > col_x + col_w - pad {
                        break;
                    }
                    let pr = (px, (l3 + (chrome_h - ph) / 2.0).round(), pw, ph);
                    Self::push_shape(out, pr, [chip_r, 0.0, 0.0], INK_1, 0.8);
                    Self::push_ring(out, pr, chip_r, hair, RULE_2, 1.0);
                    let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, px + t(6.0), (pr.1 + (ph - small_h) / 2.0).round(), p, TEXT_2, 1.0, small_track);
                    px += pw + t(6.0);
                }
            }
            self.market_hits.push(((col_x, ry, col_w, card_h), MarketHit::Card(idx)));
        }

        // ---- footer: status (left) + key hints (right) ----
        Self::push_rect(out, col_x, body_bottom + 6.0 * s, col_w, hair, RULE_2, 1.0);
        let fy = (body_bottom + 6.0 * s + (foot_h - chrome_h) / 2.0).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, col_x, fy, &status, MUTE, 1.0, track);
        let hints = "\u{2191}\u{2193} select  \u{b7}  enter install/toggle  \u{b7}  r remove  \u{b7}  esc close";
        let hints_w = self.text_w(FontId::Small, hints, small_track);
        let status_w = self.text_w(FontId::Chrome, &status, track);
        if hints_w + status_w + t(tokens::SPACE_XL) < col_w {
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, col_x + col_w - hints_w, (fy + (chrome_h - small_h) / 2.0).round(), hints, MUTE, 0.8, small_track);
        }
    }

    /// `LABEL ─────────` section header: small caps, then a rule filling the
    /// remaining width
    fn section_label(&mut self, out: &mut Vec<Instance>, x: f32, y: f32, w: f32, text: &str) {
        let key_track = self.key_track();
        let tw = self.text_w(FontId::Small, text, key_track);
        let (chrome_h, small_h) = (self.atlas.metrics(FontId::Chrome).cell_h, self.atlas.metrics(FontId::Small).cell_h);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, x, (y + (chrome_h - small_h) / 2.0).round(), text, self.palette.mute, 1.0, key_track);
        let hair = self.scale.max(1.0);
        let ry = (y + chrome_h / 2.0).round();
        let rx = x + tw + tokens::px(tokens::SPACE_M, self.scale);
        Self::push_rect(out, rx, ry, (x + w - rx).max(0.0), hair, self.palette.rule, 1.0);
    }

    /// a setting's name: small caps in the Small face, centered on its row
    fn setting_label(&mut self, out: &mut Vec<Instance>, x: f32, row_y: f32, bh: f32, text: &str) {
        let small_h = self.atlas.metrics(FontId::Small).cell_h;
        let key_track = self.key_track();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Small, x, (row_y + (bh - small_h) / 2.0).round(), text, self.palette.mute, 1.0, key_track);
    }

    /// a rounded field that cycles its value on click, with a chevron
    fn cycle_btn(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), text: &str, hot: Hot, track: f32) {
        let t = |pt| tokens::px(pt, self.scale);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let (bx, by, bw, bh) = rect;
        let he = self.hover_a(hot);
        let r = t(tokens::RADIUS_ROW);
        Self::push_shape(out, rect, [r, 0.0, 0.0], self.palette.ink0, 0.6);
        if he > 0.0 {
            Self::push_shape(out, rect, [r, 0.0, 0.0], self.palette.ink3, he);
        }
        Self::push_ring(out, rect, r, self.scale.max(1.0), self.palette.rule2.lerp(self.palette.mute, he * 0.5), 1.0);
        // cycling fields carry a chevron; a plain action button (browse) doesn't
        let icon = if hot == Hot::OpenPlugins { 0.0 } else { t(tokens::ICON - 2.0) };
        let room = bw - t(tokens::SPACE_M) * 2.0 - icon - t(tokens::SPACE_XS);
        let cw = self.atlas.metrics(FontId::Chrome).cell_w + track;
        let shown = truncate_tail(text, (room / cw).floor().max(2.0) as usize);
        let tx = if icon > 0.0 { bx + t(tokens::SPACE_M) } else { (bx + (bw - self.text_w(FontId::Chrome, &shown, track)) / 2.0).round() };
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, (by + (bh - chrome_h) / 2.0).round(), &shown, self.palette.text2, 1.0, track);
        if icon == 0.0 {
            return;
        }
        let ix = bx + bw - t(tokens::SPACE_M) - icon;
        Self::draw_icon(&mut self.atlas, out, Icon::Chevron, (ix, by, icon, bh), icon, self.scale.max(1.0), self.palette.mute.lerp(self.palette.text2, he), 1.0);
    }

    /// an on/off switch at the leading edge of its row rect
    fn toggle_btn(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), on: bool, hot: Hot, _track: f32) {
        let t = |pt| tokens::px(pt, self.scale);
        let (bx, by, _bw, bh) = rect;
        let he = self.hover_a(hot);
        let (sw, sh) = (t(32.0), t(18.0));
        let track_r = (bx, (by + (bh - sh) / 2.0).round(), sw, sh);
        let knob = sh - t(tokens::SPACE_XS);
        let kx = if on { track_r.0 + sw - knob - t(tokens::SPACE_2XS) } else { track_r.0 + t(tokens::SPACE_2XS) };
        let ky = track_r.1 + t(tokens::SPACE_2XS);
        if on {
            Self::push_shape(out, track_r, [sh / 2.0, 0.0, 0.0], self.palette.paper, 0.9 + 0.1 * he);
            Self::push_round(out, kx, ky, knob, knob, knob / 2.0, self.palette.ink0, 1.0);
        } else {
            Self::push_shape(out, track_r, [sh / 2.0, 0.0, 0.0], self.palette.ink3, 1.0);
            Self::push_ring(out, track_r, sh / 2.0, self.scale.max(1.0), self.palette.rule2.lerp(self.palette.mute, he), 1.0);
            Self::push_round(out, kx, ky, knob, knob, knob / 2.0, self.palette.mute.lerp(self.palette.text2, he), 1.0);
        }
    }

    /// a theme chip: name on top + a live swatch strip of that theme's palette,
    /// filled when active, outlined otherwise, lit on hover
    /// the trailing chip in the theme grid: follow the windows light/dark
    /// setting. same frame as a theme chip, but the swatch strip shows a
    /// dark half and a light half instead of one palette
    fn auto_chip(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), active: bool, track: f32) {
        let s = self.scale;
        let cw = self.atlas.metrics(FontId::Chrome).cell_w;
        let (bx, by, bw, bh) = rect;
        let he = self.hover_a(Hot::ThemeAuto);
        self.chip_frame(out, rect, active, he);
        let name = "auto";
        let maxc = (((bw - 8.0 * s) / (cw + track)).floor().max(1.0)) as usize;
        let t: String = name.chars().take(maxc).collect();
        let tw = self.text_w(FontId::Chrome, &t, track);
        let col = if active { self.palette.paper } else { self.palette.text2.lerp(self.palette.paper, he) };
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + (bw - tw) / 2.0, (by + 6.0 * s).round(), &t, col, 1.0, track);
        // half-and-half swatch: the dark default's bg beside the light one's
        let dark = Palette::from_theme(ThemeId::Instrument);
        let light = Palette::from_theme(ThemeId::Paper);
        let gap = 3.0 * s;
        let inset = 9.0 * s;
        let strip_w = (bw - inset * 2.0).max(1.0);
        let half_w = ((strip_w - gap) / 2.0).max(1.0);
        let sh = 11.0 * s;
        let sy = by + bh - sh - 6.0 * s;
        let r = tokens::px(2.0, s);
        Self::push_round(out, bx + inset, sy, half_w, sh, r, dark.bg, 1.0);
        Self::push_round(out, bx + inset + half_w + gap, sy, half_w, sh, r, light.bg, 1.0);
        Self::push_ring(out, (bx + inset, sy, strip_w, sh), r, 1.0, self.palette.rule2, 1.0);
    }

    fn theme_chip(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), id: ThemeId, active: bool, track: f32) {
        let s = self.scale;
        let cw = self.atlas.metrics(FontId::Chrome).cell_w;
        let (bx, by, bw, bh) = rect;
        let hot = Hot::ThemeSet(id);
        let he = self.hover_a(hot);
        self.chip_frame(out, rect, active, he);
        // theme name (truncated to fit), centered at the top
        let name = id.name();
        let maxc = (((bw - 8.0 * s) / (cw + track)).floor().max(1.0)) as usize;
        let t: String = if name.chars().count() > maxc {
            name.chars().take(maxc).collect()
        } else {
            name.to_string()
        };
        let tw = self.text_w(FontId::Chrome, &t, track);
        let col = if active { self.palette.paper } else { self.palette.text2.lerp(self.palette.paper, he) };
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + (bw - tw) / 2.0, (by + 6.0 * s).round(), &t, col, 1.0, track);

        // swatch strip: bg · fg · accent · ansi red · ansi blue
        let pal = Palette::from_theme(id);
        let sw = [
            pal.bg,
            pal.fg,
            pal.paper,
            pal.resolve_fg(Color::Indexed(1)),
            pal.resolve_fg(Color::Indexed(4)),
        ];
        let n = sw.len() as f32;
        let gap = 3.0 * s;
        let inset = 9.0 * s;
        let strip_w = (bw - inset * 2.0).max(1.0);
        let cell_w = ((strip_w - gap * (n - 1.0)) / n).max(1.0);
        let sh = 11.0 * s;
        let sy = by + bh - sh - 6.0 * s;
        let r = tokens::px(2.0, s);
        for (i, c) in sw.into_iter().enumerate() {
            let sx = bx + inset + i as f32 * (cell_w + gap);
            Self::push_round(out, sx, sy, cell_w, sh, r, c, 1.0);
        }
    }

    /// a theme card's surface: rounded, lit on hover, ringed bright when active
    fn chip_frame(&self, out: &mut Vec<Instance>, rect: Rect, active: bool, he: f32) {
        let r = tokens::px(tokens::RADIUS_ROW, self.scale);
        let hair = self.scale.max(1.0);
        Self::push_shape(out, rect, [r, 0.0, 0.0], self.palette.ink0, 0.6);
        if he > 0.0 || active {
            Self::push_shape(out, rect, [r, 0.0, 0.0], self.palette.ink3, if active { 1.0 } else { he });
        }
        if active {
            Self::push_ring(out, rect, r, hair * 1.5, self.palette.paper, 1.0);
        } else {
            Self::push_ring(out, rect, r, hair, self.palette.rule2, 1.0);
        }
    }

    /// draw a [−] value [+] stepper given the two button rects
    #[allow(clippy::too_many_arguments, non_snake_case)]
    fn stepper(
        &mut self,
        out: &mut Vec<Instance>,
        dec: (f32, f32, f32, f32),
        inc: (f32, f32, f32, f32),
        val: &str,
        hot_dec: Hot,
        hot_inc: Hot,
        val_w: f32,
        track: f32,
    ) {
        let INK_0 = self.palette.ink0;
        let RULE_2 = self.palette.rule2;
        let TEXT_2 = self.palette.text2;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        let r = tokens::px(tokens::RADIUS_ROW, self.scale);
        let hair = self.scale.max(1.0);
        // one rounded well holding [−] value [+]; each button lights on hover
        let (dx, dy, dw, dh) = dec;
        let well = (dx, dy, inc.0 + inc.2 - dx, dh);
        Self::push_shape(out, well, [r, 0.0, 0.0], INK_0, 0.6);
        Self::push_ring(out, well, r, hair, RULE_2, 1.0);
        for (rect, icon, hot) in [(dec, Icon::Minimize, hot_dec), (inc, Icon::Plus, hot_inc)] {
            let he = self.hover_a(hot);
            if he > 0.0 {
                let inset = tokens::px(tokens::SPACE_2XS, self.scale);
                let lit = (rect.0 + inset, rect.1 + inset, rect.2 - inset * 2.0, rect.3 - inset * 2.0);
                Self::push_shape(out, lit, [r - inset, 0.0, 0.0], self.palette.ink3, he);
            }
            let size = tokens::px(tokens::ICON - 2.0, self.scale);
            Self::draw_icon(&mut self.atlas, out, icon, rect, size, hair, self.palette.mute.lerp(TEXT_2, 0.4 + 0.6 * he), 1.0);
        }
        // value centered between the buttons
        let vx = (dx + dw + (val_w - self.text_w(FontId::Chrome, val, track)) / 2.0).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, vx, (dy + (dh - chrome_h) / 2.0).round(), val, TEXT_2, 1.0, track);
    }

    /// a status-bar "KEY value" pair: tracked small-caps key, then its value
    #[allow(clippy::too_many_arguments)]
    fn status_keyed(&mut self, out: &mut Vec<Instance>, x: f32, y_top: f32, key: &str, val: &str, key_c: Rgb, val_c: Rgb) -> f32 {
        let (key_track, track) = (self.key_track(), self.track_for(FontId::Small));
        let px = Self::draw_text(&mut self.atlas, out, FontId::Small, x, y_top, key, key_c, 1.0, key_track)
            + tokens::px(tokens::SPACE_S, self.scale);
        Self::draw_text(&mut self.atlas, out, FontId::Small, px, y_top, val, val_c, 1.0, track)
    }

    /// color and alpha of a command/attention dot: 1 running (breathing),
    /// 2 done, 3 bell, 4 failed; None for 0
    fn status_dot(&self, status: u8) -> Option<(Rgb, f32)> {
        match status {
            0 => None,
            1 => Some((self.palette.mute, motion::breathe(self.start.elapsed().as_secs_f32()))),
            2 => Some((self.palette.ansi_color(2), 1.0)),
            4 => Some((self.palette.ansi_color(1), 1.0)),
            _ => Some((self.palette.paper, 1.0)),
        }
    }

    /// the focused pane's command readout and its mark (1 running, 2 ok,
    /// 4 failed), reformatted only when the shown text would change
    fn command_readout(&mut self) -> Option<(String, u8)> {
        let (key, mark) = match self.status.command {
            CommandState::Idle => return None,
            CommandState::Running(since) => (since.elapsed().as_secs(), 1),
            CommandState::Finished { code, took } => {
                let failed = code.is_some_and(|c| c != 0);
                (took.as_millis() as u64 / 100 + (1 << 40) + if failed { 1 << 41 } else { 0 }, if failed { 4 } else { 2 })
            }
        };
        if self.status_cmd.0 != key {
            let text = match self.status.command {
                CommandState::Running(since) => format!("running {}", fmt_duration(since.elapsed())),
                CommandState::Finished { code: Some(c), took } if c != 0 => format!("exit {c} \u{00b7} {}", fmt_duration(took)),
                CommandState::Finished { took, .. } => fmt_duration(took),
                CommandState::Idle => String::new(),
            };
            self.status_cmd = (key, text);
        }
        Some((self.status_cmd.1.clone(), mark))
    }

    pub fn render(&mut self, panes: &[PaneView], focused: bool, maximized: bool, bare: bool) -> Result<()> {
        // a device-lost callback fired since the last frame: rebuild the gpu now,
        // before building/uploading anything against the dead device
        if self.device_lost.swap(false, Ordering::SeqCst)
            && let Some(w) = self.window.clone()
        {
            self.recreate(w)?;
        }
        let instances = self.build(panes, focused, maximized, bare);
        self.upload_atlas();

        let needed = instances.len() as u64;
        if needed > self.instance_capacity {
            self.instance_capacity = (needed * 2).next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }
        self.upload_screen_uniform();

        use wgpu::CurrentSurfaceTexture as Cst;
        if self.surface.is_none() {
            return Ok(());
        }
        // get_current_texture returns an owned enum, so the surface borrow ends
        // before each arm runs — letting the Lost arm call the &mut self recreate
        let frame = match self.surface.as_ref().unwrap().get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => Some(f),
            Cst::Outdated => {
                // stale swapchain (resize / format change): cheap reconfigure
                let s = self.surface.as_ref().unwrap();
                s.configure(&self.device, &self.config);
                match s.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => Some(f),
                    _ => None,
                }
            }
            Cst::Lost => {
                // swapchain/device lost: rebuild the gpu and SKIP this frame. the
                // per-frame uploads above already ran against the now-dead device,
                // so drawing now would use the fresh empty buffers (blank frame —
                // and an over-capacity instance range could trip a validation
                // abort). recreate clears device_lost + marks the atlas dirty, so
                // the next frame repaints correctly against the new device
                if let Some(w) = self.window.clone() {
                    self.recreate(w)?;
                }
                return Ok(());
            }
            _ => None,
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // premultiplied clear: the empty field is the glassy warm-dark
        let bg = self.palette.bg.to_linear_f32();
        let a = self.bg_alpha as f64;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cell-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64 * a,
                            g: bg[1] as f64 * a,
                            b: bg[2] as f64 * a,
                            a,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                // clamp the drawn range to the live buffer capacity so a count vs
                // buffer-size desync can never feed an oversized range to wgpu
                // (which, with panic=abort, would take the process down)
                let total = (instances.len() as u32).min(self.instance_capacity as u32);
                match self.panel_clip {
                    // everything up to `start` is the terminal/chrome + fixed panel
                    // (full surface); the body after it is clipped to the panel
                    Some((start, clip)) if start < total => {
                        pass.draw(0..6, 0..start);
                        let (w, h) = (self.config.width as f32, self.config.height as f32);
                        let sx = clip[0].clamp(0.0, w);
                        let sy = clip[1].clamp(0.0, h);
                        let sw = ((clip[0] + clip[2]).min(w) - sx).max(0.0);
                        let sh = ((clip[1] + clip[3]).min(h) - sy).max(0.0);
                        if sw >= 1.0 && sh >= 1.0 {
                            pass.set_scissor_rect(sx as u32, sy as u32, sw as u32, sh as u32);
                            pass.draw(0..6, start..total);
                            pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
                        } else {
                            pass.draw(0..6, start..total);
                        }
                    }
                    _ => pass.draw(0..6, 0..total),
                }
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        // hand the buffer back so its capacity is reused next frame
        self.scratch = instances;
        Ok(())
    }

    fn upload_screen_uniform(&mut self) {
        let screen = [self.config.width, self.config.height];
        if self.uploaded_screen == Some(screen) {
            return;
        }
        let uniforms = Uniforms {
            screen: [screen[0] as f32, screen[1] as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        self.uploaded_screen = Some(screen);
    }

    pub fn shutdown(self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }

    /// dev capture harness: a surfaceless renderer that draws into an offscreen
    /// texture so the full chrome (tab strip, buttons, menus, panes) can be
    /// rendered to a PNG without a window. compiled out of release
    #[cfg(any(test, debug_assertions))]
    #[cfg_attr(all(test, not(debug_assertions)), allow(dead_code))]
    pub fn new_headless(width: u32, height: u32, content_pt: f32, chrome_pt: f32, scale: f32) -> Renderer {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("no gpu adapter for headless render");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-headless"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .expect("headless device");
        Self::new_headless_on(device, queue, width, height, content_pt, chrome_pt, scale)
    }

    /// headless renderer on an existing device, so tests can share one device
    /// across the whole binary instead of enumerating adapters per test
    #[cfg(any(test, debug_assertions))]
    fn new_headless_on(
        device: wgpu::Device,
        queue: wgpu::Queue,
        width: u32,
        height: u32,
        content_pt: f32,
        chrome_pt: f32,
        scale: f32,
    ) -> Renderer {
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 1,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
        };
        let atlas = GlyphAtlas::new(content_pt, chrome_pt, scale, None, 1.32);
        let mut r = Self::from_parts(
            device,
            queue,
            None,
            format,
            config,
            atlas,
            scale,
            content_pt,
            chrome_pt,
            false,
        );

        // offscreen target (COPY_SRC for readback) + a row-aligned readback buffer
        let target = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let bpr = padded_bytes_per_row(width);
        let readback = r.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: bpr as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        r.offscreen = Some((target, readback));
        r
    }

    /// dev capture harness: render the scene into the offscreen target and write
    /// it to `path` as a PNG. only valid on a renderer from `new_headless`
    #[cfg(any(test, debug_assertions))]
    pub fn render_png(
        &mut self,
        panes: &[PaneView],
        focused: bool,
        maximized: bool,
        bare: bool,
        path: &str,
    ) -> std::io::Result<()> {
        use std::io::Error;
        let instances = self.build(panes, focused, maximized, bare);
        self.upload_atlas();

        let needed = instances.len() as u64;
        if needed > self.instance_capacity {
            self.instance_capacity = (needed * 2).next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }
        self.upload_screen_uniform();

        let (width, height) = (self.config.width, self.config.height);
        let Some((target, readback)) = self.offscreen.as_ref() else {
            return Err(Error::other("render_png needs a headless renderer"));
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = self.palette.bg.to_linear_f32();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("offscreen-encoder") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                // mirror the live render's settings-body scissor so a scrolled
                // settings panel clips in the capture instead of overflowing
                let total = instances.len() as u32;
                match self.panel_clip {
                    Some((start, clip)) if start < total => {
                        pass.draw(0..6, 0..start);
                        let (w, h) = (self.config.width as f32, self.config.height as f32);
                        let sx = clip[0].clamp(0.0, w);
                        let sy = clip[1].clamp(0.0, h);
                        let sw = ((clip[0] + clip[2]).min(w) - sx).max(0.0);
                        let sh = ((clip[1] + clip[3]).min(h) - sy).max(0.0);
                        if sw >= 1.0 && sh >= 1.0 {
                            pass.set_scissor_rect(sx as u32, sy as u32, sw as u32, sh as u32);
                            pass.draw(0..6, start..total);
                            pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
                        } else {
                            pass.draw(0..6, start..total);
                        }
                    }
                    _ => pass.draw(0..6, 0..total),
                }
            }
        }
        let bpr = padded_bytes_per_row(width);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        // map the readback buffer (block until the gpu finishes) and un-pad rows
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range().expect("readback buffer did not map");
        let row = (width * 4) as usize;
        let mut rgba = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * bpr as usize;
            rgba.extend_from_slice(&data[start..start + row]);
        }
        drop(data);
        readback.unmap();
        self.scratch = instances;
        crate::render::preview::write_png(path, width, height, &rgba)
    }
}

/// resolve a Tier-2 color spec (a palette role name or a #hex string) to an Rgb,
/// falling back to the dock body color for anything unrecognized; a pure mirror
/// of dock_color so the role table stays unit-testable
fn resolve_dock_color(p: &Palette, spec: &str) -> Rgb {
    // a raw hex snaps to the nearest color the theme already has, so a plugin
    // stays on-palette in every theme instead of carrying its own colors
    if let Some(hex) = spec.strip_prefix('#')
        && let Some(c) = Renderer::parse_hex_rgb(hex)
    {
        return nearest_theme_color(p, c);
    }
    match spec {
        "paper" | "accent" => p.paper,
        "text" | "text2" => p.text2,
        "mute" => p.mute,
        "rule" => p.rule,
        "rule2" => p.rule2,
        "ink" | "ink1" => p.ink1,
        "ink0" => p.ink0,
        "ink3" => p.ink3,
        "ink4" => p.ink4,
        "err" => p.ansi_color(1),
        "ok" => p.ansi_color(2),
        "warn" => p.ansi_color(3),
        "info" => p.ansi_color(4),
        s => s
            .strip_prefix("ansi")
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|&n| n < 16)
            .map_or(p.text2, |n| p.ansi_color(n)),
    }
}

/// the theme color perceptually closest to `c` (redmean-weighted rgb distance)
/// among its chrome tokens and 16 ansi colors
fn nearest_theme_color(p: &Palette, c: Rgb) -> Rgb {
    let dist = |a: Rgb, b: Rgb| {
        let rm = (a.r as f32 + b.r as f32) / 2.0;
        let (dr, dg, db) = (a.r as f32 - b.r as f32, a.g as f32 - b.g as f32, a.b as f32 - b.b as f32);
        (2.0 + rm / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - rm) / 256.0) * db * db
    };
    [p.paper, p.text2, p.mute, p.rule, p.rule2, p.ink0, p.ink1, p.ink3, p.ink4]
        .into_iter()
        .chain((0..16).map(|n| p.ansi_color(n)))
        .min_by(|a, b| dist(*a, c).total_cmp(&dist(*b, c)))
        .unwrap_or(p.text2)
}

/// map a Tier-2 primitive's normalized rect (x,y,w,h in 0..1) into the pixel box
/// (bx,by,bw,bh), clipping the right and bottom edges to the box so a plugin can
/// never paint past its own canvas; None when the mapped rect is sub-pixel
fn canvas_rect_px(box_px: (f32, f32, f32, f32), x: f32, y: f32, w: f32, h: f32) -> Option<(f32, f32, f32, f32)> {
    let (bx, by, bw, bh) = box_px;
    let rx = bx + x * bw;
    let ry = by + y * bh;
    let rw = (w * bw).min(bx + bw - rx).max(0.0);
    let rh = (h * bh).min(by + bh - ry).max(0.0);
    if rw >= 1.0 && rh >= 1.0 {
        Some((rx, ry, rw, rh))
    } else {
        None
    }
}

/// vertical crop of an image quad to a pane's visible height [0, content_h].
/// `top_y` is the quad's top in pane-local pixels (negative when scrolled above
/// the viewport), `height` its native pixel height. returns (visible_top,
/// visible_height, uv_top_frac, uv_bot_frac) — the uv fracs are 0..1 into the
/// image height — or None when the quad is fully off-screen or degenerate
fn clip_image_v(top_y: f32, height: f32, content_h: f32) -> Option<(f32, f32, f32, f32)> {
    if height <= 0.0 {
        return None;
    }
    let vis_top = top_y.max(0.0);
    let vis_bot = (top_y + height).min(content_h);
    if vis_bot <= vis_top {
        return None;
    }
    Some((vis_top, vis_bot - vis_top, (vis_top - top_y) / height, (vis_bot - top_y) / height))
}

#[cfg(feature = "microbench")]
impl Renderer {
    /// build the per-frame instance list for a full screen `iters` times with a
    /// warm atlas; returns (total elapsed, instances in the last build). lets the
    /// --microbench harness time draw_grid (it + Instance are private to this mod)
    pub(crate) fn bench_draw_grid(
        atlas: &mut GlyphAtlas,
        term: &Terminal,
        iters: u64,
    ) -> (std::time::Duration, usize) {
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out: Vec<Instance> = Vec::new();
        let run = |atlas: &mut GlyphAtlas, out: &mut Vec<Instance>| {
            out.clear();
            Self::draw_grid(
                atlas, &palette, out, term, 0.0, 0.0, true, 1.0, 1.0, 2.0, CursorShape::Block, None, None, &[], true, 1.0, true,
            );
        };
        for _ in 0..(iters / 8).max(1) {
            run(atlas, &mut out);
        }
        let t0 = std::time::Instant::now();
        for _ in 0..iters {
            run(atlas, &mut out);
        }
        (t0.elapsed(), out.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clip_image_v, CursorShape, FontId, GlyphAtlas, Instance, LinkPreview, Palette, Renderer,
        Terminal, ThemeId,
    };

    fn draw_backgrounds(input: &[u8], cols: usize) -> (Vec<Instance>, f32) {
        let mut term = Terminal::new(2, cols);
        vte::Parser::new().advance(&mut term, input);
        term.grid.cursor.visible = false;
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let cell_w = atlas.metrics(FontId::Content).cell_w;
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_grid(
            &mut atlas,
            &palette,
            &mut out,
            &term,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
            2.0,
            CursorShape::Block,
            None,
            None,
            &[],
            true,
            1.0,
            false,
        );
        (out, cell_w)
    }

    #[test]
    fn equal_background_cells_share_one_run() {
        let (out, cell_w) = draw_backgrounds(b"\x1b[41m   \x1b[0m", 6);
        assert_eq!(out.len(), 1);
        assert!((out[0].size[0] - cell_w * 3.0).abs() < 0.01);
    }

    #[test]
    fn alternating_background_colors_stay_separate() {
        let (out, cell_w) = draw_backgrounds(b"\x1b[41m \x1b[42m \x1b[41m ", 6);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|instance| (instance.size[0] - cell_w).abs() < 0.01));
    }

    #[test]
    fn hidden_cell_breaks_a_background_run() {
        let (out, cell_w) = draw_backgrounds(b"\x1b[41m \x1b[8m \x1b[28m ", 6);
        assert_eq!(out.len(), 2);
        assert!((out[0].pos[0] - 0.0).abs() < 0.01);
        assert!((out[1].pos[0] - cell_w * 2.0).abs() < 0.01);
    }

    #[test]
    fn inverse_foreground_cells_share_a_background_run() {
        let (out, cell_w) = draw_backgrounds(b"\x1b[31;7m  \x1b[27m", 6);
        assert_eq!(out.len(), 1);
        assert!((out[0].size[0] - cell_w * 2.0).abs() < 0.01);
    }

    #[test]
    fn link_preview_keeps_the_real_host_visible() {
        let mut preview = LinkPreview::new("https://account@example.com/releases/stable");
        preview.fit(21);
        assert_eq!(preview.fitted, "example.com  https/r\u{2026}");
        preview.fit(80);
        assert_eq!(preview.fitted, "example.com  https/releases/stable");

        let mut long =
            LinkPreview::new("https://misleading-subdomain.very-long.example.com/path");
        long.fit(12);
        assert_eq!(long.fitted, "mislea\u{2026}e.com");

        let mut deceptive = LinkPreview::new("https://evil-example.com/path");
        deceptive.fit(12);
        assert_eq!(deceptive.fitted, "evil-e\u{2026}e.com");
    }

    #[test]
    fn image_clip_fully_visible() {
        assert_eq!(clip_image_v(10.0, 50.0, 200.0), Some((10.0, 50.0, 0.0, 1.0)));
    }

    #[test]
    fn image_clip_bottom_overrun() {
        // top at 180 in a 200px pane, 50px tall: the bottom 30px is cropped off
        let (vt, vh, u0, u1) = clip_image_v(180.0, 50.0, 200.0).unwrap();
        assert_eq!((vt, vh), (180.0, 20.0));
        assert_eq!(u0, 0.0);
        assert!((u1 - 0.4).abs() < 1e-6);
    }

    #[test]
    fn image_clip_scrolled_above_top() {
        // top 30px above the viewport: show the lower 20px, uv starts mid-image
        let (vt, vh, u0, u1) = clip_image_v(-30.0, 50.0, 200.0).unwrap();
        assert_eq!((vt, vh), (0.0, 20.0));
        assert!((u0 - 0.6).abs() < 1e-6);
        assert_eq!(u1, 1.0);
    }

    #[test]
    fn image_clip_fully_offscreen() {
        assert!(clip_image_v(-60.0, 50.0, 200.0).is_none()); // wholly above
        assert!(clip_image_v(250.0, 50.0, 200.0).is_none()); // wholly below
        assert!(clip_image_v(10.0, 0.0, 200.0).is_none()); // degenerate height
    }

    #[test]
    fn live_bottom_keeps_a_stable_scrollbar() {
        let mut term = Terminal::new(2, 4);
        term.grid.linefeed();
        term.grid.linefeed();
        assert_eq!(term.grid.view_offset, 0);
        assert_eq!(term.grid.scrollback.len(), 1);

        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut draw = |term: &Terminal| {
            let mut out = Vec::new();
            Renderer::draw_grid(
                &mut atlas,
                &palette,
                &mut out,
                term,
                0.0,
                0.0,
                true,
                1.0,
                1.0,
                2.0,
                CursorShape::Block,
                None,
                None,
                &[],
                true,
                1.0,
                true,
            );
            out.len()
        };

        let empty = Terminal::new(2, 4);
        assert_eq!(draw(&term), draw(&empty) + 2);
    }

    // "x ==> y" paints its punctuation as ONE ligature strip instance where
    // the per-cell path pushed three glyphs; ligatures=false restores those
    #[test]
    fn ligature_run_collapses_to_one_instance() {
        let mut term = Terminal::new(2, 12);
        let mut p = vte::Parser::new();
        p.advance(&mut term, b"x ==> y");
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let draw = |liga: bool, atlas: &mut GlyphAtlas| {
            let mut out = Vec::new();
            Renderer::draw_grid(
                atlas,
                &palette,
                &mut out,
                &term,
                0.0,
                0.0,
                true,
                1.0,
                1.0,
                2.0,
                CursorShape::Block,
                None,
                None,
                &[],
                true,
                1.0,
                liga,
            );
            out.len()
        };
        let per_cell = draw(false, &mut atlas);
        let ligated = draw(true, &mut atlas);
        assert_eq!(ligated, per_cell - 2, "three run cells collapse into one strip");
    }

    // a run over colored backgrounds must flush its strip AFTER every covered
    // cell's bg rect, or the later rects wash the ligature's tail (instances
    // paint in vec order); regression for the adversarial-review P1
    #[test]
    fn ligature_strip_paints_over_covered_cell_backgrounds() {
        let mut term = Terminal::new(2, 8);
        let mut p = vte::Parser::new();
        p.advance(&mut term, b"\x1b[41m==>\x1b[0m");
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_grid(
            &mut atlas,
            &palette,
            &mut out,
            &term,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
            2.0,
            CursorShape::Block,
            None,
            None,
            &[],
            true,
            1.0,
            true,
        );
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        let cell_h = atlas.metrics(super::FontId::Content).cell_h;
        let strip = out
            .iter()
            .position(|i| i.kind == 1 && i.size[0] > cell_w * 2.5)
            .expect("the ==> run renders as one strip");
        // the red bg rects of all three covered cells precede the strip
        let last_run_bg = out
            .iter()
            .enumerate()
            .filter(|(_, i)| i.kind == 0 && i.pos[1] < cell_h * 0.5 && i.pos[0] < cell_w * 2.5)
            .map(|(j, _)| j)
            .max()
            .expect("bg rects present");
        assert!(last_run_bg < strip, "bg {last_run_bg} must precede strip {strip}");
    }

    // kitty z-index: a z<0 placement's quad must land before every cell
    // instance (so text and colored backgrounds paint over it), a z>=0 one
    // after them (instances paint in vec order)
    #[test]
    fn negative_z_placements_paint_beneath_the_cells() {
        let mut term = Terminal::new(4, 10);
        let mut p = vte::Parser::new();
        p.advance(&mut term, b"\x1b[41mxx\x1b[0m");
        let under_id = term.images.insert(1, 1, vec![9, 9, 9, 255]);
        let over_id = term.images.insert(1, 1, vec![7, 7, 7, 255]);
        term.grid.place_image_at(under_id, 0, 0, 1, 1, -1);
        term.grid.place_image_at(over_id, 0, 0, 1, 1, 0);
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_grid(
            &mut atlas,
            &palette,
            &mut out,
            &term,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
            2.0,
            CursorShape::Block,
            None,
            None,
            &[],
            true,
            1.0,
            false,
        );
        let quads: Vec<usize> =
            out.iter().enumerate().filter(|(_, i)| i.kind == 3).map(|(j, _)| j).collect();
        assert_eq!(quads.len(), 2, "both placements render");
        let bgs: Vec<usize> =
            out.iter().enumerate().filter(|(_, i)| i.kind == 0).map(|(j, _)| j).collect();
        assert!(!bgs.is_empty(), "the red cells push bg rects");
        assert!(quads[0] < *bgs.first().unwrap(), "z=-1 sits beneath every cell rect");
        assert!(quads[1] > *bgs.last().unwrap(), "z=0 sits above the cell rects");
    }

    // kitty unicode placeholders: each placeholder cell paints its own tile of
    // the virtual placement, and a bare cell inherits row/col from its left
    // neighbor per the protocol's omitted-diacritic rules
    #[test]
    fn placeholder_cells_tile_their_virtual_placement() {
        let mut term = Terminal::new(2, 10);
        // a 2x1 image virtually placed into a 2x1 cell box
        let id = term.images.insert(2, 1, vec![255, 255, 255, 255, 0, 0, 0, 255]);
        term.grid.set_virtual_placement(id, 2, 1);
        // fg indexed color = image id; first cell says row 0 col 0, the bare
        // second cell inherits row 0 and col 1
        let mut p = vte::Parser::new();
        let text = format!("\x1b[38;5;{id}m\u{10EEEE}\u{305}\u{305}\u{10EEEE}\x1b[0m");
        p.advance(&mut term, text.as_bytes());
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_grid(
            &mut atlas,
            &palette,
            &mut out,
            &term,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
            2.0,
            CursorShape::Block,
            None,
            None,
            &[],
            true,
            1.0,
            false,
        );
        let tiles: Vec<&super::Instance> = out.iter().filter(|i| i.kind == 3).collect();
        assert_eq!(tiles.len(), 2, "both placeholder cells paint a tile");
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        assert_eq!(tiles[0].pos[0], 0.0);
        assert!((tiles[1].pos[0] - cell_w).abs() < 0.01, "second tile sits in column 1");
        // the box is wider than tall, so the fit spans the full width: the
        // left tile samples the image's left half, the right tile its right
        assert!((tiles[0].uv_max[0] - tiles[1].uv_min[0]).abs() < 1e-6);
        assert!(tiles[0].uv_min[0] < tiles[0].uv_max[0]);
        // no glyph instances: the placeholder chars render as image, not text
        assert!(!out.iter().any(|i| i.kind == 1), "no text glyphs for placeholder cells");
    }

    #[test]
    fn preedit_cell_offsets_span_widths() {
        use super::preedit_cell_offset;
        let s = "aあb"; // 1 + 2 + 1 cells
        assert_eq!(preedit_cell_offset(s, 0), 0);
        assert_eq!(preedit_cell_offset(s, 1), 1);
        assert_eq!(preedit_cell_offset(s, 4), 3);
        assert_eq!(preedit_cell_offset(s, s.len()), 4);
        // a combining mark occupies no cell of its own
        assert_eq!(preedit_cell_offset("e\u{301}", 3), 1);
    }

    fn preedit_rects(term: &Terminal, text: &str, caret: Option<(usize, usize)>) -> Vec<super::Instance> {
        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_preedit(
            &mut atlas,
            &palette,
            &mut out,
            term,
            0.0,
            0.0,
            super::PreeditView { text, caret },
            2.0,
        );
        out
    }

    #[test]
    fn preedit_draws_nothing_when_empty_or_scrolled() {
        let mut term = Terminal::new(2, 10);
        assert!(preedit_rects(&term, "", Some((0, 0))).is_empty());
        term.grid.linefeed();
        term.grid.linefeed();
        term.grid.view_offset = 1;
        assert!(preedit_rects(&term, "abc", Some((0, 0))).is_empty());
    }

    #[test]
    fn preedit_bg_covers_the_run_at_the_cursor() {
        let atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        let mut term = Terminal::new(2, 10);
        term.grid.cursor.col = 2;
        let out = preedit_rects(&term, "abc", Some((3, 3)));
        // the bg fill is the first instance: at the cursor cell, 3 cells wide
        assert_eq!(out[0].pos[0], 2.0 * cell_w);
        assert_eq!(out[0].size[0], 3.0 * cell_w);
        // end caret: the bar is the last instance, one run-width right of the start
        assert_eq!(out.last().unwrap().pos[0], 5.0 * cell_w);
        assert_eq!(out.last().unwrap().size[0], 2.0);
    }

    #[test]
    fn preedit_slides_left_at_the_right_edge() {
        let atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        let mut term = Terminal::new(2, 10);
        term.grid.cursor.col = 8;
        let out = preedit_rects(&term, "abc", Some((0, 0)));
        assert_eq!(out[0].pos[0], 7.0 * cell_w);
        assert_eq!(out[0].size[0], 3.0 * cell_w);
    }

    #[test]
    fn preedit_wider_than_the_pane_keeps_its_tail() {
        let atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        let term = Terminal::new(2, 4);
        // 8 cells into a 4-col pane: the head clips, the tail stays visible
        let out = preedit_rects(&term, "abcdefgh", None);
        assert_eq!(out[0].pos[0], 0.0);
        assert_eq!(out[0].size[0], 4.0 * cell_w);
    }

    #[test]
    fn preedit_active_clause_gets_a_thick_line() {
        let atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let cell_w = atlas.metrics(super::FontId::Content).cell_w;
        let term = Terminal::new(2, 10);
        // clause spans bytes 0..3 = cells 0..3
        let out = preedit_rects(&term, "abc", Some((0, 3)));
        let clause = out.last().unwrap();
        assert_eq!(clause.pos[0], 0.0);
        assert_eq!(clause.size[0], 3.0 * cell_w);
        // double the single-line thickness marks it as active
        let single = &out[out.len() - 2];
        assert_eq!(clause.size[1], single.size[1] * 2.0);
    }

    #[test]
    fn scrollbar_uses_the_right_padding_gutter() {
        let thumb = Renderer::scrollbar_geom(10.0, 20.0, 4, 2, 8.0, 16.0, 2.0, 10, 0).unwrap();
        assert!(thumb.track_x >= 10.0 + 4.0 * 8.0);
    }

    #[test]
    fn scrollbar_renders_a_prompt_tick() {
        let mut marked = Terminal::new(2, 4);
        marked.grid.mark_prompt();
        marked.grid.linefeed();
        marked.grid.linefeed();
        let mut plain = Terminal::new(2, 4);
        plain.grid.linefeed();
        plain.grid.linefeed();

        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut draw = |term: &Terminal| {
            let mut out = Vec::new();
            Renderer::draw_grid(
                &mut atlas,
                &palette,
                &mut out,
                term,
                0.0,
                0.0,
                true,
                1.0,
                1.0,
                2.0,
                CursorShape::Block,
                None,
                None,
                &[],
                true,
                1.0,
                true,
            );
            out.len()
        };

        assert_eq!(draw(&marked), draw(&plain) + 1);
    }

    // a prompt at the live bottom lands its tick inside the thumb; it must
    // paint as a dark notch (bg), not the paper color that vanishes into it
    #[test]
    fn bottom_prompt_tick_notches_the_thumb() {
        let mut term = Terminal::new(2, 4);
        term.grid.linefeed();
        term.grid.linefeed();
        term.grid.mark_prompt();

        let mut atlas = GlyphAtlas::new(14.0, 12.5, 1.0, None, 1.32);
        let palette = Palette::from_theme(ThemeId::Instrument);
        let mut out = Vec::new();
        Renderer::draw_grid(
            &mut atlas,
            &palette,
            &mut out,
            &term,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
            2.0,
            CursorShape::Block,
            None,
            None,
            &[],
            true,
            1.0,
            true,
        );
        let tick = out.last().unwrap();
        let bg = palette.bg.to_linear_f32();
        assert_eq!(&tick.color[..3], &bg[..3]);
    }

    #[test]
    fn tier2_hex_color_parses_3_and_6_digit() {
        use super::{Renderer, Rgb};
        assert_eq!(Renderer::parse_hex_rgb("fff"), Some(Rgb::new(255, 255, 255)));
        assert_eq!(Renderer::parse_hex_rgb("000"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(Renderer::parse_hex_rgb("6486a6"), Some(Rgb::new(0x64, 0x86, 0xa6)));
        assert_eq!(Renderer::parse_hex_rgb("f0a"), Some(Rgb::new(255, 0, 170)));
    }

    #[test]
    fn tier2_hex_color_rejects_bad_input() {
        use super::Renderer;
        assert!(Renderer::parse_hex_rgb("").is_none());
        assert!(Renderer::parse_hex_rgb("12").is_none());
        assert!(Renderer::parse_hex_rgb("12345").is_none());
        assert!(Renderer::parse_hex_rgb("gg00zz").is_none());
    }

    #[test]
    fn dock_color_resolves_roles_hex_and_fallback() {
        use super::{resolve_dock_color, Palette, ThemeId};
        let p = Palette::from_theme(ThemeId::Instrument);
        assert_eq!(resolve_dock_color(&p, "paper"), p.paper);
        assert_eq!(resolve_dock_color(&p, "ink"), p.ink1);
        assert_eq!(resolve_dock_color(&p, "ink1"), p.ink1);
        assert_eq!(resolve_dock_color(&p, "rule2"), p.rule2);
        // accent maps to the high-contrast paper color, like the widget title
        assert_eq!(resolve_dock_color(&p, "accent"), p.paper);
        // a #hex spec snaps to the theme's nearest color: its own exact color
        // comes back unchanged, and pure red lands on the theme's red
        assert_eq!(resolve_dock_color(&p, &format!("#{:02x}{:02x}{:02x}", p.paper.r, p.paper.g, p.paper.b)), p.paper);
        assert_eq!(resolve_dock_color(&p, "#ff0000"), p.ansi_color(1));
        assert_eq!(resolve_dock_color(&p, "ok"), p.ansi_color(2));
        assert_eq!(resolve_dock_color(&p, "ansi12"), p.ansi_color(12));
        assert_eq!(resolve_dock_color(&p, "ansi16"), p.text2);
        // anything unrecognized falls back to the dock body color
        assert_eq!(resolve_dock_color(&p, "chartreuse"), p.text2);
        assert_eq!(resolve_dock_color(&p, "#zzz"), p.text2);
    }

    #[test]
    fn canvas_rect_clips_to_its_box() {
        use super::canvas_rect_px;
        let b = (100.0_f32, 200.0_f32, 80.0_f32, 40.0_f32);
        // a full-box primitive maps to the whole box
        assert_eq!(canvas_rect_px(b, 0.0, 0.0, 1.0, 1.0), Some((100.0, 200.0, 80.0, 40.0)));
        // a primitive starting mid-box and over-wide is clipped to the box edges
        let (rx, ry, rw, rh) = canvas_rect_px(b, 0.5, 0.5, 1.0, 1.0).unwrap();
        assert_eq!((rx, ry), (140.0, 220.0));
        assert!((rx + rw - (b.0 + b.2)).abs() < 1e-4, "right edge must stay inside the box");
        assert!((ry + rh - (b.1 + b.3)).abs() < 1e-4, "bottom edge must stay inside the box");
        // a sub-pixel primitive is dropped
        assert_eq!(canvas_rect_px(b, 0.0, 0.0, 0.001, 0.001), None);
        // a zero-room primitive at the far corner is dropped
        assert_eq!(canvas_rect_px(b, 1.0, 1.0, 0.5, 0.5), None);
    }
}

/// round a tightly-packed RGBA row up to wgpu's 256-byte copy alignment
#[cfg(any(test, debug_assertions))]
fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

/// serialize every gpu-using test: the ci vm's software adapter intermittently
/// dies with STATUS_ACCESS_VIOLATION when device work runs concurrently from
/// the parallel test harness
#[cfg(test)]
fn gpu_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// one adapter enumeration + device shared by every gpu test (device and queue
/// are cloneable handles); per-test enumeration serialized behind gpu_serial
/// costs over a minute of wall clock. None when the machine has no adapter
#[cfg(test)]
fn test_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    static DEV: std::sync::OnceLock<Option<(wgpu::Device, wgpu::Queue)>> = std::sync::OnceLock::new();
    DEV.get_or_init(|| {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-test-shared"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .ok()
    })
    .clone()
}

#[cfg(test)]
mod gpu_tests {
    // the shared surfaceless device, or None when no adapter is present
    // (validation tests skip rather than fail)
    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        super::test_device()
    }

    // validate shader.wgsl through naga's front-end without a window — catches
    // wgsl syntax/type errors (a malformed binding or fragment branch)
    #[test]
    fn shader_validates() {
        let _gpu = super::gpu_serial();
        let Some((device, _queue)) = headless_device() else {
            return;
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell-shader-test"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let err = pollster::block_on(scope.pop());
        assert!(err.is_none(), "shader.wgsl failed validation: {err:?}");
    }

    // build the real bind group layouts + cell pipeline headlessly: catches a
    // shader/layout binding mismatch, e.g. if the color-emoji bindings 4/5 in
    // shader.wgsl and build_atlas_bgl ever drift apart
    #[test]
    fn pipeline_validates() {
        let _gpu = super::gpu_serial();
        let Some((device, _queue)) = headless_device() else {
            return;
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let uniform_bgl = super::build_uniform_bgl(&device);
        let atlas_bgl = super::build_atlas_bgl(&device);
        let _pipeline =
            super::build_cell_pipeline(&device, &uniform_bgl, &atlas_bgl, wgpu::TextureFormat::Bgra8UnormSrgb);
        let err = pollster::block_on(scope.pop());
        assert!(err.is_none(), "cell pipeline failed validation: {err:?}");
    }
}

#[cfg(test)]
mod hit_tests {
    use super::*;

    // a real headless renderer on the shared test device, or None when no gpu
    // adapter is present (e.g. CI), so these skip rather than fail
    fn headless() -> Option<Renderer> {
        let (device, queue) = test_device()?;
        // tall enough that both market rows render, so two distinct rows can be
        // routed; scale 1.0 keeps the layout math simple
        Some(Renderer::new_headless_on(device, queue, 1280, 900, 14.0, 12.5, 1.0))
    }

    fn tmp_png(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("termie-hittest-{tag}-{}.png", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn drag_hints_stay_on_drag_handles() {
        assert_eq!(
            drag_hint_variants(Some(Hot::WindowTabs))[0],
            ("DRAG BADGE", "merge all tabs into another window")
        );
        assert_eq!(
            drag_hint_variants(Some(Hot::Tab(2)))[0],
            ("DRAG TAB", "reorder \u{00b7} move between windows \u{00b7} pull out")
        );
        assert!(drag_hint_variants(Some(Hot::TabClose(2))).is_empty());
        assert!(drag_hint_variants(Some(Hot::Gear)).is_empty());
    }

    #[test]
    fn market_clicks_route_to_close_chip_and_card() {
        let _gpu = gpu_serial();
        let Some(mut r) = headless() else {
            return;
        };
        r.set_tabs(vec!["a".into()], 0);
        r.set_market(Some(MarketView {
            rows: vec![
                MarketRowView { name: "tamagotchi".into(), version: "1.2".into(), description: "a pet".into(), permissions: vec![], installed: true, enabled: true },
                MarketRowView { name: "relay".into(), version: "0.4".into(), description: "bus".into(), permissions: vec!["write_pty".into()], installed: false, enabled: false },
            ],
            selected: 0,
            status: String::new(),
            loading: false,
            fetch_failed: false,
        }));
        r.settle_overlay();
        let _ = r.render_png(&[], true, false, false, &tmp_png("market"));

        // every registered region must resolve to the hit it was built for, and a
        // card or chip must map back to its own row index
        let mut seen: Vec<MarketHit> = Vec::new();
        for &((x, y, w, h), expected) in &r.market_hits {
            let got = r.market_hit_at(x + w / 2.0, y + h / 2.0).expect("a registered region must hit");
            assert_eq!(got, expected, "the centre of a region resolved to the wrong hit");
            seen.push(expected);
        }
        // the close control, plus a body card and an action chip for each row
        for want in [MarketHit::Close, MarketHit::Card(0), MarketHit::Chip(0), MarketHit::Card(1), MarketHit::Chip(1)] {
            assert!(seen.contains(&want), "{want:?} should be hittable");
        }
        // a point in the far corner is outside every region
        assert_eq!(r.market_hit_at(2.0, 2.0), None);
        let _ = std::fs::remove_file(tmp_png("market"));
    }

    #[test]
    fn palette_rows_and_find_buttons_are_hittable() {
        let _gpu = gpu_serial();
        let Some(mut r) = headless() else {
            return;
        };
        r.set_tabs(vec!["a".into()], 0);
        r.set_palette(Some(PaletteView {
            query: "ne".into(),
            items: vec!["new tab".into(), "new tab here".into(), "new window".into()],
            hints: Vec::new(),
            selected: 0,
            scope: "commands",
        }));
        r.settle_overlay();
        let _ = r.render_png(&[], true, false, false, &tmp_png("palette"));
        let ((bx, by, bw, _), row_h, n, first) = r.palette_geom.expect("palette geometry recorded");
        assert_eq!((n, first), (3, 0));
        // the centre of each row resolves to its own index
        for i in 0..n {
            let y = by + row_h * (i as f32 + 1.0) + row_h / 2.0;
            assert_eq!(r.palette_row_at(bx + bw / 2.0, y), Some(i));
        }
        // the input row and the outside are not rows
        assert_eq!(r.palette_row_at(bx + bw / 2.0, by + row_h / 2.0), None);
        assert!(!r.palette_contains(2.0, 2.0));
        assert!(r.palette_contains(bx + 2.0, by + 2.0));

        // a list longer than nine rows windows around the selection (the font
        // picker feeds dozens of families); clicks resolve to absolute indices
        r.set_palette(Some(PaletteView {
            query: String::new(),
            items: (0..20).map(|i| format!("font {i}")).collect(),
            hints: Vec::new(),
            selected: 15,
            scope: "fonts",
        }));
        r.settle_overlay();
        let _ = r.render_png(&[], true, false, false, &tmp_png("palette"));
        let ((bx, by, bw, _), row_h, n, first) = r.palette_geom.expect("windowed geometry");
        // selected 15 sits on the last visible row: window is [7, 15]
        assert_eq!((n, first), (9, 7));
        let last = by + row_h * 9.0 + row_h / 2.0;
        assert_eq!(r.palette_row_at(bx + bw / 2.0, last), Some(15));
        let top = by + row_h + row_h / 2.0;
        assert_eq!(r.palette_row_at(bx + bw / 2.0, top), Some(7));

        r.set_palette(None);
        r.set_find(Some(FindView {
            query: "err".into(),
            count: 4,
            current: 1,
            capped: false,
            matches: Vec::new(),
            regex_on: true,
            bad: false,
        }));
        let _ = r.render_png(&[], true, false, false, &tmp_png("find"));
        let (rx, prev, next, close) = r.find_btns.expect("find buttons recorded");
        assert_eq!(r.find_btn_at(rx.0 + rx.2 / 2.0, rx.1 + rx.3 / 2.0), Some(0));
        assert_eq!(r.find_btn_at(prev.0 + prev.2 / 2.0, prev.1 + prev.3 / 2.0), Some(1));
        assert_eq!(r.find_btn_at(next.0 + next.2 / 2.0, next.1 + next.3 / 2.0), Some(2));
        assert_eq!(r.find_btn_at(close.0 + close.2 / 2.0, close.1 + close.3 / 2.0), Some(3));
        assert_eq!(r.find_btn_at(2.0, 2.0), None);
        let _ = std::fs::remove_file(tmp_png("palette"));
        let _ = std::fs::remove_file(tmp_png("find"));
    }

    #[test]
    fn context_menu_rows_route_to_their_index() {
        let _gpu = gpu_serial();
        let Some(mut r) = headless() else {
            return;
        };
        r.set_tabs(vec!["a".into(), "b".into()], 0);
        // the tab menu carries a different item list than the pane menu; the
        // shared geometry must resolve each of its rows and reject the outside
        let items: Vec<String> = TAB_MENU_ITEMS.iter().map(|s| s.to_string()).collect();
        r.set_pane_menu(Some(PaneMenuView { x: 60.0, y: 60.0, hovered: None, items, accents: Vec::new() }));
        let labels: Vec<String> = TAB_MENU_ITEMS.iter().map(|s| s.to_string()).collect();
        let (bx, by, mw, row_h, pad) = r.pane_menu_geom(60.0, 60.0, &labels, false);
        for i in 0..TAB_MENU_ITEMS.len() {
            let cy = by + pad + row_h * (i as f32 + 0.5);
            assert_eq!(r.pane_menu_item_at(bx + mw / 2.0, cy), Some(i));
        }
        assert_eq!(r.pane_menu_item_at(bx + mw / 2.0, by), None);
        assert_eq!(r.pane_menu_item_at(2.0, 2.0), None);
    }

    #[test]
    fn newtab_chevron_is_a_distinct_hit_zone() {
        let _gpu = gpu_serial();
        let Some(mut r) = headless() else {
            return;
        };
        r.set_tabs(vec!["a".into()], 0);
        let (nx, ny, nw, nh) = r.newtab_rect();
        let my = ny + nh / 2.0;
        // the '+' opens a default tab; the chevron flush against its right edge
        // opens the profile menu, so the two must resolve to different hits
        assert!(matches!(r.hit_test(nx + nw / 2.0, my), Hit::Button(Hot::NewTab)));
        assert!(matches!(r.hit_test(nx + nw + 2.0, my), Hit::Button(Hot::NewTabMenu)));
        assert!(matches!(
            r.hit_test(r.window_tabs_rect().2 / 2.0, my),
            Hit::Button(Hot::WindowTabs)
        ));
    }

    #[test]
    fn dock_tier2_widget_is_hittable_and_paints() {
        let _gpu = gpu_serial();
        let Some(mut r) = headless() else {
            return;
        };
        r.set_tabs(vec!["a".into()], 0);
        r.set_dock(vec![
            DockWidget {
                title: "Gauge".into(),
                lines: vec!["60%".into()],
                draw: vec![
                    DockDraw::Rect { x: 0.0, y: 0.0, w: 1.0, h: 0.25, color: "ink".into() },
                    DockDraw::Rect { x: 0.0, y: 0.0, w: 0.6, h: 0.25, color: "#6486a6".into() },
                    DockDraw::Text { x: 0.0, y: 0.3, text: "60%".into(), color: "paper".into() },
                ],
                canvas_h: Some(64.0),
            },
            DockWidget { title: "Status".into(), lines: vec!["ok".into()], draw: vec![], canvas_h: None },
        ]);
        let _ = r.render_png(&[], true, false, false, &tmp_png("dock"));

        // the dock carved one clickable band per widget; a point inside band i
        // must select widget i
        assert_eq!(r.dock_hitboxes.len(), 2, "two widgets should yield two bands");
        for (i, &(x, y, w, h)) in r.dock_hitboxes.iter().enumerate() {
            assert_eq!(r.widget_at(x + w / 2.0, y + h / 2.0), Some(i), "a point in band {i} should select widget {i}");
        }
        // a point far outside the dock hits no widget
        assert_eq!(r.widget_at(5.0, 5.0), None);
        let _ = std::fs::remove_file(tmp_png("dock"));
    }
}
