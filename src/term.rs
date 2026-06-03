use vte::{Params, Perform};

use crate::color::Color;
use crate::grid::{CursorShape, Grid};

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
    pub last_osc133: Option<Osc133>,
    pub bell: bool,

    /// bytes the terminal wants to send back to the pty (DSR/DA replies)
    pub responses: Vec<u8>,
    pub dirty: bool,
    /// DEC 2026 synchronized output: while true an app is mid-frame, so the
    /// renderer holds off painting until the frame ends (no torn/flickering UI)
    pub sync_output: bool,
    /// kitty keyboard protocol flag stack; the last entry is active. starts as
    /// [0] (legacy encoding) and apps push/set richer reporting onto it
    kbd_stack: Vec<u8>,
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
            last_osc133: None,
            bell: false,
            responses: Vec::new(),
            dirty: true,
            sync_output: false,
            kbd_stack: vec![0],
        }
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
    pub fn encode_mouse(&self, btn: u8, pressed: bool, motion: bool, col: usize, row: usize) -> Option<Vec<u8>> {
        if self.mouse_proto == MouseProto::Off {
            return None;
        }
        let (c, r) = (col + 1, row + 1);
        if self.mouse_sgr {
            let cb = btn as u32 + if motion { 32 } else { 0 };
            let m = if pressed { 'M' } else { 'm' };
            Some(format!("\x1b[<{cb};{c};{r}{m}").into_bytes())
        } else {
            // legacy X10: release is button 3; values offset by 32, clamped
            let cb = if pressed { btn as u32 + if motion { 32 } else { 0 } } else { 3 };
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
    }

    fn set_mode(&mut self, private: bool, mode: u16, enable: bool) {
        if private {
            match mode {
                1 => self.app_cursor_keys = enable,
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

    fn apply_sgr(&mut self, groups: &[Vec<u16>]) {
        let cur = &mut self.grid.cursor;
        if groups.is_empty() {
            cur.fg = Color::Default;
            cur.bg = Color::DefaultBg;
            cur.attrs = Default::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let g = &groups[i];
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
            match code {
                0 => {
                    cur.fg = Color::Default;
                    cur.bg = Color::DefaultBg;
                    cur.attrs = Default::default();
                }
                1 => cur.attrs.bold = true,
                2 => cur.attrs.dim = true,
                3 => cur.attrs.italic = true,
                4 => cur.attrs.underline = true,
                7 => cur.attrs.inverse = true,
                8 => cur.attrs.hidden = true,
                9 => cur.attrs.strike = true,
                22 => {
                    cur.attrs.bold = false;
                    cur.attrs.dim = false;
                }
                23 => cur.attrs.italic = false,
                24 => cur.attrs.underline = false,
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

fn param_at(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
        self.dirty = true;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => self.grid.backspace(),
            0x09 => self.grid.tab(),
            0x0a..=0x0c => self.grid.linefeed(),
            0x0d => self.grid.carriage_return(),
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
                self.grid.goto(row, col);
            }
            'H' | 'f' => {
                let row = param_at(params, 0, 1) as usize - 1;
                let col = param_at(params, 1, 1) as usize - 1;
                self.grid.goto(row, col);
            }
            'J' => self.grid.erase_in_display(param_at(params, 0, 0)),
            'K' => self.grid.erase_in_line(param_at(params, 0, 0)),
            'L' => self.grid.insert_lines(param_at(params, 0, 1) as usize),
            'M' => self.grid.delete_lines(param_at(params, 0, 1) as usize),
            '@' => self.grid.insert_chars(param_at(params, 0, 1) as usize),
            'P' => self.grid.delete_chars(param_at(params, 0, 1) as usize),
            'X' => {
                let n = param_at(params, 0, 1) as usize;
                let row = self.grid.cursor.row;
                let col = self.grid.cursor.col;
                let end = (col + n).min(self.grid.cols);
                for c in col..end {
                    self.grid.lines[row][c] = Default::default();
                }
            }
            'S' => self.grid.scroll_up(param_at(params, 0, 1) as usize),
            'T' => self.grid.scroll_down(param_at(params, 0, 1) as usize),
            'm' => {
                let groups: Vec<Vec<u16>> = params.iter().map(|p| p.to_vec()).collect();
                self.apply_sgr(&groups);
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
                if !private {
                    self.responses.extend_from_slice(b"\x1b[?6c");
                }
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
            _ => {}
        }
        self.dirty = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
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
}
