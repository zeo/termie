use vte::{Params, Perform};

use crate::color::{Color, DynColors, Palette};
use crate::grid::{CursorShape, Grid, UnderlineStyle};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Osc133 {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandDone(Option<i32>),
}

/// mouse tracking mode requested by the program (DECSET 1000/1002/1003)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseProto {
    Off,
    /// 1000: press/release only
    Normal,
    /// 1002: press/release + motion while a button is held
    Button,
    /// 1003: any motion
    Any,
}

/// a pending color query (OSC 4/10/11/12); the app layer answers from the palette
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorReq {
    Fg,
    Bg,
    Cursor,
    Ansi(u8),
}

/// format the OSC reply to a color query as rgb:RRRR/GGGG/BBBB (8-bit values
/// doubled to 16-bit per xterm), terminated with ST
pub fn format_color_reply(req: ColorReq, pal: &Palette) -> Vec<u8> {
    let (code, c) = match req {
        ColorReq::Fg => ("10".to_string(), pal.fg),
        ColorReq::Bg => ("11".to_string(), pal.bg),
        ColorReq::Cursor => ("12".to_string(), pal.cursor),
        ColorReq::Ansi(n) => (format!("4;{n}"), pal.ansi_color(n)),
    };
    color_reply(&code, c)
}

fn color_reply(code: &str, c: crate::color::Rgb) -> Vec<u8> {
    format!(
        "\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
        code, c.r, c.r, c.g, c.g, c.b, c.b
    )
    .into_bytes()
}

/// parse an xterm color spec: `rgb:R/G/B` (1-4 hex digits per channel, most
/// significant bits win) or `#RRGGBB` / `#RGB` — the forms theme scripts and
/// editors actually emit when setting colors
fn parse_color_spec(s: &[u8]) -> Option<crate::color::Rgb> {
    fn channel(p: &str) -> Option<u8> {
        let v = u32::from_str_radix(p, 16).ok()?;
        Some(match p.len() {
            1 => (v * 17) as u8,
            2 => v as u8,
            3 => (v >> 4) as u8,
            4 => (v >> 8) as u8,
            _ => return None,
        })
    }
    let s = std::str::from_utf8(s).ok()?.trim();
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let r = channel(it.next()?)?;
        let g = channel(it.next()?)?;
        let b = channel(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        return Some(crate::color::Rgb::new(r, g, b));
    }
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some(crate::color::Rgb::new((v >> 16) as u8, (v >> 8) as u8, v as u8)),
            3 => Some(crate::color::Rgb::new(
                (((v >> 8) & 0xf) * 17) as u8,
                (((v >> 4) & 0xf) * 17) as u8,
                ((v & 0xf) * 17) as u8,
            )),
            _ => None,
        };
    }
    None
}

/// kitty keyboard protocol flags termie honors: disambiguate (1), report
/// event types (2), report all keys as escape codes (8), report associated
/// text (16). alternate-keys (4) needs the layout's base key, which winit
/// doesn't surface. bits an app requests outside this mask are dropped, so a
/// CSI ? u query always reports exactly what we apply
const KBD_SUPPORTED: u8 = 0b11011;
/// bound the flag stack so a misbehaving app can't grow it without limit
const KBD_STACK_CAP: usize = 16;

pub struct Terminal {
    pub grid: Grid,
    saved_primary: Option<Grid>,
    pub using_alt: bool,

    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    pub mouse_proto: MouseProto,
    pub mouse_sgr: bool,
    pub focus_events: bool,
    /// mode 1007 "alternate scroll": wheel becomes arrow keys on the alt
    /// screen. ships on (the pager-friendly default) but apps may turn it off
    pub alt_scroll: bool,

    pub title: String,
    /// set when OSC 0/2 changes the title; drained by the app like cwd_dirty
    pub title_dirty: bool,
    pub cwd: Option<String>,
    /// set when OSC-7 updates cwd; the app consumes it to relabel tabs instead
    /// of rescanning every output chunk for the escape (which also missed an
    /// OSC-7 split across two reads and fired on the byte pattern in file data)
    pub cwd_dirty: bool,
    /// dynamic colors a program set at runtime (OSC 4/10/11/12): base16-shell,
    /// pywal, editors setting the background — layered over the pane's palette
    /// at paint, reset by OSC 104/110-112 or a hard reset
    pub colors: DynColors,
    pub last_osc133: Option<Osc133>,
    /// a command is executing (inside an OSC 133 C..D window) — drives the
    /// pane's "agent running" badge
    pub cmd_running: bool,
    /// sticky command-finished event (exit code), drained by the app like
    /// bell/notify; last_osc133 alone misses it because the next prompt's A/B
    /// usually arrives in the same chunk
    pub cmd_done: Option<Option<i32>>,
    pub bell: bool,
    /// the message body of an OSC 9 / OSC 777 notification, drained by the app
    /// into the status-bar readout (the bell flag carries the attention signal)
    pub notify: Option<String>,

    /// bytes the terminal wants to send back to the pty (DSR/DA replies)
    pub responses: Vec<u8>,
    pub dirty: bool,
    /// DEC 2026 synchronized output: while true an app is mid-frame, so the
    /// renderer holds off painting until the frame ends (no torn/flickering UI)
    pub sync_output: bool,
    /// kitty keyboard protocol flag stack; the last entry is active. starts as
    /// [0] (legacy encoding) and apps push/set richer reporting onto it
    kbd_stack: Vec<u8>,
    /// pending OSC 52 clipboard write, drained by the app to the OS clipboard
    pub clipboard: Option<String>,
    /// pending OSC 4/10/11/12 color queries, answered by the app from the palette
    pub color_queries: Vec<ColorReq>,
    /// ConEmu OSC 9;4 progress: (state, percent). state 1 normal, 2 error,
    /// 3 indeterminate, 4 paused; None when cleared (state 0)
    pub progress: Option<(u8, u8)>,
    /// g0/g1 charset designations + active locking shift (SO/SI) for the DEC
    /// special-graphics line-drawing set
    g0: Charset,
    g1: Charset,
    gl: u8,
    /// last printed char, for REP (CSI Ps b)
    last_print: Option<char>,
    /// decoded kitty graphics images for this pane
    pub images: crate::image::ImageStore,
    /// cell size in physical pixels, fed by the renderer; (0,0) = unknown and
    /// the pixel-size XTWINOPS reports stay silent rather than lie
    cell_px: (u16, u16),
    /// in-flight DCS consumer, alive between hook and unhook
    dcs: Option<Dcs>,
    /// DECSDM (mode 80): set pins a sixel image to the top-left and leaves the
    /// cursor alone; reset (the default) scrolls it inline with the text
    sixel_display_mode: bool,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        Terminal {
            grid: Grid::new(rows, cols),
            saved_primary: None,
            using_alt: false,
            app_cursor_keys: false,
            bracketed_paste: false,
            mouse_proto: MouseProto::Off,
            mouse_sgr: false,
            focus_events: false,
            alt_scroll: true,
            title: String::new(),
            title_dirty: false,
            cwd: None,
            cwd_dirty: false,
            colors: DynColors::default(),
            last_osc133: None,
            cmd_running: false,
            cmd_done: None,
            bell: false,
            notify: None,
            responses: Vec::new(),
            dirty: true,
            sync_output: false,
            kbd_stack: vec![0],
            clipboard: None,
            color_queries: Vec::new(),
            progress: None,
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            gl: 0,
            last_print: None,
            images: crate::image::ImageStore::default(),
            cell_px: (0, 0),
            dcs: None,
            sixel_display_mode: false,
        }
    }

    /// let the renderer feed the content cell size so XTWINOPS 14/16 can
    /// report pixel geometry — image tools size kitty graphics from those,
    /// since nothing can ioctl a pixel size through ConPTY
    pub fn set_cell_px(&mut self, w: u16, h: u16) {
        self.cell_px = (w, h);
    }

    /// current cell size in physical pixels; (0, 0) until the renderer feeds it
    pub fn cell_px(&self) -> (u16, u16) {
        self.cell_px
    }

    /// kitty cursor-movement policy after a placement: right by the box's
    /// columns and down onto its LAST row (not past it), wrapping at the right
    /// edge and scrolling past the bottom margin exactly like printed text.
    /// cols/rows of 0 fall back to the image's pixel size over the cell size
    /// (assumed 10x20 until the renderer attaches, like the sixel path)
    pub fn advance_cursor_past_image(&mut self, px_w: u32, px_h: u32, cols: u16, rows: u16) {
        let cw = if self.cell_px.0 > 0 { self.cell_px.0 as usize } else { 10 };
        let ch = if self.cell_px.1 > 0 { self.cell_px.1 as usize } else { 20 };
        let cols_eff = if cols > 0 { cols as usize } else { (px_w as usize).div_ceil(cw) };
        let rows_eff = if rows > 0 { rows as usize } else { (px_h as usize).div_ceil(ch) };
        let mut down = rows_eff.saturating_sub(1);
        let col = self.grid.cursor.col + cols_eff;
        if col >= self.grid.cols {
            self.grid.cursor.col = 0;
            down += 1;
        } else {
            self.grid.cursor.col = col;
        }
        for _ in 0..down {
            self.grid.linefeed();
        }
        self.grid.cursor.wrap_pending = false;
    }

    fn map_charset(&self, c: char) -> char {
        let active = if self.gl == 0 { self.g0 } else { self.g1 };
        if active == Charset::DecGraphics {
            dec_special_graphics(c)
        } else {
            c
        }
    }

    /// DECSTR soft reset: restore charset, common modes, the SGR pen, the scroll
    /// region and the cursor-shape default without rebuilding the grid
    fn soft_reset(&mut self) {
        self.g0 = Charset::Ascii;
        self.g1 = Charset::Ascii;
        self.gl = 0;
        self.app_cursor_keys = false;
        self.bracketed_paste = false;
        // drop mouse / focus / sync / kitty keys so a soft-resetting TUI can't
        // leave the stream in a state that floods the next prompt with reports
        self.mouse_proto = MouseProto::Off;
        self.mouse_sgr = false;
        self.focus_events = false;
        self.sync_output = false;
        self.kbd_stack = vec![0];
        self.apply_sgr(&[&[0u16][..]]);
        self.grid.set_scroll_region(0, self.grid.rows - 1);
        self.grid.origin_mode = false;
        // DECSTR puts IRM back to replace
        self.grid.insert_mode = false;
        // DECSTR restores autowrap to its set default
        self.grid.autowrap = true;
        self.grid.cursor.shape_set = false;
        self.grid.cursor.shape_blink = None;
        // DECSTR turns the text cursor back on (DECTCEM)
        self.grid.cursor.visible = true;
    }

    /// DECRQM mode-state reply value: 1 = set, 2 = reset, 0 = not recognized
    fn dec_mode_state(&self, mode: u16) -> u16 {
        let on = match mode {
            1 => self.app_cursor_keys,
            6 => self.grid.origin_mode,
            7 => self.grid.autowrap,
            // unset means the user-configured default (not visible from here);
            // report the spec default of blinking until an app sets it
            12 => self.grid.cursor.shape_blink.unwrap_or(true),
            25 => self.grid.cursor.visible,
            1000 => self.mouse_proto == MouseProto::Normal,
            1002 => self.mouse_proto == MouseProto::Button,
            1003 => self.mouse_proto == MouseProto::Any,
            1004 => self.focus_events,
            1006 => self.mouse_sgr,
            1007 => self.alt_scroll,
            80 => self.sixel_display_mode,
            2004 => self.bracketed_paste,
            2026 => self.sync_output,
            47 | 1047 | 1049 => self.using_alt,
            _ => return 0,
        };
        if on { 1 } else { 2 }
    }

    /// the current SGR pen as a DECRQSS report body ("0;1;38:2:255:0:0m"),
    /// colon subparams for extended color and underline style — the shape our
    /// own parser and every modern probe accept. tmux and the truecolor
    /// detection scripts set a pen, query it back, and look for the echo
    fn sgr_report(&self) -> String {
        use std::fmt::Write;
        let cur = &self.grid.cursor;
        let a = &cur.attrs;
        let mut s = String::from("0");
        for (on, code) in [
            (a.bold, 1),
            (a.dim, 2),
            (a.italic, 3),
            (a.blink, 5),
            (a.inverse, 7),
            (a.hidden, 8),
            (a.strike, 9),
            (a.overline, 53),
        ] {
            if on {
                let _ = write!(s, ";{code}");
            }
        }
        match a.underline {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => s.push_str(";4"),
            UnderlineStyle::Double => s.push_str(";4:2"),
            UnderlineStyle::Curly => s.push_str(";4:3"),
            UnderlineStyle::Dotted => s.push_str(";4:4"),
            UnderlineStyle::Dashed => s.push_str(";4:5"),
        }
        let mut color = |base: u16, c: Color| {
            let _ = match c {
                Color::Indexed(n) if n < 8 && base < 58 => write!(s, ";{}", base - 8 + n as u16),
                Color::Indexed(n) if n < 16 && base < 58 => write!(s, ";{}", base + 52 + (n - 8) as u16),
                Color::Indexed(n) => write!(s, ";{base}:5:{n}"),
                Color::Rgb(r, g, b) => write!(s, ";{base}:2:{r}:{g}:{b}"),
                Color::Default | Color::DefaultBg => Ok(()),
            };
        };
        color(38, cur.fg);
        color(48, cur.bg);
        color(58, a.ul);
        s.push('m');
        s
    }

    /// the DECSCUSR value for the current cursor: 1/2 block, 3/4 underline,
    /// 5/6 bar, odd = blinking. an app that never set one gets the default (1)
    fn cursor_style_code(&self) -> u8 {
        if !self.grid.cursor.shape_set {
            return 1;
        }
        let base = match self.grid.cursor.shape {
            CursorShape::Block => 1,
            CursorShape::Underline => 3,
            CursorShape::Bar => 5,
        };
        base + u8::from(self.grid.cursor.shape_blink == Some(false))
    }

    /// active kitty keyboard protocol flags (top of the stack)
    pub fn kbd_flags(&self) -> u8 {
        *self.kbd_stack.last().unwrap_or(&0)
    }

    /// CSI = flags ; mode u: modify the active entry in place. mode 1 replaces,
    /// 2 sets the given bits, 3 clears them
    fn kbd_set(&mut self, flags: u8, mode: u16) {
        if let Some(top) = self.kbd_stack.last_mut() {
            match mode {
                3 => *top &= !flags,
                2 => *top |= flags & KBD_SUPPORTED,
                _ => *top = flags & KBD_SUPPORTED,
            }
        }
    }

    /// CSI > flags u: push a new active entry
    fn kbd_push(&mut self, flags: u8) {
        if self.kbd_stack.len() >= KBD_STACK_CAP {
            self.kbd_stack.remove(0);
        }
        self.kbd_stack.push(flags & KBD_SUPPORTED);
    }

    /// CSI < number u: pop entries, never removing the base entry
    fn kbd_pop(&mut self, n: usize) {
        for _ in 0..n {
            if self.kbd_stack.len() > 1 {
                self.kbd_stack.pop();
            } else {
                break;
            }
        }
    }

    /// whether a CursorMoved should be reported given if a button is held
    pub fn wants_motion(&self, button_down: bool) -> bool {
        matches!(self.mouse_proto, MouseProto::Any) || (self.mouse_proto == MouseProto::Button && button_down)
    }

    /// encode a mouse event for the active protocol; None if mouse mode is off.
    /// `btn`: 0=left 1=middle 2=right, 64=wheel-up 65=wheel-down; col/row 0-based
    /// modifiers is the xterm modifier bitfield (shift 4, alt 8, ctrl 16) OR'd
    /// into the button code; pass 0 for plain button-only reporting
    pub fn encode_mouse(&self, btn: u8, pressed: bool, motion: bool, col: usize, row: usize, modifiers: u8) -> Option<Vec<u8>> {
        if self.mouse_proto == MouseProto::Off {
            return None;
        }
        let (c, r) = (col + 1, row + 1);
        let md = modifiers as u32;
        // SGR when the app asked for it, or when X10 would clamp (coords > 223
        // become 255 and then corrupt any TUI that treats the byte as UTF-8).
        // falling back to SGR on overflow is what modern hosts do so a wide
        // pane never poisons the input stream with high-bit X10 bytes
        let use_sgr = self.mouse_sgr || c > 223 || r > 223;
        if use_sgr {
            let cb = btn as u32 + md + if motion { 32 } else { 0 };
            let m = if pressed { 'M' } else { 'm' };
            Some(format!("\x1b[<{cb};{c};{r}{m}").into_bytes())
        } else {
            // legacy X10: release is button 3; values offset by 32
            let cb = if pressed { btn as u32 + md + if motion { 32 } else { 0 } } else { 3 + md };
            let enc = |v: u32| -> u8 { (v + 32) as u8 };
            Some(vec![0x1b, b'[', b'M', enc(cb), enc(c as u32), enc(r as u32)])
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.grid.resize(rows, cols);
        if let Some(p) = self.saved_primary.as_mut() {
            p.resize(rows, cols);
        }
        self.dirty = true;
    }

    fn enter_alt(&mut self) {
        if self.using_alt {
            return;
        }
        let (rows, cols) = (self.grid.rows, self.grid.cols);
        let alt = Grid::new(rows, cols);
        let primary = std::mem::replace(&mut self.grid, alt);
        self.saved_primary = Some(primary);
        self.using_alt = true;
    }

    fn leave_alt(&mut self) {
        if !self.using_alt {
            return;
        }
        if let Some(primary) = self.saved_primary.take() {
            self.grid = primary;
        }
        self.using_alt = false;
        // a fullscreen app just exited; drop the interaction modes it may have
        // set but the shell never wants, so they can't bleed into the prompt
        // (stray mouse reports, kitty-encoded keys, application arrow keys,
        // a stuck DEC 2026 frame). bracketed paste is left alone — shells use it
        self.mouse_proto = MouseProto::Off;
        self.mouse_sgr = false;
        self.app_cursor_keys = false;
        self.kbd_stack = vec![0];
        self.sync_output = false;
        // focus reporting is rarely wanted at a bare prompt; a TUI that needs
        // it re-enables on the next entry, and leaving it on makes some hosts
        // emit CSI I/O that land as garbage in line editors
        self.focus_events = false;
    }

    fn set_mode(&mut self, private: bool, mode: u16, enable: bool) {
        if private {
            match mode {
                1 => self.app_cursor_keys = enable,
                6 => self.grid.set_origin_mode(enable),
                7 => {
                    // DECAWM: off pins prints at the right margin (tput rmam)
                    self.grid.autowrap = enable;
                    if !enable {
                        self.grid.cursor.wrap_pending = false;
                    }
                }
                // ATT610 cursor blink (vim's guicursor blinkon/blinkoff path)
                12 => self.grid.cursor.shape_blink = Some(enable),
                25 => self.grid.cursor.visible = enable,
                80 => self.sixel_display_mode = enable,
                2026 => self.sync_output = enable,
                1000 => self.mouse_proto = if enable { MouseProto::Normal } else { MouseProto::Off },
                1002 => self.mouse_proto = if enable { MouseProto::Button } else { MouseProto::Off },
                1003 => self.mouse_proto = if enable { MouseProto::Any } else { MouseProto::Off },
                1006 => self.mouse_sgr = enable,
                1004 => self.focus_events = enable,
                1007 => self.alt_scroll = enable,
                2004 => self.bracketed_paste = enable,
                47 | 1047 => {
                    if enable {
                        self.enter_alt()
                    } else {
                        self.leave_alt()
                    }
                }
                1049 => {
                    if enable {
                        self.enter_alt();
                        self.grid.erase_in_display(2);
                        self.grid.goto(0, 0);
                    } else {
                        self.leave_alt();
                    }
                }
                _ => {}
            }
        } else if mode == 4 {
            // IRM: insert mode (old full-screen editors and vttest)
            self.grid.insert_mode = enable;
        }
    }

    fn apply_sgr(&mut self, groups: &[&[u16]]) {
        let cur = &mut self.grid.cursor;
        if groups.is_empty() {
            cur.fg = Color::Default;
            cur.bg = Color::DefaultBg;
            cur.attrs = Default::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            let code = g.first().copied().unwrap_or(0);
            // colon-encoded extended color is self-contained in one group
            if g.len() > 1 && (code == 38 || code == 48 || code == 58) {
                if let Some(color) = parse_ext_color_slice(&g[1..]) {
                    match code {
                        38 => cur.fg = color,
                        48 => cur.bg = color,
                        // 58: underline color (git-delta, LSP squiggles)
                        _ => cur.attrs.ul = color,
                    }
                }
                i += 1;
                continue;
            }
            // colon-encoded underline style, e.g. 4:3 (curly)
            if g.len() > 1 && code == 4 {
                cur.attrs.underline = match g.get(1).copied().unwrap_or(1) {
                    0 => UnderlineStyle::None,
                    2 => UnderlineStyle::Double,
                    3 => UnderlineStyle::Curly,
                    4 => UnderlineStyle::Dotted,
                    5 => UnderlineStyle::Dashed,
                    _ => UnderlineStyle::Single,
                };
                i += 1;
                continue;
            }
            match code {
                0 => {
                    cur.fg = Color::Default;
                    cur.bg = Color::DefaultBg;
                    cur.attrs = Default::default();
                }
                1 => cur.attrs.bold = true,
                2 => cur.attrs.dim = true,
                3 => cur.attrs.italic = true,
                4 => cur.attrs.underline = UnderlineStyle::Single,
                5 | 6 => cur.attrs.blink = true,
                7 => cur.attrs.inverse = true,
                8 => cur.attrs.hidden = true,
                9 => cur.attrs.strike = true,
                22 => {
                    cur.attrs.bold = false;
                    cur.attrs.dim = false;
                }
                23 => cur.attrs.italic = false,
                21 => cur.attrs.underline = UnderlineStyle::Double,
                24 => cur.attrs.underline = UnderlineStyle::None,
                25 => cur.attrs.blink = false,
                27 => cur.attrs.inverse = false,
                28 => cur.attrs.hidden = false,
                29 => cur.attrs.strike = false,
                30..=37 => cur.fg = Color::Indexed((code - 30) as u8),
                39 => cur.fg = Color::Default,
                40..=47 => cur.bg = Color::Indexed((code - 40) as u8),
                49 => cur.bg = Color::DefaultBg,
                53 => cur.attrs.overline = true,
                55 => cur.attrs.overline = false,
                59 => cur.attrs.ul = Color::Default,
                90..=97 => cur.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => cur.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let set = |cur: &mut crate::grid::Cursor, color: Color| match code {
                        38 => cur.fg = color,
                        48 => cur.bg = color,
                        _ => cur.attrs.ul = color,
                    };
                    // semicolon form: read following groups
                    let kind = groups.get(i + 1).and_then(|g| g.first().copied());
                    match kind {
                        Some(5) => {
                            if let Some(n) = groups.get(i + 2).and_then(|g| g.first().copied()) {
                                set(cur, Color::Indexed(n as u8));
                                i += 3;
                                continue;
                            }
                        }
                        Some(2) => {
                            let r = groups.get(i + 2).and_then(|g| g.first().copied());
                            let gr = groups.get(i + 3).and_then(|g| g.first().copied());
                            let b = groups.get(i + 4).and_then(|g| g.first().copied());
                            if let (Some(r), Some(gn), Some(b)) = (r, gr, b) {
                                set(cur, Color::Rgb(r as u8, gn as u8, b as u8));
                                i += 5;
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn parse_ext_color_slice(s: &[u16]) -> Option<Color> {
    // s is the tail after 38/48: e.g. [5, n] or [2, r, g, b] or [2, cs, r, g, b]
    match s.first().copied()? {
        5 => Some(Color::Indexed(*s.get(1)? as u8)),
        2 => {
            // some encoders insert a colorspace id: [2, cs, r, g, b]
            let (r, g, b) = if s.len() >= 5 {
                (s[2], s[3], s[4])
            } else {
                (*s.get(1)?, *s.get(2)?, *s.get(3)?)
            };
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

/// minimal base64 decode for OSC 52 clipboard payloads (skips padding and
/// whitespace, returns None on an invalid character); also reused by the kitty
/// graphics scanner for image payloads
pub(crate) fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        buf = (buf << 6) | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// flatten OSC notification params into one displayable line: parts joined
/// with ": " (title: body), control bytes dropped, bounded so a hostile
/// program can't grow the status readout without limit
fn notification_text(parts: &[&[u8]]) -> String {
    let mut out = String::new();
    for part in parts {
        let s = String::from_utf8_lossy(part);
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.extend(s.chars().filter(|c| !c.is_control()));
        if out.len() >= 200 {
            break;
        }
    }
    if out.len() > 200 {
        let mut cut = 200;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

fn param_at(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Charset {
    Ascii,
    DecGraphics,
}

/// what an open DCS string is feeding
enum Dcs {
    /// sixel graphics decode (DCS q)
    Sixel(Box<crate::sixel::SixelDecoder>),
    /// DECRQSS status request (DCS $ q); the payload names the setting
    Rqss(Vec<u8>),
    /// XTGETTCAP (DCS + q); the payload is ';'-joined hex-encoded cap names
    Tcap(Vec<u8>),
}

/// a DECRQSS request is at most two bytes; anything longer is garbage and
/// only needs to stay bounded until the invalid reply
const RQSS_CAP: usize = 8;
/// XTGETTCAP requests are a handful of short names; bound a hostile stream
const TCAP_CAP: usize = 512;

/// terminfo capabilities XTGETTCAP answers — the set nvim and friends probe
/// to detect truecolor and styled underlines when terminfo can't be trusted
/// (e.g. over ssh). booleans report as a bare name, per xterm/kitty
fn tcap_value(name: &[u8]) -> Option<&'static [u8]> {
    match name {
        b"TN" | b"name" => Some(b"xterm-256color"),
        b"Co" | b"colors" => Some(b"256"),
        // boolean truecolor flags (tmux's Tc, terminfo's RGB)
        b"Tc" | b"RGB" | b"Su" => Some(b""),
        b"smulx" => Some(b"\x1b[4:%p1%dm"),
        b"setrgbf" => Some(b"\x1b[38:2:%p1%d:%p2%d:%p3%dm"),
        b"setrgbb" => Some(b"\x1b[48:2:%p1%d:%p2%d:%p3%dm"),
        _ => None,
    }
}

fn hex_decode(s: &[u8]) -> Option<Vec<u8>> {
    fn nib(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.chunks_exact(2).map(|p| Some(nib(p[0])? << 4 | nib(p[1])?)).collect()
}

fn hex_encode(s: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// the DEC special-graphics set (ESC ( 0): maps ASCII into the box-drawing /
/// line glyphs legacy curses apps draw with via SO/SI
fn dec_special_graphics(c: char) -> char {
    match c {
        '`' => '\u{25c6}',
        'a' => '\u{2592}',
        'b' => '\u{2409}',
        'c' => '\u{240c}',
        'd' => '\u{240d}',
        'e' => '\u{240a}',
        'f' => '\u{00b0}',
        'g' => '\u{00b1}',
        'h' => '\u{2424}',
        'i' => '\u{240b}',
        'j' => '\u{2518}',
        'k' => '\u{2510}',
        'l' => '\u{250c}',
        'm' => '\u{2514}',
        'n' => '\u{253c}',
        'o' => '\u{23ba}',
        'p' => '\u{23bb}',
        'q' => '\u{2500}',
        'r' => '\u{23bc}',
        's' => '\u{23bd}',
        't' => '\u{251c}',
        'u' => '\u{2524}',
        'v' => '\u{2534}',
        'w' => '\u{252c}',
        'x' => '\u{2502}',
        'y' => '\u{2264}',
        'z' => '\u{2265}',
        '{' => '\u{03c0}',
        '|' => '\u{2260}',
        '}' => '\u{00a3}',
        '~' => '\u{00b7}',
        _ => c,
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        let mapped = self.map_charset(c);
        self.grid.put_char(mapped);
        // REP repeats the glyph as presented, so store the post-charset glyph
        self.last_print = Some(mapped);
        self.dirty = true;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => self.grid.backspace(),
            0x09 => self.grid.tab(),
            0x0a..=0x0c => self.grid.linefeed(),
            0x0d => self.grid.carriage_return(),
            0x0e => self.gl = 1, // SO -> invoke g1
            0x0f => self.gl = 0, // SI -> invoke g0
            _ => {}
        }
        self.dirty = true;
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        match action {
            'A' => self.grid.move_up(param_at(params, 0, 1) as usize),
            'B' | 'e' => self.grid.move_down(param_at(params, 0, 1) as usize),
            'C' | 'a' => self.grid.move_right(param_at(params, 0, 1) as usize),
            'D' => self.grid.move_left(param_at(params, 0, 1) as usize),
            'E' => {
                self.grid.move_down(param_at(params, 0, 1) as usize);
                self.grid.carriage_return();
            }
            'F' => {
                self.grid.move_up(param_at(params, 0, 1) as usize);
                self.grid.carriage_return();
            }
            'G' | '`' => {
                let col = param_at(params, 0, 1) as usize - 1;
                let row = self.grid.cursor.row;
                self.grid.goto(row, col);
            }
            'd' => {
                let row = param_at(params, 0, 1) as usize - 1;
                let col = self.grid.cursor.col;
                self.grid.goto_addressed(row, col);
            }
            'H' | 'f' => {
                let row = param_at(params, 0, 1) as usize - 1;
                let col = param_at(params, 1, 1) as usize - 1;
                self.grid.goto_addressed(row, col);
            }
            'J' => self.grid.erase_in_display(param_at(params, 0, 0)),
            'K' => self.grid.erase_in_line(param_at(params, 0, 0)),
            'L' => self.grid.insert_lines(param_at(params, 0, 1) as usize),
            'M' => self.grid.delete_lines(param_at(params, 0, 1) as usize),
            '@' => self.grid.insert_chars(param_at(params, 0, 1) as usize),
            'P' => self.grid.delete_chars(param_at(params, 0, 1) as usize),
            'X' => self.grid.erase_chars(param_at(params, 0, 1) as usize),
            // CHT / CBT: forward / backward over tab stops
            'I' => self.grid.tab_forward(param_at(params, 0, 1) as usize),
            'Z' => self.grid.tab_backward(param_at(params, 0, 1) as usize),
            // TBC: clear the stop at the cursor (0) or all stops (3)
            'g' => self.grid.clear_tab_stops(param_at(params, 0, 0)),
            'b' => {
                // REP: repeat the last presented glyph N times (already charset-mapped)
                if let Some(mapped) = self.last_print {
                    let n = (param_at(params, 0, 1) as usize).min(self.grid.cols * self.grid.rows);
                    for _ in 0..n {
                        self.grid.put_char(mapped);
                    }
                }
            }
            // XTSMGRAPHICS (CSI ? Pi ; Pa ; Pv S): sixel tools size their
            // output from these. registers and geometry are fixed properties
            // here, so every action reads back the same values
            'S' if private => {
                match param_at(params, 0, 0) {
                    1 => self.responses.extend_from_slice(b"\x1b[?1;0;256S"),
                    2 => self
                        .responses
                        .extend_from_slice(format!("\x1b[?2;0;{};{}S", crate::sixel::MAX_W, crate::sixel::MAX_H).as_bytes()),
                    // ReGIS and anything else: status 1 = error in item
                    i => self.responses.extend_from_slice(format!("\x1b[?{};1S", i).as_bytes()),
                }
            }
            'S' => self.grid.scroll_up(param_at(params, 0, 1) as usize),
            'T' => self.grid.scroll_down(param_at(params, 0, 1) as usize),
            // a leading > or ? marks XTMODKEYS/XTQMODKEYS, not SGR — swallowing
            // them keeps a tmux/neovim modifyOtherKeys probe from being applied
            // as dim+underline attribute garbage
            'm' if intermediates.is_empty() => {
                // borrow the param groups into a stack array (no per-sequence heap
                // alloc on the parse hot path); 32 covers any real SGR run
                let mut groups: [&[u16]; 32] = [&[]; 32];
                let mut n = 0;
                for p in params.iter() {
                    if n == groups.len() {
                        break;
                    }
                    groups[n] = p;
                    n += 1;
                }
                self.apply_sgr(&groups[..n]);
            }
            'r' => {
                let top = param_at(params, 0, 1) as usize - 1;
                let bottom = param_at(params, 1, self.grid.rows as u16) as usize - 1;
                self.grid.set_scroll_region(top, bottom);
            }
            // SM/RM: every Ps in CSI ? Ps ; Ps ; ... h/l must apply. real TUIs
            // enable mouse as a single combined sequence
            // (`CSI ? 1000;1002;1003;1006 h`); only taking the first left sgr
            // and any-motion off, so reports went out as clamped X10 bytes
            'h' | 'l' => {
                let enable = action == 'h';
                let mut saw = false;
                for p in params.iter() {
                    let mode = p.first().copied().unwrap_or(0);
                    // a zero param is xterm's "default" and is not a real mode
                    // number we track — skip rather than calling set_mode(0)
                    if mode == 0 {
                        continue;
                    }
                    self.set_mode(private, mode, enable);
                    saw = true;
                }
                if !saw {
                    self.set_mode(private, 0, enable);
                }
            }
            'n' => {
                let what = param_at(params, 0, 0);
                if what == 6 {
                    let r = self.grid.cursor.row + 1;
                    let c = self.grid.cursor.col + 1;
                    self.responses
                        .extend_from_slice(format!("\x1b[{};{}R", r, c).as_bytes());
                } else if what == 5 {
                    self.responses.extend_from_slice(b"\x1b[0n");
                }
            }
            'c' => {
                if intermediates.first() == Some(&b'>') {
                    // DA2 secondary device attributes: a VT220-class id, version 0
                    self.responses.extend_from_slice(b"\x1b[>41;0;0c");
                } else if !private {
                    // DA1: VT220-class (62) with sixel (4) and ANSI color (22) —
                    // lsix/chafa/img2sixel detect sixel from the ";4"
                    self.responses.extend_from_slice(b"\x1b[?62;4;22c");
                }
            }
            // XTVERSION (CSI > q): report name + version as DCS > | text ST
            'q' if intermediates.first() == Some(&b'>') => {
                self.responses.extend_from_slice(
                    concat!("\x1bP>|termie ", env!("CARGO_PKG_VERSION"), "\x1b\\").as_bytes(),
                );
            }
            'q' if intermediates.first() == Some(&b' ') => {
                // DECSCUSR (CSI Ps SP q) cursor shape. read the raw param: 0 and 1
                // are both "blinking block" per spec, 2 steady block, 3/4
                // underline, 5/6 bar. param_at would coerce an explicit 0 to the
                // default and mishandle the 0 = block case, so read it directly
                let n = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                self.grid.cursor.shape = match n {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
                self.grid.cursor.shape_set = true;
                // odd params (and 0) are the blinking variants, even the steady
                self.grid.cursor.shape_blink = Some(matches!(n, 0 | 1 | 3 | 5));
            }
            's' => self.grid.save_cursor(),
            'u' => match intermediates.first() {
                // kitty keyboard: report the active flags (CSI ? u)
                Some(&b'?') => {
                    let f = self.kbd_flags();
                    self.responses
                        .extend_from_slice(format!("\x1b[?{}u", f).as_bytes());
                }
                // kitty keyboard: set flags on the active entry (CSI = flags ; mode u)
                Some(&b'=') => {
                    let flags = param_at(params, 0, 0) as u8;
                    let mode = param_at(params, 1, 1);
                    self.kbd_set(flags, mode);
                }
                // kitty keyboard: push a flags entry (CSI > flags u)
                Some(&b'>') => self.kbd_push(param_at(params, 0, 0) as u8),
                // kitty keyboard: pop flags entries (CSI < number u)
                Some(&b'<') => self.kbd_pop(param_at(params, 0, 1) as usize),
                // plain CSI u: SCO restore cursor
                _ => self.grid.restore_cursor(),
            },
            // DECSTR soft reset (CSI ! p)
            'p' if intermediates.first() == Some(&b'!') => self.soft_reset(),
            // DECRQM mode query (CSI ? Ps $ p) -> DECRPM report
            'p' if private && intermediates.get(1) == Some(&b'$') => {
                let m = param_at(params, 0, 0);
                let state = self.dec_mode_state(m);
                self.responses
                    .extend_from_slice(format!("\x1b[?{};{}$y", m, state).as_bytes());
            }
            // DECRQM for ANSI modes (CSI Ps $ p): only IRM is tracked
            'p' if !private && intermediates.first() == Some(&b'$') => {
                let m = param_at(params, 0, 0);
                let state = match m {
                    4 => {
                        if self.grid.insert_mode {
                            1
                        } else {
                            2
                        }
                    }
                    _ => 0,
                };
                self.responses
                    .extend_from_slice(format!("\x1b[{};{}$y", m, state).as_bytes());
            }
            // XTWINOPS size reports; the resize/iconify/title-stack ops are
            // deliberately ignored
            't' if !private => match param_at(params, 0, 0) {
                // text area in pixels: reply CSI 4 ; height ; width t
                14 => {
                    let (cw, ch) = self.cell_px;
                    if cw > 0 && ch > 0 {
                        let w = self.grid.cols * cw as usize;
                        let h = self.grid.rows * ch as usize;
                        self.responses.extend_from_slice(format!("\x1b[4;{};{}t", h, w).as_bytes());
                    }
                }
                // cell size in pixels: reply CSI 6 ; height ; width t
                16 => {
                    let (cw, ch) = self.cell_px;
                    if cw > 0 && ch > 0 {
                        self.responses.extend_from_slice(format!("\x1b[6;{};{}t", ch, cw).as_bytes());
                    }
                }
                // text area in cells: reply CSI 8 ; rows ; cols t
                18 => {
                    self.responses.extend_from_slice(
                        format!("\x1b[8;{};{}t", self.grid.rows, self.grid.cols).as_bytes(),
                    );
                }
                _ => {}
            },
            _ => {}
        }
        self.dirty = true;
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // charset designation: ESC ( Fc -> g0, ESC ) Fc -> g1 (0 = DEC special
        // graphics, anything else treated as ascii)
        if let Some(&i) = intermediates.first()
            && (i == b'(' || i == b')')
        {
            let cs = if byte == b'0' { Charset::DecGraphics } else { Charset::Ascii };
            if i == b'(' {
                self.g0 = cs;
            } else {
                self.g1 = cs;
            }
            self.dirty = true;
            return;
        }
        // DECALN screen alignment test (ESC # 8): without this check the byte
        // would fall into the DECRC arm below and quietly move the cursor
        if intermediates.first() == Some(&b'#') {
            if byte == b'8' {
                self.grid.screen_alignment_test();
            }
            self.dirty = true;
            return;
        }
        match byte {
            b'M' => self.grid.reverse_index(),
            b'D' => self.grid.linefeed(),
            b'E' => {
                self.grid.linefeed();
                self.grid.carriage_return();
            }
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            // HTS: set a tab stop at the cursor column
            b'H' => self.grid.set_tab_stop(),
            b'c' => {
                // RIS full reset
                let (rows, cols) = (self.grid.rows, self.grid.cols);
                self.grid = Grid::new(rows, cols);
                self.saved_primary = None;
                self.using_alt = false;
                self.app_cursor_keys = false;
                self.bracketed_paste = false;
                self.alt_scroll = true;
                self.kbd_stack = vec![0];
                self.g0 = Charset::Ascii;
                self.g1 = Charset::Ascii;
                self.gl = 0;
                self.last_print = None;
                self.progress = None;
                self.dcs = None;
                self.sixel_display_mode = false;
                // dynamic OSC colors don't survive a hard reset (xterm)
                self.colors.clear();
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&kind) = params.first() else {
            return;
        };
        match kind {
            b"0" | b"2" => {
                if let Some(t) = params.get(1) {
                    // same control-strip + length bound as notifications: the
                    // title feeds the tab strip and the OS window title
                    let title = notification_text(&[t]);
                    if title != self.title {
                        self.title = title;
                        self.title_dirty = true;
                    }
                }
            }
            b"7" => {
                if let Some(u) = params.get(1) {
                    // real paths are small; bound the stored copy like the
                    // title/notify handlers so a hostile stream can't pin an
                    // oversized cwd string
                    let u = &u[..u.len().min(4096)];
                    self.cwd = Some(String::from_utf8_lossy(u).into_owned());
                    self.cwd_dirty = true;
                }
            }
            b"8" => {
                // OSC 8 ; params ; URI  — an empty URI ends the current link;
                // rejoin fields past the id in case the URI itself contains ';'
                let uri = params.get(2..).map(|rest| {
                    rest.iter()
                        .map(|p| String::from_utf8_lossy(p))
                        .collect::<Vec<_>>()
                        .join(";")
                });
                self.grid.set_link(uri.as_deref());
            }
            b"52" => {
                // OSC 52 ; targets ; base64 — clipboard write. ignore "?" (a
                // read query) so a remote program can't exfiltrate the clipboard.
                // bounded at 1MB decoded so a hostile stream can't stuff the OS
                // clipboard without limit; the app shows a notice when this
                // lands so a silent hijack is at least visible
                const MAX_CLIPBOARD: usize = 1024 * 1024;
                if let Some(&data) = params.get(2)
                    && data != b"?"
                    && data.len() <= MAX_CLIPBOARD / 3 * 4
                    && let Some(bytes) = base64_decode(data)
                {
                    self.clipboard = Some(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            b"10" | b"11" | b"12" => {
                // OSC 10/11/12 — query (`?`) or SET the default fg / bg /
                // cursor color; per xterm, extra params set the successive
                // dynamic colors (OSC 10;fg;bg sets both). sets layer over the
                // theme at paint and reset via OSC 110-112
                let base = (kind[1] - b'0') as usize;
                for (i, p) in params.iter().skip(1).enumerate() {
                    let slot = base + i;
                    if slot > 2 {
                        break;
                    }
                    if p.len() == 1 && p[0] == b'?' {
                        // answer an overridden color ourselves; the theme's
                        // value needs the app's palette, so it round-trips
                        let (set, req, code) = match slot {
                            0 => (self.colors.fg, ColorReq::Fg, "10"),
                            1 => (self.colors.bg, ColorReq::Bg, "11"),
                            _ => (self.colors.cursor, ColorReq::Cursor, "12"),
                        };
                        match set {
                            Some(c) => self.responses.extend_from_slice(&color_reply(code, c)),
                            None => self.color_queries.push(req),
                        }
                    } else if let Some(c) = parse_color_spec(p) {
                        match slot {
                            0 => self.colors.fg = Some(c),
                            1 => self.colors.bg = Some(c),
                            _ => self.colors.cursor = Some(c),
                        }
                    }
                }
            }
            b"4" => {
                // OSC 4 ; n ; spec [; n ; spec ...] — remap palette entry n
                // (base16-shell / pywal territory); `?` as the spec queries it
                let mut i = 1;
                while let (Some(np), Some(sp)) = (params.get(i), params.get(i + 1)) {
                    if let Some(n) = std::str::from_utf8(np).ok().and_then(|s| s.parse::<u8>().ok())
                    {
                        if sp.len() == 1 && sp[0] == b'?' {
                            match self.colors.ansi(n) {
                                Some(c) => self
                                    .responses
                                    .extend_from_slice(&color_reply(&format!("4;{n}"), c)),
                                None => self.color_queries.push(ColorReq::Ansi(n)),
                            }
                        } else if let Some(c) = parse_color_spec(sp) {
                            self.colors.set_ansi(n, c);
                        }
                    }
                    i += 2;
                }
            }
            b"104" => {
                // OSC 104 — reset remapped palette entries (all when bare)
                if params.len() <= 1 {
                    self.colors.reset_ansi(None);
                } else {
                    for p in &params[1..] {
                        if let Some(n) =
                            std::str::from_utf8(p).ok().and_then(|s| s.parse::<u8>().ok())
                        {
                            self.colors.reset_ansi(Some(n));
                        }
                    }
                }
            }
            b"110" => self.colors.fg = None,
            b"111" => self.colors.bg = None,
            b"112" => self.colors.cursor = None,
            b"9" => {
                // OSC 9 ; 4 ; state ; percent — ConEmu taskbar progress
                if params.get(1).copied() == Some(b"4") {
                    let num = |i: usize| {
                        params
                            .get(i)
                            .and_then(|p| std::str::from_utf8(p).ok())
                            .and_then(|s| s.parse::<u8>().ok())
                            .unwrap_or(0)
                    };
                    self.progress = match num(2) {
                        0 => None,
                        s @ (1 | 2 | 4) => Some((s, num(3).min(100))),
                        3 => Some((3, 0)),
                        _ => self.progress,
                    };
                } else if params.get(1).copied() == Some(b"9") {
                    // cmd's shell integration reports its current directory as
                    // OSC 9;9;$P, which has a plain Windows path instead of OSC 7's URI
                    if let Some(path) = params.get(2).filter(|path| !path.is_empty()) {
                        // bounded like OSC 7 above
                        let path = &path[..path.len().min(4096)];
                        self.cwd = Some(String::from_utf8_lossy(path).into_owned());
                        self.cwd_dirty = true;
                    }
                } else if params.get(1).is_some_and(|p| !p.is_empty() && !p.iter().all(u8::is_ascii_digit)) {
                    // OSC 9 ; message — an iTerm2-style notification; ring it
                    // through the bell so it dots the tab and flashes the
                    // taskbar, and keep the text for the status-bar readout.
                    // numeric-first bodies are other ConEmu subcommands
                    // (9;9 cwd, 9;10 …), not toasts
                    self.bell = true;
                    self.notify = Some(notification_text(&params[1..]));
                }
            }
            b"777" => {
                // rxvt/tmux notification convention: OSC 777 ; notify ; title ; body
                if params.get(1).copied() == Some(b"notify") {
                    self.bell = true;
                    self.notify = Some(notification_text(&params[2..]));
                }
            }
            b"133" => {
                if let Some(m) = params.get(1) {
                    self.last_osc133 = match m.first() {
                        Some(b'A') => {
                            // record the prompt row for jump nav; skip on the alt
                            // screen (full-screen apps aren't a command history)
                            if !self.using_alt {
                                self.grid.mark_prompt();
                            }
                            Some(Osc133::PromptStart)
                        }
                        Some(b'B') => Some(Osc133::PromptEnd),
                        Some(b'C') => {
                            self.cmd_running = true;
                            Some(Osc133::CommandStart)
                        }
                        Some(b'D') => {
                            let code = params
                                .get(2)
                                .and_then(|c| std::str::from_utf8(c).ok())
                                .and_then(|s| s.parse::<i32>().ok());
                            // stamp the mark so the scrollbar can flag the
                            // failed command's spot in history
                            if !self.using_alt {
                                self.grid.set_last_prompt_exit(code);
                            }
                            self.cmd_running = false;
                            self.cmd_done = Some(code);
                            Some(Osc133::CommandDone(code))
                        }
                        _ => self.last_osc133,
                    };
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.dcs = match (action, intermediates) {
            // DCS P1;P2;P3 q — sixel. unwritten pixels stay transparent, which
            // over the cell background matches both P2 background modes
            ('q', []) => Some(Dcs::Sixel(Box::default())),
            // DCS $ q — DECRQSS, the payload names the queried setting
            ('q', [b'$']) => Some(Dcs::Rqss(Vec::new())),
            // DCS + q — XTGETTCAP terminfo capability query
            ('q', [b'+']) => Some(Dcs::Tcap(Vec::new())),
            _ => None,
        };
    }

    fn put(&mut self, byte: u8) {
        match self.dcs.as_mut() {
            Some(Dcs::Sixel(dec)) => dec.put(byte),
            Some(Dcs::Rqss(req)) if req.len() < RQSS_CAP => req.push(byte),
            Some(Dcs::Tcap(req)) if req.len() < TCAP_CAP => req.push(byte),
            // a full rqss/tcap buffer drops further bytes; no dcs ignores them
            _ => {}
        }
    }

    fn unhook(&mut self) {
        match self.dcs.take() {
            Some(Dcs::Sixel(dec)) => {
                let Some((w, h, rgba)) = dec.finish() else { return };
                let id = self.images.insert(w, h, rgba);
                if self.sixel_display_mode {
                    // DECSDM: the image pins to the upper-left, cursor untouched
                    self.grid.place_image_at(id, 0, 0, 0, 0, 0);
                } else {
                    // sixel scrolling (the default): anchor at the cursor, then
                    // move the cursor to the line below the image, scrolling as
                    // needed. an unknown cell height (renderer not attached
                    // yet) assumes 20 px
                    self.grid.place_image(id, 0, 0, 0);
                    let cell_h = if self.cell_px.1 > 0 { self.cell_px.1 as usize } else { 20 };
                    for _ in 0..(h as usize).div_ceil(cell_h) {
                        self.grid.linefeed();
                    }
                }
                self.dirty = true;
            }
            Some(Dcs::Rqss(req)) => {
                // reply DCS 1 $ r <setting> ST when the setting is known,
                // DCS 0 $ r ST otherwise
                let body = match req.as_slice() {
                    b"m" => Some(self.sgr_report()),
                    b"r" => Some(format!("{};{}r", self.grid.region_top + 1, self.grid.region_bottom + 1)),
                    b" q" => Some(format!("{} q", self.cursor_style_code())),
                    // DECSCL: VT220-class with 7-bit responses, matching DA1
                    b"\"p" => Some("62;1\"p".to_string()),
                    // DECSCA: character protection is never on
                    b"\"q" => Some("0\"q".to_string()),
                    _ => None,
                };
                let reply = match body {
                    Some(b) => format!("\x1bP1$r{b}\x1b\\"),
                    None => "\x1bP0$r\x1b\\".to_string(),
                };
                self.responses.extend_from_slice(reply.as_bytes());
            }
            Some(Dcs::Tcap(req)) => {
                // hex names in, hex name=value pairs out; booleans echo the
                // bare name. reply 1 when anything resolved, 0 otherwise
                let mut parts: Vec<String> = Vec::new();
                for name_hex in req.split(|&b| b == b';') {
                    let Some(name) = hex_decode(name_hex) else { continue };
                    let Some(val) = tcap_value(&name) else { continue };
                    if val.is_empty() {
                        parts.push(hex_encode(&name));
                    } else {
                        parts.push(format!("{}={}", hex_encode(&name), hex_encode(val)));
                    }
                }
                let reply = if parts.is_empty() {
                    "\x1bP0+r\x1b\\".to_string()
                } else {
                    format!("\x1bP1+r{}\x1b\\", parts.join(";"))
                };
                self.responses.extend_from_slice(reply.as_bytes());
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    fn feed(t: &mut Terminal, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    #[test]
    fn dec_graphics_charset_and_so_si() {
        // ESC ( 0 designates DEC special graphics into g0; 'q' -> horizontal line
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"\x1b(0q");
        assert_eq!(t.grid.lines[0][0].c, '\u{2500}');
        // SO/SI switch the active set: g1 = graphics, SO invokes g1, SI back to g0
        let mut t2 = Terminal::new(4, 10);
        feed(&mut t2, b"\x1b)0\x0ex\x0fx");
        assert_eq!(t2.grid.lines[0][0].c, '\u{2502}'); // g1 graphics: x -> │
        assert_eq!(t2.grid.lines[0][1].c, 'x'); // g0 ascii: literal x
    }

    #[test]
    fn rep_repeats_last_glyph() {
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"A\x1b[3b");
        let row: String = t.grid.lines[0].iter().take(4).map(|c| c.c).collect();
        assert_eq!(row, "AAAA");
    }

    #[test]
    fn da2_response_and_decstr_resets_charset() {
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"\x1b[>c");
        assert_eq!(t.responses, b"\x1b[>41;0;0c");
        // DECSTR (CSI ! p) restores g0 to ascii after a graphics designation
        let mut t2 = Terminal::new(4, 10);
        feed(&mut t2, b"\x1b(0\x1b[!pq");
        assert_eq!(t2.grid.lines[0][0].c, 'q'); // literal, not a box char
    }

    #[test]
    fn mouse_modifiers_or_into_button() {
        let mut t = Terminal::new(4, 10);
        t.mouse_proto = MouseProto::Normal;
        t.mouse_sgr = true;
        // shift(4) | ctrl(16) OR into button 0 -> 20
        assert_eq!(
            t.encode_mouse(0, true, false, 0, 0, 4 | 16).unwrap(),
            b"\x1b[<20;1;1M".to_vec()
        );
        // no modifiers -> plain button-only report (unchanged behavior)
        assert_eq!(
            t.encode_mouse(0, true, false, 0, 0, 0).unwrap(),
            b"\x1b[<0;1;1M".to_vec()
        );
    }

    #[test]
    fn decrqm_reports_mode_state() {
        let mut t = Terminal::new(4, 10);
        // origin mode starts reset -> 2
        feed(&mut t, b"\x1b[?6$p");
        assert_eq!(t.responses, b"\x1b[?6;2$y");
        t.responses.clear();
        // set it, then query -> 1
        feed(&mut t, b"\x1b[?6h\x1b[?6$p");
        assert_eq!(t.responses, b"\x1b[?6;1$y");
        t.responses.clear();
        // unrecognized mode -> 0
        feed(&mut t, b"\x1b[?9999$p");
        assert_eq!(t.responses, b"\x1b[?9999;0$y");
    }

    #[test]
    fn decrqm_reports_default_and_mouse_modes() {
        let mut t = Terminal::new(4, 10);
        // cursor visibility (mode 25) defaults on -> 1
        feed(&mut t, b"\x1b[?25$p");
        assert_eq!(t.responses, b"\x1b[?25;1$y");
        t.responses.clear();
        // set mouse button-tracking (1002), query reports set
        feed(&mut t, b"\x1b[?1002h\x1b[?1002$p");
        assert_eq!(t.responses, b"\x1b[?1002;1$y");
        t.responses.clear();
        // a mouse mode that is not set reports reset
        feed(&mut t, b"\x1b[?1003$p");
        assert_eq!(t.responses, b"\x1b[?1003;2$y");
    }

    #[test]
    fn rep_repeats_charset_mapped_and_clamps() {
        // REP repeats the POST-charset glyph: ESC(0 maps 'q'->box, CSI 2 b repeats it
        let mut t = Terminal::new(2, 6);
        feed(&mut t, b"\x1b(0q\x1b[2b");
        assert_eq!(t.grid.lines[0][0].c, '\u{2500}');
        assert_eq!(t.grid.lines[0][1].c, '\u{2500}');
        assert_eq!(t.grid.lines[0][2].c, '\u{2500}');
        // a huge REP count is clamped to cols*rows and must not panic
        let mut t2 = Terminal::new(2, 6);
        feed(&mut t2, b"X\x1b[99999b");
        assert!(t2.grid.cursor.row < t2.grid.rows && t2.grid.cursor.col < t2.grid.cols);
    }

    #[test]
    fn decstr_resets_full_surface() {
        let mut t = Terminal::new(10, 10);
        // hide cursor, set a scroll region, origin mode, a red+bold pen, bracketed paste
        feed(&mut t, b"\x1b[?25l\x1b[3;7r\x1b[?6h\x1b[31;1m\x1b[?2004h");
        assert!(!t.grid.cursor.visible);
        assert!(t.grid.origin_mode);
        assert!(t.bracketed_paste);
        feed(&mut t, b"\x1b[!p"); // DECSTR soft reset
        assert!(t.grid.cursor.visible); // DECTCEM re-show
        assert!(!t.grid.origin_mode);
        assert!(!t.bracketed_paste);
        assert_eq!(t.grid.cursor.fg, Color::Default); // SGR pen reset
        assert!(!t.grid.cursor.attrs.bold);
    }

    #[test]
    fn ris_resets_grid_and_modes() {
        let mut t = Terminal::new(4, 8);
        feed(&mut t, b"\x1b[?1049h"); // alt screen
        feed(&mut t, b"\x1b[>1u"); // push a kitty keyboard flag
        feed(&mut t, b"\x1b(0X"); // graphics charset + a char
        feed(&mut t, b"\x1bc"); // RIS
        assert!(!t.using_alt);
        assert_eq!(t.kbd_flags(), 0);
        assert_eq!(t.grid.lines[0][0].c, ' '); // grid blanked
        // charset reset: 'q' is literal again, not a box char
        feed(&mut t, b"q");
        assert_eq!(t.grid.lines[0][0].c, 'q');
    }

    #[test]
    fn decom_makes_cup_region_relative() {
        let mut t = Terminal::new(10, 5);
        feed(&mut t, b"\x1b[4;8r"); // scroll region rows 4..8 (0-based 3..7)
        feed(&mut t, b"\x1b[?6h"); // DECOM on -> cursor homes to the region top
        assert_eq!(t.grid.cursor.row, 3);
        feed(&mut t, b"\x1b[2;1HY"); // CUP row 2 region-relative -> absolute row 4
        assert_eq!(t.grid.lines[4][0].c, 'Y');
    }

    #[test]
    fn sgr_truecolor_and_print() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[38;2;255;0;0mhi");
        assert_eq!(t.grid.lines[0][0].c, 'h');
        assert_eq!(t.grid.lines[0][0].fg, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn cursor_position_and_erase() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[2;3Hx");
        assert_eq!(t.grid.lines[1][2].c, 'x');
    }

    #[test]
    fn dsr_reports_position() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[2;5H\x1b[6n");
        assert_eq!(t.responses, b"\x1b[2;5R");
    }

    #[test]
    fn decscusr_sets_shape_and_block_on_zero() {
        // CSI 2 SP q -> steady block; the SP (0x20) is the DECSCUSR intermediate
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[2 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Block);
        assert!(t.grid.cursor.shape_set);
        // the steady variant pins the blink off; the blinking variant pins it on
        assert_eq!(t.grid.cursor.shape_blink, Some(false));
        // CSI 5 SP q -> bar
        feed(&mut t, b"\x1b[5 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Bar);
        assert_eq!(t.grid.cursor.shape_blink, Some(true));
        // DECSTR releases the override back to the configured default
        feed(&mut t, b"\x1b[!p");
        assert_eq!(t.grid.cursor.shape_blink, None);
        // CSI 0 SP q -> default, which is a block (this was dead code before)
        feed(&mut t, b"\x1b[0 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Block);
        // CSI 4 SP q -> underline
        feed(&mut t, b"\x1b[4 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Underline);
    }

    #[test]
    fn sgr_underline_color_and_overline() {
        let mut t = Terminal::new(2, 20);
        // colon form 58:2::r:g:b (kitty/foot emit the colorspace slot)
        feed(&mut t, b"\x1b[4m\x1b[58:2::255:0:0mx");
        assert_eq!(t.grid.lines[0][0].attrs.ul, Color::Rgb(255, 0, 0));
        // semicolon form 58;5;n and the 59 reset
        feed(&mut t, b"\x1b[58;5;33my\x1b[59mz");
        assert_eq!(t.grid.lines[0][1].attrs.ul, Color::Indexed(33));
        assert_eq!(t.grid.lines[0][2].attrs.ul, Color::Default);
        // overline set/clear
        feed(&mut t, b"\x1b[53mo\x1b[55mp");
        assert!(t.grid.lines[0][3].attrs.overline);
        assert!(!t.grid.lines[0][4].attrs.overline);
        // SGR 0 clears both
        feed(&mut t, b"\x1b[53;58;5;1m\x1b[0mq");
        let a = t.grid.lines[0][5].attrs;
        assert!(!a.overline);
        assert_eq!(a.ul, Color::Default);
    }

    #[test]
    fn decawm_off_pins_the_margin() {
        let mut t = Terminal::new(2, 4);
        // autowrap off: printing past the margin overwrites the last column
        feed(&mut t, b"\x1b[?7labcdef");
        assert_eq!(t.grid.lines[0][3].c, 'f');
        assert_eq!(t.grid.cursor.row, 0);
        assert_eq!(t.grid.cursor.col, 3);
        // DECRQM reports it reset; re-enabling wraps again
        feed(&mut t, b"\x1b[?7$p");
        assert_eq!(t.responses, b"\x1b[?7;2$y");
        t.responses.clear();
        feed(&mut t, b"\x1b[?7hgh");
        assert_eq!(t.grid.lines[1][0].c, 'h');
    }

    #[test]
    fn xtmodkeys_is_not_applied_as_sgr() {
        let mut t = Terminal::new(2, 10);
        // tmux/neovim probe modifyOtherKeys with CSI > 4;2 m — it must not
        // land as SGR 4 (underline) + SGR 2 (dim)
        feed(&mut t, b"\x1b[>4;2mx");
        assert_eq!(t.grid.lines[0][0].c, 'x');
        assert_eq!(t.grid.lines[0][0].attrs.underline, UnderlineStyle::None);
        assert!(!t.grid.lines[0][0].attrs.dim);
        // plain SGR still applies
        feed(&mut t, b"\x1b[4my");
        assert_ne!(t.grid.lines[0][1].attrs.underline, UnderlineStyle::None);
    }

    #[test]
    fn xtversion_reports_name_and_version() {
        let mut t = Terminal::new(2, 10);
        feed(&mut t, b"\x1b[>0q");
        let want = format!("\x1bP>|termie {}\x1b\\", env!("CARGO_PKG_VERSION"));
        assert_eq!(t.responses, want.as_bytes());
    }

    #[test]
    fn xtwinops_reports_sizes() {
        let mut t = Terminal::new(24, 80);
        // cell count needs no pixel knowledge: CSI 8 ; rows ; cols t
        feed(&mut t, b"\x1b[18t");
        assert_eq!(t.responses, b"\x1b[8;24;80t");
        t.responses.clear();
        // pixel reports stay silent until the renderer feeds a cell size
        feed(&mut t, b"\x1b[14t\x1b[16t");
        assert!(t.responses.is_empty());
        t.set_cell_px(9, 20);
        // text area px: CSI 4 ; height ; width t — 24*20 x 80*9
        feed(&mut t, b"\x1b[14t");
        assert_eq!(t.responses, b"\x1b[4;480;720t");
        t.responses.clear();
        // cell px: CSI 6 ; height ; width t
        feed(&mut t, b"\x1b[16t");
        assert_eq!(t.responses, b"\x1b[6;20;9t");
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"primary");
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.using_alt);
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.using_alt);
        assert_eq!(t.grid.lines[0][0].c, 'p');
    }

    #[test]
    fn leaving_alt_screen_resets_interaction_modes() {
        let mut t = Terminal::new(4, 20);
        // a tui enters the alt screen and turns on mouse tracking + kitty keys
        feed(&mut t, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?1h\x1b[>1u\x1b[?2026h");
        assert_eq!(t.mouse_proto, MouseProto::Normal);
        assert!(t.mouse_sgr);
        assert!(t.focus_events);
        assert!(t.app_cursor_keys);
        assert!(t.sync_output);
        assert_eq!(t.kbd_flags(), 1);
        // leaving the alt screen must drop them so a stray click can't print a
        // mouse report and keys aren't kitty-encoded once the shell is back
        feed(&mut t, b"\x1b[?1049l");
        assert_eq!(t.mouse_proto, MouseProto::Off);
        assert!(!t.mouse_sgr);
        assert!(!t.focus_events);
        assert!(!t.app_cursor_keys);
        assert!(!t.sync_output);
        assert_eq!(t.kbd_flags(), 0);
    }

    #[test]
    fn combined_mouse_modes_all_apply() {
        // CSI ? 1000;1002;1003;1006 h is how real TUIs enable mouse — every
        // Ps must land, not only the first
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[?1000;1002;1003;1006h");
        assert_eq!(t.mouse_proto, MouseProto::Any);
        assert!(t.mouse_sgr);
        // SGR report for a mid-pane cell
        let b = t.encode_mouse(0, true, false, 10, 5, 0).unwrap();
        assert_eq!(b, b"\x1b[<0;11;6M");
        // disable the bundle
        feed(&mut t, b"\x1b[?1000;1002;1003;1006l");
        assert_eq!(t.mouse_proto, MouseProto::Off);
        assert!(!t.mouse_sgr);
    }

    #[test]
    fn x10_overflow_falls_back_to_sgr() {
        // without 1006, a cell past the X10 223 limit must not emit a clamped
        // high byte that poisons a UTF-8 input parser
        let mut t = Terminal::new(4, 300);
        feed(&mut t, b"\x1b[?1000h");
        assert!(!t.mouse_sgr);
        let b = t.encode_mouse(0, true, false, 250, 0, 0).unwrap();
        assert_eq!(b, b"\x1b[<0;251;1M");
        // still inside the X10 range: legacy encoding
        let b2 = t.encode_mouse(0, true, false, 10, 5, 0).unwrap();
        assert_eq!(b2, vec![0x1b, b'[', b'M', 32, 10 + 32 + 1, 5 + 32 + 1]);
    }

    #[test]
    fn soft_reset_clears_mouse_and_sync() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[?1003;1006h\x1b[?2026h\x1b[?1004h\x1b[>1u");
        assert_eq!(t.mouse_proto, MouseProto::Any);
        assert!(t.sync_output);
        feed(&mut t, b"\x1b[!p");
        assert_eq!(t.mouse_proto, MouseProto::Off);
        assert!(!t.mouse_sgr);
        assert!(!t.sync_output);
        assert!(!t.focus_events);
        assert_eq!(t.kbd_flags(), 0);
    }

    #[test]
    fn kitty_push_query_pop() {
        let mut t = Terminal::new(4, 20);
        assert_eq!(t.kbd_flags(), 0);
        feed(&mut t, b"\x1b[>1u");
        assert_eq!(t.kbd_flags(), 1);
        feed(&mut t, b"\x1b[?u");
        assert_eq!(t.responses, b"\x1b[?1u");
        t.responses.clear();
        feed(&mut t, b"\x1b[<u");
        assert_eq!(t.kbd_flags(), 0);
        feed(&mut t, b"\x1b[?u");
        assert_eq!(t.responses, b"\x1b[?0u");
    }

    #[test]
    fn kitty_set_modes_and_mask() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[=3;1u");
        assert_eq!(t.kbd_flags(), 3);
        feed(&mut t, b"\x1b[=2;3u"); // mode 3 clears bit 2
        assert_eq!(t.kbd_flags(), 1);
        feed(&mut t, b"\x1b[=8;2u"); // mode 2 OR-in flag 8 (report all keys)
        assert_eq!(t.kbd_flags(), 9);
        feed(&mut t, b"\x1b[=4;2u"); // alternate-keys (4) is unsupported -> masked off
        assert_eq!(t.kbd_flags(), 9);
    }

    #[test]
    fn kitty_unsupported_bits_masked_on_push() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[>31u"); // 1|2|4|8|16; alternate-keys (4) is the unsupported bit
        assert_eq!(t.kbd_flags(), 27);
    }

    #[test]
    fn plain_csi_u_still_restores_cursor() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[3;4H\x1b[s"); // save cursor at row 2, col 3
        feed(&mut t, b"\x1b[1;1H\x1b[u"); // move home, then restore
        assert_eq!(t.grid.cursor.row, 2);
        assert_eq!(t.grid.cursor.col, 3);
    }

    #[test]
    fn apc_graphics_payload_is_swallowed() {
        // kitty-graphics APC payloads must be consumed, not leaked as base64
        // into the grid (apps send them after probing DA1 / XTVERSION)
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b_Gf=100,a=T,m=0;iVBORw0KGgo=\x1b\\hi");
        assert_eq!(t.grid.lines[0][0].c, 'h');
        assert_eq!(t.grid.lines[0][1].c, 'i');
    }

    #[test]
    fn sgr_underline_styles_and_blink() {
        let mut t = Terminal::new(2, 10);
        feed(&mut t, b"\x1b[4mA");
        assert_eq!(t.grid.lines[0][0].attrs.underline, UnderlineStyle::Single);
        feed(&mut t, b"\x1b[4:3mB");
        assert_eq!(t.grid.lines[0][1].attrs.underline, UnderlineStyle::Curly);
        feed(&mut t, b"\x1b[4:5mC");
        assert_eq!(t.grid.lines[0][2].attrs.underline, UnderlineStyle::Dashed);
        feed(&mut t, b"\x1b[21mD");
        assert_eq!(t.grid.lines[0][3].attrs.underline, UnderlineStyle::Double);
        feed(&mut t, b"\x1b[24mE");
        assert_eq!(t.grid.lines[0][4].attrs.underline, UnderlineStyle::None);
        feed(&mut t, b"\x1b[5mF");
        assert!(t.grid.lines[0][5].attrs.blink);
        feed(&mut t, b"\x1b[25mG");
        assert!(!t.grid.lines[0][6].attrs.blink);
    }

    #[test]
    fn osc8_hyperlink() {
        let mut t = Terminal::new(2, 20);
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\Link\x1b]8;;\x1b\\X");
        let id = t.grid.lines[0][0].link;
        assert_ne!(id, 0);
        assert_eq!(t.grid.lines[0][3].link, id); // 'k' of Link
        assert_eq!(t.grid.link_uri(id), Some("https://example.com"));
        assert_eq!(t.grid.lines[0][4].link, 0); // 'X' after the link ended
    }

    #[test]
    fn osc7_sets_cwd_dirty_only_on_real_cwd_change() {
        let mut t = Terminal::new(2, 40);
        let mut p = Parser::new();
        // a real OSC-7 sets cwd and raises the relabel flag
        p.advance(&mut t, b"\x1b]7;file:///C:/work\x07");
        assert_eq!(t.cwd.as_deref(), Some("file:///C:/work"));
        assert!(t.cwd_dirty);
        t.cwd_dirty = false;
        // still fires when the OSC-7 is split across two reads — the old byte
        // scan missed this since no 3-byte window spanned the chunk boundary
        p.advance(&mut t, b"\x1b]7;file:///C:/a");
        p.advance(&mut t, b"nother\x07");
        assert_eq!(t.cwd.as_deref(), Some("file:///C:/another"));
        assert!(t.cwd_dirty);
        t.cwd_dirty = false;
        // a different OSC that merely starts with '7' (OSC 70) must NOT relabel,
        // though the old `windows(3) == ESC ] 7` scan matched its prefix
        p.advance(&mut t, b"\x1b]70;ignored\x07");
        assert!(!t.cwd_dirty);
    }

    #[test]
    fn osc9_9_sets_cmd_cwd() {
        let mut t = Terminal::new(2, 40);
        feed(&mut t, b"\x1b]9;9;C:\\work\x1b\\");
        assert_eq!(t.cwd.as_deref(), Some("C:\\work"));
        assert!(t.cwd_dirty);
    }

    #[test]
    fn full_text_joins_wraps_and_reads_history() {
        let mut t = Terminal::new(3, 8);
        // four hard lines on a 3-row screen: "one" scrolls into history
        feed(&mut t, b"one\r\ntwo\r\nthree\r\nfour");
        // plus a soft-wrapped logical line longer than the width
        feed(&mut t, b"\r\nabcdefghij");
        let text = t.grid.full_text();
        assert_eq!(text, "one\ntwo\nthree\nfour\nabcdefghij");
    }

    #[test]
    fn mode_12_controls_cursor_blink() {
        let mut t = Terminal::new(2, 10);
        assert_eq!(t.grid.cursor.shape_blink, None);
        feed(&mut t, b"\x1b[?12l");
        assert_eq!(t.grid.cursor.shape_blink, Some(false));
        feed(&mut t, b"\x1b[?12$p");
        assert_eq!(t.responses, b"\x1b[?12;2$y");
        t.responses.clear();
        feed(&mut t, b"\x1b[?12h");
        assert_eq!(t.grid.cursor.shape_blink, Some(true));
        // DECSTR clears the app override back to the configured default
        feed(&mut t, b"\x1b[!p");
        assert_eq!(t.grid.cursor.shape_blink, None);
    }

    #[test]
    fn irm_insert_mode_shifts_the_line() {
        let mut t = Terminal::new(2, 10);
        feed(&mut t, b"abcdef\x1b[1;3H");
        // replace mode overwrites
        feed(&mut t, b"X");
        assert_eq!(t.grid.lines[0][2].c, 'X');
        assert_eq!(t.grid.lines[0][3].c, 'd');
        // insert mode shifts the tail right, dropping what falls off the edge
        feed(&mut t, b"\x1b[4h\x1b[1;3HY");
        assert_eq!(t.grid.lines[0][2].c, 'Y');
        assert_eq!(t.grid.lines[0][3].c, 'X');
        assert_eq!(t.grid.lines[0][4].c, 'd');
        // DECRQM (ANSI form) reports it set
        feed(&mut t, b"\x1b[4$p");
        assert_eq!(t.responses, b"\x1b[4;1$y");
        t.responses.clear();
        // DECSTR restores replace mode
        feed(&mut t, b"\x1b[!p");
        assert!(!t.grid.insert_mode);
        feed(&mut t, b"\x1b[4$p");
        assert_eq!(t.responses, b"\x1b[4;2$y");
    }

    #[test]
    fn mode_1007_tracks_alternate_scroll() {
        let mut t = Terminal::new(4, 10);
        assert!(t.alt_scroll);
        feed(&mut t, b"\x1b[?1007l");
        assert!(!t.alt_scroll);
        // DECRQM reports it reset
        feed(&mut t, b"\x1b[?1007$p");
        assert_eq!(t.responses, b"\x1b[?1007;2$y");
        t.responses.clear();
        feed(&mut t, b"\x1b[?1007h");
        assert!(t.alt_scroll);
        // RIS restores the default-on state
        feed(&mut t, b"\x1b[?1007l\x1bc");
        assert!(t.alt_scroll);
    }

    #[test]
    fn decaln_fills_screen_and_resets_margins() {
        let mut t = Terminal::new(4, 10);
        // shrink the region, turn on origin mode, park the cursor mid-screen
        feed(&mut t, b"\x1b[2;3r\x1b[?6h\x1b[2;5Hxyz\x1b#8");
        assert_eq!(t.grid.cursor.row, 0);
        assert_eq!(t.grid.cursor.col, 0);
        assert!(!t.grid.origin_mode);
        assert_eq!(t.grid.region_top, 0);
        assert_eq!(t.grid.region_bottom, 3);
        for r in 0..4 {
            for c in 0..10 {
                assert_eq!(t.grid.lines[r][c].c, 'E', "row {r} col {c}");
            }
        }
        // ESC 8 alone must still be DECRC, not a stray alignment fill
        feed(&mut t, b"\x1b7\x1b[3;3Hq\x1b8");
        assert_eq!(t.grid.lines[2][2].c, 'q');
    }

    #[test]
    fn tab_stops_default_every_eight() {
        let mut t = Terminal::new(2, 40);
        feed(&mut t, b"\t");
        assert_eq!(t.grid.cursor.col, 8);
        feed(&mut t, b"\x1b[2I"); // CHT 2
        assert_eq!(t.grid.cursor.col, 24);
        feed(&mut t, b"\x1b[Z"); // CBT
        assert_eq!(t.grid.cursor.col, 16);
        feed(&mut t, b"\x1b[9I"); // past the last stop pins at the right edge
        assert_eq!(t.grid.cursor.col, 39);
    }

    #[test]
    fn hts_and_tbc_manage_custom_stops() {
        let mut t = Terminal::new(2, 40);
        // clear all stops, set custom ones at 5 and 11
        feed(&mut t, b"\x1b[3g\x1b[1;6H\x1bH\x1b[1;12H\x1bH\x1b[1;1H");
        feed(&mut t, b"\t");
        assert_eq!(t.grid.cursor.col, 5);
        feed(&mut t, b"\t");
        assert_eq!(t.grid.cursor.col, 11);
        feed(&mut t, b"\t"); // no stop past 11: pin at the last column
        assert_eq!(t.grid.cursor.col, 39);
        // TBC 0 clears only the stop under the cursor
        feed(&mut t, b"\x1b[1;12H\x1b[g\x1b[1;1H\t\t");
        assert_eq!(t.grid.cursor.col, 39);
        feed(&mut t, b"\x1b[1;1H\t");
        assert_eq!(t.grid.cursor.col, 5);
    }

    #[test]
    fn tab_stops_survive_resize() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[3g\x1b[1;10H\x1bH\x1b[1;1H");
        t.grid.resize(4, 40);
        feed(&mut t, b"\t");
        assert_eq!(t.grid.cursor.col, 9); // custom stop kept
        feed(&mut t, b"\t");
        assert_eq!(t.grid.cursor.col, 24); // new columns carry the default cadence
    }

    #[test]
    fn osc52_sets_clipboard() {
        let mut t = Terminal::new(2, 20);
        // base64("hello") == aGVsbG8=
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(t.clipboard.as_deref(), Some("hello"));
        // a read query ("?") must be ignored, not answered
        t.clipboard = None;
        feed(&mut t, b"\x1b]52;c;?\x1b\\");
        assert_eq!(t.clipboard, None);
        // a payload past the 1MB cap is dropped whole, not truncated
        let mut huge = b"\x1b]52;c;".to_vec();
        huge.resize(huge.len() + 1024 * 1024 * 4 / 3 + 4, b'A');
        huge.extend_from_slice(b"\x1b\\");
        feed(&mut t, &huge);
        assert_eq!(t.clipboard, None);
    }

    #[test]
    fn osc9_4_progress_states() {
        let mut t = Terminal::new(2, 20);
        feed(&mut t, b"\x1b]9;4;1;50\x1b\\");
        assert_eq!(t.progress, Some((1, 50)));
        // percent clamps to 100; error and paused carry one too
        feed(&mut t, b"\x1b]9;4;2;250\x1b\\");
        assert_eq!(t.progress, Some((2, 100)));
        feed(&mut t, b"\x1b]9;4;4;30\x1b\\");
        assert_eq!(t.progress, Some((4, 30)));
        // indeterminate has no percent
        feed(&mut t, b"\x1b]9;4;3;99\x1b\\");
        assert_eq!(t.progress, Some((3, 0)));
        // an unknown state leaves the current value; 0 clears
        feed(&mut t, b"\x1b]9;4;7;10\x1b\\");
        assert_eq!(t.progress, Some((3, 0)));
        feed(&mut t, b"\x1b]9;4;0;0\x1b\\");
        assert_eq!(t.progress, None);
        // a non-progress OSC 9 (toast) never touches progress
        feed(&mut t, b"\x1b]9;hello\x1b\\");
        assert_eq!(t.progress, None);
        // RIS clears a live progress
        feed(&mut t, b"\x1b]9;4;1;10\x1b\\\x1bc");
        assert_eq!(t.progress, None);
    }

    #[test]
    fn osc_dynamic_colors_set_query_and_reset() {
        use crate::color::Rgb;
        let mut t = Terminal::new(2, 10);
        // set the background (base16-shell style) and remap ansi red
        feed(&mut t, b"\x1b]11;#1e1e2e\x1b\\");
        feed(&mut t, b"\x1b]4;1;rgb:ff/45/00\x1b\\");
        assert_eq!(t.colors.bg, Some(Rgb::new(0x1e, 0x1e, 0x2e)));
        assert_eq!(t.colors.ansi(1), Some(Rgb::new(0xff, 0x45, 0x00)));
        // 16-bit-per-channel specs keep their most significant bits
        feed(&mut t, b"\x1b]10;rgb:ffff/8080/0000\x1b\\");
        assert_eq!(t.colors.fg, Some(Rgb::new(0xff, 0x80, 0x00)));
        // a query for an overridden color answers directly, no app round-trip
        t.color_queries.clear();
        t.responses.clear();
        feed(&mut t, b"\x1b]11;?\x1b\\");
        assert!(t.color_queries.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&t.responses),
            "\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"
        );
        // an un-overridden query still round-trips through the app
        feed(&mut t, b"\x1b]12;?\x1b\\");
        assert_eq!(t.color_queries, vec![ColorReq::Cursor]);
        // targeted resets clear exactly what they name
        feed(&mut t, b"\x1b]104;1\x1b\\");
        assert_eq!(t.colors.ansi(1), None);
        feed(&mut t, b"\x1b]111\x1b\\");
        assert_eq!(t.colors.bg, None);
        assert!(t.colors.fg.is_some(), "fg untouched by the bg reset");
        // a hard reset drops everything
        feed(&mut t, b"\x1bc");
        assert!(!t.colors.any());
    }

    #[test]
    fn osc133_tracks_command_running_and_done() {
        let mut t = Terminal::new(2, 10);
        assert!(!t.cmd_running);
        feed(&mut t, b"\x1b]133;C\x1b\\");
        assert!(t.cmd_running);
        assert_eq!(t.cmd_done, None);
        // D ends the window and leaves a sticky done event with the exit code,
        // even when the next prompt's A/B arrive in the same chunk
        feed(&mut t, b"\x1b]133;D;1\x1b\\\x1b]133;A\x1b\\\x1b]133;B\x1b\\");
        assert!(!t.cmd_running);
        assert_eq!(t.cmd_done.take(), Some(Some(1)));
    }

    #[test]
    fn osc_title_dirty_only_on_real_change() {
        let mut t = Terminal::new(2, 10);
        feed(&mut t, b"\x1b]2;claude \x01- working\x1b\\");
        assert_eq!(t.title, "claude - working");
        assert!(t.title_dirty);
        t.title_dirty = false;
        // the same title again is not a change
        feed(&mut t, b"\x1b]2;claude - working\x1b\\");
        assert!(!t.title_dirty);
        // OSC 0 (icon+title) goes through the same path
        feed(&mut t, b"\x1b]0;done\x1b\\");
        assert_eq!(t.title, "done");
        assert!(t.title_dirty);
    }

    #[test]
    fn osc_notifications_ring_the_bell() {
        let mut t = Terminal::new(2, 10);
        // an iTerm2-style toast rings the bell (routed to tab dot + taskbar)
        // and keeps its text for the status-bar readout
        feed(&mut t, b"\x1b]9;build done\x1b\\");
        assert!(t.bell);
        assert_eq!(t.notify.take().as_deref(), Some("build done"));
        t.bell = false;
        // rxvt/tmux convention: title and body join into one line
        feed(&mut t, b"\x1b]777;notify;title;body\x1b\\");
        assert!(t.bell);
        assert_eq!(t.notify.take().as_deref(), Some("title: body"));
        t.bell = false;
        // a message with embedded control bytes is sanitized
        feed(&mut t, b"\x1b]9;a\x01b\x1b\\");
        assert_eq!(t.notify.take().as_deref(), Some("ab"));
        t.bell = false;
        // progress and other numeric ConEmu subcommands stay silent
        feed(&mut t, b"\x1b]9;4;1;50\x1b\\\x1b]9;9;C:/x\x1b\\\x1b]777;other\x1b\\");
        assert!(!t.bell);
        assert!(t.notify.is_none());
    }

    #[test]
    fn osc_color_query_and_reply() {
        let mut t = Terminal::new(2, 10);
        feed(&mut t, b"\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;1;?\x1b\\");
        assert_eq!(t.color_queries, vec![ColorReq::Bg, ColorReq::Fg, ColorReq::Ansi(1)]);
        // a SET (no '?') is ignored, not recorded as a query
        feed(&mut t, b"\x1b]11;rgb:00/00/00\x1b\\");
        assert_eq!(t.color_queries.len(), 3);
        let pal = crate::color::Palette::from_theme(crate::color::ThemeId::Instrument);
        assert_eq!(
            format_color_reply(ColorReq::Bg, &pal),
            b"\x1b]11;rgb:1414/1414/1414\x1b\\"
        );
    }

    #[test]
    fn pathological_input_does_not_panic() {
        let mut t = Terminal::new(24, 80);
        let inputs: &[&[u8]] = &[
            b"\x1b[999999999;999999999H",
            b"\x1b[999999999X\x1b[999999999@\x1b[999999999P",
            b"\x1b[999999999S\x1b[999999999T\x1b[999999999L\x1b[999999999M",
            b"\x1b[999999999;999999999r",
            b"\x1b]0;\x1b\\\x1b]8;;\x1b\\\x1b]52;c;@@@@\x1b\\\x1b]4;99999;?\x1b\\",
            b"\x1b[?99999h\x1b[?99999l\x1b[38;2;999;999;999m",
            b"\xff\xfe\x00\x01\x02 junk \x1b[ \x1b] partial",
            b"\x1b[>999999u\x1b[=999999;999999u\x1b[<999999u",
        ];
        for chunk in inputs {
            feed(&mut t, chunk);
        }
        // a flood of wide chars, combining marks, tabs, and newlines
        let flood: Vec<u8> = (0..3000).flat_map(|_| "\u{1f680}a\u{0301}\r\n\t".bytes()).collect();
        feed(&mut t, &flood);
        // reaching here without panicking is the assertion
        assert_eq!(t.grid.cols, 80);
    }

    #[test]
    fn random_streams_keep_grid_consistent() {
        // deterministic xorshift so the corpus is reproducible without a dep
        let mut state: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // weighted toward control/escape bytes so the CSI/OSC/SGR machinery is
        // exercised, not just printable text. P/q/#/!/~/- open and drive DCS
        // (sixel, XTGETTCAP, DECRQSS), _ and G reach the APC kitty-graphics
        // path, and \ closes any of them with ST mid-stream
        let alphabet: &[u8] =
            b"\x1b[]0123456789;:?>=<mHJKABCDsuhlrPq#!~-_G$+\\ \x07\r\n\t\x08\xff\xc2\x80\xf0\x9f";
        for _ in 0..500 {
            let rows = 1 + (next() % 40) as usize;
            let cols = 1 + (next() % 120) as usize;
            let mut t = Terminal::new(rows, cols);
            t.set_cell_px(8, 16);
            let len = (next() % 800) as usize;
            let buf: Vec<u8> = (0..len).map(|_| alphabet[(next() as usize) % alphabet.len()]).collect();
            feed(&mut t, &buf);
            // a random reflow, then assert consistency right away (a later in-band
            // mode change could legitimately resize the grid again)
            let (r2, c2) = (1 + (next() % 40) as usize, 1 + (next() % 120) as usize);
            t.resize(r2, c2);
            assert_eq!(t.grid.rows, r2);
            assert_eq!(t.grid.cols, c2);
            assert_eq!(t.grid.lines.len(), r2, "live line count must equal rows");
            for line in &t.grid.lines {
                assert_eq!(line.len(), c2, "every live line must be cols wide");
            }
            assert!(t.grid.cursor.row < r2, "cursor row in bounds");
            assert!(t.grid.cursor.col <= c2, "cursor col in bounds");
            // keep feeding to exercise post-resize transitions (no dim assert)
            feed(&mut t, &buf);
        }
    }

    #[test]
    fn sixel_dcs_decodes_places_and_scrolls() {
        let mut t = Terminal::new(6, 20);
        t.set_cell_px(8, 6);
        // a 3x6 px red image = one cell row; the cursor lands on the next line
        feed(&mut t, b"\x1bP0;0;0q#1;2;100;0;0!3~\x1b\\");
        let p = t.grid.placements();
        assert_eq!(p.len(), 1);
        let img = t.images.get(p[0].image_id).expect("image stored");
        assert_eq!((img.width, img.height), (3, 6));
        assert_eq!(img.rgba[..4], [255, 0, 0, 255]);
        assert_eq!((p[0].abs_line, p[0].col), (0, 0));
        assert_eq!((t.grid.cursor.row, t.grid.cursor.col), (1, 0));
    }

    #[test]
    fn sixel_taller_than_screen_scrolls_with_text() {
        let mut t = Terminal::new(3, 10);
        t.set_cell_px(8, 6);
        // 18 px tall = 3 cell rows on a 3-row screen: the advance must scroll
        feed(&mut t, b"\x1bPq~-~-~\x1b\\");
        assert_eq!(t.grid.cursor.row, 2, "cursor pinned at the bottom row");
        // the placement stays glued to its (now scrolled) text line
        let p = t.grid.placements();
        assert_eq!(p[0].abs_line, 0);
        assert!(t.grid.screen_row_signed(p[0].abs_line) < 0, "anchor scrolled above the viewport top");
    }

    #[test]
    fn decsdm_pins_the_image_and_reports() {
        let mut t = Terminal::new(6, 20);
        t.set_cell_px(8, 6);
        feed(&mut t, b"\x1b[?80h\x1b[3;5H\x1bPq~\x1b\\");
        let p = t.grid.placements();
        assert_eq!((p[0].abs_line, p[0].col), (0, 0), "pinned to the top-left");
        assert_eq!((t.grid.cursor.row, t.grid.cursor.col), (2, 4), "cursor untouched");
        feed(&mut t, b"\x1b[?80$p");
        assert_eq!(t.responses, b"\x1b[?80;1$y");
        t.responses.clear();
        feed(&mut t, b"\x1b[?80l\x1b[?80$p");
        assert_eq!(t.responses, b"\x1b[?80;2$y");
    }

    #[test]
    fn da1_advertises_sixel() {
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"\x1b[c");
        assert_eq!(t.responses, b"\x1b[?62;4;22c");
    }

    #[test]
    fn xtsmgraphics_reports_registers_and_geometry() {
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"\x1b[?1;1;0S");
        assert_eq!(t.responses, b"\x1b[?1;0;256S");
        t.responses.clear();
        feed(&mut t, b"\x1b[?2;1;0S");
        assert_eq!(t.responses, b"\x1b[?2;0;4096;4096S");
        t.responses.clear();
        // ReGIS (3) is not supported: status 1, error in item
        feed(&mut t, b"\x1b[?3;1;0S");
        assert_eq!(t.responses, b"\x1b[?3;1S");
        t.responses.clear();
        // plain CSI S stays scroll-up, never a graphics reply
        feed(&mut t, b"\x1b[1S");
        assert!(t.responses.is_empty());
    }

    #[test]
    fn non_sixel_dcs_is_ignored() {
        // XTGETTCAP (DCS + q) and DECRQSS (DCS $ q) must not start a sixel
        let mut t = Terminal::new(4, 10);
        feed(&mut t, b"\x1bP+q544e\x1b\\\x1bP$qm\x1b\\");
        assert!(t.grid.placements().is_empty());
    }

    #[test]
    fn xtgettcap_answers_the_caps_nvim_probes() {
        let mut t = Terminal::new(4, 20);
        // "RGB" (hex 524742) is a boolean: the reply echoes the bare name
        feed(&mut t, b"\x1bP+q524742\x1b\\");
        assert_eq!(t.responses, b"\x1bP1+r524742\x1b\\");
        t.responses.clear();
        // "smulx" resolves to the styled-underline template; unknown "foo"
        // is dropped from the same multi-cap request
        feed(&mut t, b"\x1bP+q736d756c78;666f6f\x1b\\");
        assert_eq!(t.responses, b"\x1bP1+r736d756c78=1b5b343a25703125646d\x1b\\");
        t.responses.clear();
        // nothing known -> the standard failure reply, never silence
        feed(&mut t, b"\x1bP+q666f6f\x1b\\");
        assert_eq!(t.responses, b"\x1bP0+r\x1b\\");
        t.responses.clear();
        // odd-length hex is malformed, not a panic
        feed(&mut t, b"\x1bP+q524\x1b\\");
        assert_eq!(t.responses, b"\x1bP0+r\x1b\\");
    }

    #[test]
    fn decrqss_reports_the_sgr_pen() {
        let mut t = Terminal::new(4, 20);
        // a default pen reports bare reset
        feed(&mut t, b"\x1bP$qm\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r0m\x1b\\");
        t.responses.clear();
        // bold + italic + curly underline + truecolor fg + indexed bg round-trip
        feed(&mut t, b"\x1b[1;3;4:3;38;2;1;2;3;48;5;110m\x1bP$qm\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r0;1;3;4:3;38:2:1:2:3;48:5:110m\x1b\\");
        t.responses.clear();
        // the basic 16 report as their compact codes
        feed(&mut t, b"\x1b[0;31;103m\x1bP$qm\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r0;31;103m\x1b\\");
    }

    #[test]
    fn decrqss_reports_region_cursor_style_and_invalid() {
        let mut t = Terminal::new(10, 20);
        feed(&mut t, b"\x1b[3;7r\x1bP$qr\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r3;7r\x1b\\");
        t.responses.clear();
        // cursor style: default 1, then a steady bar (6)
        feed(&mut t, b"\x1bP$q q\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r1 q\x1b\\");
        t.responses.clear();
        feed(&mut t, b"\x1b[6 q\x1bP$q q\x1b\\");
        assert_eq!(t.responses, b"\x1bP1$r6 q\x1b\\");
        t.responses.clear();
        // an unknown setting gets the invalid reply, not silence
        feed(&mut t, b"\x1bP$qz\x1b\\");
        assert_eq!(t.responses, b"\x1bP0$r\x1b\\");
    }

    #[test]
    fn renders_a_box_drawing_frame() {
        let mut t = Terminal::new(4, 8);
        feed(
            &mut t,
            "\x1b[1;1H\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\
             \x1b[2;1H\u{2502}      \u{2502}\
             \x1b[3;1H\u{2502}      \u{2502}\
             \x1b[4;1H\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}"
                .as_bytes(),
        );
        let top: String = t.grid.lines[0].iter().map(|c| c.c).collect();
        assert_eq!(top, "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}");
        assert_eq!(t.grid.lines[1][0].c, '\u{2502}');
        assert_eq!(t.grid.lines[1][7].c, '\u{2502}');
        assert_eq!(t.grid.lines[3][7].c, '\u{2518}');
    }
}
