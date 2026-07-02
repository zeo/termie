use vte::{Params, Perform};

use crate::color::{Color, Palette};
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
    format!(
        "\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
        code, c.r, c.r, c.g, c.g, c.b, c.b
    )
    .into_bytes()
}

/// kitty keyboard protocol flags termie honors: disambiguate (1) + report
/// event types (2). bits an app requests outside this mask are dropped, so a
/// CSI ? u query always reports exactly what we apply
const KBD_SUPPORTED: u8 = 0b11;
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

    pub title: String,
    pub cwd: Option<String>,
    /// set when OSC-7 updates cwd; the app consumes it to relabel tabs instead
    /// of rescanning every output chunk for the escape (which also missed an
    /// OSC-7 split across two reads and fired on the byte pattern in file data)
    pub cwd_dirty: bool,
    pub last_osc133: Option<Osc133>,
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
            title: String::new(),
            cwd: None,
            cwd_dirty: false,
            last_osc133: None,
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
        }
    }

    /// let the renderer feed the content cell size so XTWINOPS 14/16 can
    /// report pixel geometry — image tools size kitty graphics from those,
    /// since nothing can ioctl a pixel size through ConPTY
    pub fn set_cell_px(&mut self, w: u16, h: u16) {
        self.cell_px = (w, h);
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
        self.apply_sgr(&[&[0u16][..]]);
        self.grid.set_scroll_region(0, self.grid.rows - 1);
        self.grid.origin_mode = false;
        self.grid.cursor.shape_set = false;
        // DECSTR turns the text cursor back on (DECTCEM)
        self.grid.cursor.visible = true;
    }

    /// DECRQM mode-state reply value: 1 = set, 2 = reset, 0 = not recognized
    fn dec_mode_state(&self, mode: u16) -> u16 {
        let on = match mode {
            1 => self.app_cursor_keys,
            6 => self.grid.origin_mode,
            25 => self.grid.cursor.visible,
            1000 => self.mouse_proto == MouseProto::Normal,
            1002 => self.mouse_proto == MouseProto::Button,
            1003 => self.mouse_proto == MouseProto::Any,
            1004 => self.focus_events,
            1006 => self.mouse_sgr,
            2004 => self.bracketed_paste,
            2026 => self.sync_output,
            47 | 1047 | 1049 => self.using_alt,
            _ => return 0,
        };
        if on { 1 } else { 2 }
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
        if self.mouse_sgr {
            let cb = btn as u32 + md + if motion { 32 } else { 0 };
            let m = if pressed { 'M' } else { 'm' };
            Some(format!("\x1b[<{cb};{c};{r}{m}").into_bytes())
        } else {
            // legacy X10: release is button 3; values offset by 32, clamped
            let cb = if pressed { btn as u32 + md + if motion { 32 } else { 0 } } else { 3 + md };
            let enc = |v: u32| -> u8 { (v + 32).min(255) as u8 };
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
        // (stray mouse reports, kitty-encoded keys, application arrow keys).
        // bracketed paste and focus reporting are left alone since shells use them
        self.mouse_proto = MouseProto::Off;
        self.mouse_sgr = false;
        self.app_cursor_keys = false;
        self.kbd_stack = vec![0];
    }

    fn set_mode(&mut self, private: bool, mode: u16, enable: bool) {
        if private {
            match mode {
                1 => self.app_cursor_keys = enable,
                6 => self.grid.set_origin_mode(enable),
                25 => self.grid.cursor.visible = enable,
                2026 => self.sync_output = enable,
                1000 => self.mouse_proto = if enable { MouseProto::Normal } else { MouseProto::Off },
                1002 => self.mouse_proto = if enable { MouseProto::Button } else { MouseProto::Off },
                1003 => self.mouse_proto = if enable { MouseProto::Any } else { MouseProto::Off },
                1006 => self.mouse_sgr = enable,
                1004 => self.focus_events = enable,
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
        }
        // non-private (ANSI) modes: none needed for the tracer bullet
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
                        _ => {}
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
                90..=97 => cur.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => cur.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    // semicolon form: read following groups
                    let kind = groups.get(i + 1).and_then(|g| g.first().copied());
                    match kind {
                        Some(5) => {
                            if let Some(n) = groups.get(i + 2).and_then(|g| g.first().copied()) {
                                let color = Color::Indexed(n as u8);
                                if code == 38 {
                                    cur.fg = color;
                                } else {
                                    cur.bg = color;
                                }
                                i += 3;
                                continue;
                            }
                        }
                        Some(2) => {
                            let r = groups.get(i + 2).and_then(|g| g.first().copied());
                            let gr = groups.get(i + 3).and_then(|g| g.first().copied());
                            let b = groups.get(i + 4).and_then(|g| g.first().copied());
                            if let (Some(r), Some(gn), Some(b)) = (r, gr, b) {
                                let color = Color::Rgb(r as u8, gn as u8, b as u8);
                                if code == 38 {
                                    cur.fg = color;
                                } else {
                                    cur.bg = color;
                                }
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
            'b' => {
                // REP: repeat the last presented glyph N times (already charset-mapped)
                if let Some(mapped) = self.last_print {
                    let n = (param_at(params, 0, 1) as usize).min(self.grid.cols * self.grid.rows);
                    for _ in 0..n {
                        self.grid.put_char(mapped);
                    }
                }
            }
            'S' => self.grid.scroll_up(param_at(params, 0, 1) as usize),
            'T' => self.grid.scroll_down(param_at(params, 0, 1) as usize),
            'm' => {
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
            'h' => self.set_mode(private, param_at(params, 0, 0), true),
            'l' => self.set_mode(private, param_at(params, 0, 0), false),
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
                    self.responses.extend_from_slice(b"\x1b[?6c");
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
        match byte {
            b'M' => self.grid.reverse_index(),
            b'D' => self.grid.linefeed(),
            b'E' => {
                self.grid.linefeed();
                self.grid.carriage_return();
            }
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'c' => {
                // RIS full reset
                let (rows, cols) = (self.grid.rows, self.grid.cols);
                self.grid = Grid::new(rows, cols);
                self.saved_primary = None;
                self.using_alt = false;
                self.app_cursor_keys = false;
                self.bracketed_paste = false;
                self.kbd_stack = vec![0];
                self.g0 = Charset::Ascii;
                self.g1 = Charset::Ascii;
                self.gl = 0;
                self.last_print = None;
                self.progress = None;
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
                    self.title = String::from_utf8_lossy(t).into_owned();
                }
            }
            b"7" => {
                if let Some(u) = params.get(1) {
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
                // read query) so a remote program can't exfiltrate the clipboard
                if let Some(&data) = params.get(2)
                    && data != b"?"
                    && let Some(bytes) = base64_decode(data)
                {
                    self.clipboard = Some(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            b"10" | b"11" | b"12" => {
                // OSC 10/11/12 ; ?  — query the default fg / bg / cursor color
                if let Some(p) = params.get(1)
                    && p.len() == 1
                    && p[0] == b'?'
                {
                    self.color_queries.push(match kind {
                        b"10" => ColorReq::Fg,
                        b"11" => ColorReq::Bg,
                        _ => ColorReq::Cursor,
                    });
                }
            }
            b"4" => {
                // OSC 4 ; n ; ?  — query palette color n
                if let Some(q) = params.get(2)
                    && q.len() == 1
                    && q[0] == b'?'
                    && let Some(n) = params
                        .get(1)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse::<u8>().ok())
                {
                    self.color_queries.push(ColorReq::Ansi(n));
                }
            }
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
                        Some(b'C') => Some(Osc133::CommandStart),
                        Some(b'D') => {
                            let code = params
                                .get(2)
                                .and_then(|c| std::str::from_utf8(c).ok())
                                .and_then(|s| s.parse::<i32>().ok());
                            Some(Osc133::CommandDone(code))
                        }
                        _ => self.last_osc133,
                    };
                }
            }
            _ => {}
        }
    }

    // DCS sequences are unused for the tracer bullet
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
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
        // CSI 5 SP q -> bar
        feed(&mut t, b"\x1b[5 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Bar);
        // CSI 0 SP q -> default, which is a block (this was dead code before)
        feed(&mut t, b"\x1b[0 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Block);
        // CSI 4 SP q -> underline
        feed(&mut t, b"\x1b[4 q");
        assert_eq!(t.grid.cursor.shape, CursorShape::Underline);
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
        feed(&mut t, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1h\x1b[>1u");
        assert_eq!(t.mouse_proto, MouseProto::Normal);
        assert!(t.mouse_sgr);
        assert!(t.app_cursor_keys);
        assert_eq!(t.kbd_flags(), 1);
        // leaving the alt screen must drop them so a stray click can't print a
        // mouse report and keys aren't kitty-encoded once the shell is back
        feed(&mut t, b"\x1b[?1049l");
        assert_eq!(t.mouse_proto, MouseProto::Off);
        assert!(!t.mouse_sgr);
        assert!(!t.app_cursor_keys);
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
        feed(&mut t, b"\x1b[=8;2u"); // mode 2 OR-in flag 8, which is unsupported -> masked off
        assert_eq!(t.kbd_flags(), 1);
    }

    #[test]
    fn kitty_unsupported_bits_masked_on_push() {
        let mut t = Terminal::new(4, 20);
        feed(&mut t, b"\x1b[>13u"); // 1|4|8; only disambiguate(1) is supported
        assert_eq!(t.kbd_flags(), 1);
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
        // advertising ghostty invites kitty-graphics (APC G) payloads; the
        // parser must consume them, not leak base64 into the grid
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
    fn osc52_sets_clipboard() {
        let mut t = Terminal::new(2, 20);
        // base64("hello") == aGVsbG8=
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(t.clipboard.as_deref(), Some("hello"));
        // a read query ("?") must be ignored, not answered
        t.clipboard = None;
        feed(&mut t, b"\x1b]52;c;?\x1b\\");
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
        // exercised, not just printable text
        let alphabet: &[u8] = b"\x1b[]0123456789;:?>=<mHJKABCDsuhlr \x07\r\n\t\x08\xff\xc2\x80\xf0\x9f";
        for _ in 0..500 {
            let rows = 1 + (next() % 40) as usize;
            let cols = 1 + (next() % 120) as usize;
            let mut t = Terminal::new(rows, cols);
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
