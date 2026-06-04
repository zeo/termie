use std::collections::VecDeque;

use crate::color::Color;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Attrs {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub inverse: bool,
    pub strike: bool,
    pub hidden: bool,
    pub blink: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// OSC 8 hyperlink id into Grid::links; 0 = no link
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::DefaultBg,
            attrs: Attrs::default(),
            link: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub visible: bool,
    pub shape: CursorShape,
    /// true once an app set the shape via DECSCUSR; until then the renderer
    /// uses the user's configured default shape
    pub shape_set: bool,
    /// deferred wrap: cursor sits past the last column until the next print
    pub wrap_pending: bool,
    /// active OSC 8 hyperlink id applied to printed cells; 0 = none
    pub link: u16,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            row: 0,
            col: 0,
            fg: Color::Default,
            bg: Color::DefaultBg,
            attrs: Attrs::default(),
            visible: true,
            shape: CursorShape::Block,
            shape_set: false,
            wrap_pending: false,
            link: 0,
        }
    }
}

/// one row of cells plus whether it soft-wrapped into the next row. derefs to
/// the cell Vec so existing indexing/iteration is unchanged; the wrapped flag
/// lets resize rejoin logical lines and rewrap them to a new width
#[derive(Clone, PartialEq, Debug)]
pub struct Line {
    cells: Vec<Cell>,
    pub wrapped: bool,
}

impl std::ops::Deref for Line {
    type Target = Vec<Cell>;
    fn deref(&self) -> &Vec<Cell> {
        &self.cells
    }
}

impl std::ops::DerefMut for Line {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        &mut self.cells
    }
}

pub struct Grid {
    pub rows: usize,
    pub cols: usize,
    pub lines: Vec<Line>,
    pub scrollback: VecDeque<Line>,
    pub scrollback_limit: usize,
    pub cursor: Cursor,
    saved_cursor: Cursor,
    /// scroll region, 0-based inclusive [top, bottom]
    pub region_top: usize,
    pub region_bottom: usize,
    /// scrollback view offset (lines scrolled up from the live bottom)
    pub view_offset: usize,
    /// total lines ever pushed into scrollback (monotonic); lets prompt marks
    /// use stable absolute indices that survive eviction
    total_scrolled: u64,
    /// absolute line indices of OSC 133 prompt starts, ascending; pruned as
    /// history is evicted
    prompts: Vec<u64>,
    /// OSC 8 hyperlink targets; a cell's link id indexes here, 0 = none and
    /// links[0] is the empty sentinel
    pub links: Vec<String>,
}

fn blank_line(cols: usize) -> Line {
    Line {
        cells: vec![Cell::default(); cols],
        wrapped: false,
    }
}

/// terminal cell width of a char: 0 (combining/zero-width), 1 (normal), or 2
/// (East-Asian wide / fullwidth / emoji). a compact built-in table (no deps)
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    // zero-width: combining marks, joiners, variation selectors
    if matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0EB1 | 0x0EB4..=0x0EBC
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200B..=0x200F | 0x202A..=0x202E
        | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F)
    {
        return 0;
    }
    // wide: East Asian Wide/Fullwidth + common emoji blocks
    if matches!(cp,
        0x1100..=0x115F | 0x2329 | 0x232A | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1FAFF
        | 0x20000..=0x3FFFD)
    {
        return 2;
    }
    1
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Grid {
            rows,
            cols,
            lines: (0..rows).map(|_| blank_line(cols)).collect(),
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
            links: vec![String::new()],
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            region_top: 0,
            region_bottom: rows - 1,
            view_offset: 0,
            total_scrolled: 0,
            prompts: Vec::new(),
        }
    }

    /// the line currently shown at viewport row `r` (scrollback-aware), borrowed
    /// — avoids the per-frame Vec allocation of viewport()
    pub fn line_at(&self, r: usize) -> &Line {
        if self.view_offset == 0 {
            return &self.lines[r.min(self.lines.len() - 1)];
        }
        let total = self.scrollback.len() + self.lines.len();
        let start = total.saturating_sub(self.rows + self.view_offset);
        let idx = start + r;
        if idx < self.scrollback.len() {
            &self.scrollback[idx]
        } else {
            let i = (idx - self.scrollback.len()).min(self.lines.len() - 1);
            &self.lines[i]
        }
    }

    /// the http(s) url under viewport cell (row, col), as (col_start, col_end,
    /// url) with col_end exclusive; None if the cell isn't inside a web link.
    /// rows are viewport-relative so this reads through line_at
    pub fn url_at(&self, row: usize, col: usize) -> Option<(usize, usize, String)> {
        if row >= self.rows {
            return None;
        }
        let line = self.line_at(row);
        let n = line.len();
        if col >= n {
            return None;
        }
        // permissive url token: graphic ascii minus the chars that can't appear
        // unescaped in a url and minus whitespace
        let is_url = |c: char| {
            c.is_ascii()
                && !c.is_ascii_whitespace()
                && !c.is_ascii_control()
                && !"<>\"{}|\\^`".contains(c)
        };
        if !is_url(line[col].c) {
            return None;
        }
        let mut lo = col;
        while lo > 0 && is_url(line[lo - 1].c) {
            lo -= 1;
        }
        let mut hi = col + 1;
        while hi < n && is_url(line[hi].c) {
            hi += 1;
        }
        let run: String = line[lo..hi].iter().map(|c| c.c).collect();
        // the url begins at the scheme inside the run (so "see http://x" works)
        let start = run.find("https://").or_else(|| run.find("http://"))?;
        let mut url: String = run[start..].to_string();
        // drop trailing sentence punctuation that is unlikely to be part of it
        while url.ends_with([')', ']', '}', '.', ',', ';', ':', '!', '?', '\'', '"']) {
            url.pop();
        }
        if url.len() < 8 {
            return None;
        }
        let col_start = lo + start;
        let col_end = col_start + url.chars().count();
        if col < col_start || col >= col_end {
            return None;
        }
        Some((col_start, col_end, url))
    }

    /// inclusive (col_start, col_end) of the word under viewport cell (row, col)
    /// for double-click select; expands over identifier/path-like runs, or over a
    /// whitespace run if the cell is blank. rows are viewport-relative
    pub fn word_bounds(&self, row: usize, col: usize) -> (usize, usize) {
        if row >= self.rows {
            return (col, col);
        }
        let line = self.line_at(row);
        let n = line.len();
        if n == 0 {
            return (0, 0);
        }
        let col = col.min(n - 1);
        // word-class: alnum plus the punctuation that usually reads as part of a
        // path, url, flag, or identifier in terminal output
        let class = |c: char| -> u8 {
            if c == ' ' || c == '\0' {
                0
            } else if c.is_alphanumeric() || "_./-:~@+".contains(c) {
                1
            } else {
                2
            }
        };
        let here = class(line[col].c);
        let mut lo = col;
        while lo > 0 && class(line[lo - 1].c) == here {
            lo -= 1;
        }
        let mut hi = col;
        while hi + 1 < n && class(line[hi + 1].c) == here {
            hi += 1;
        }
        (lo, hi)
    }

    /// inclusive last column of content on viewport `row` (trailing blanks
    /// trimmed) for triple-click line select; 0 if the row is empty
    pub fn line_last_col(&self, row: usize) -> usize {
        if row >= self.rows {
            return 0;
        }
        let line = self.line_at(row);
        let mut hi = 0;
        for (i, cell) in line.iter().enumerate() {
            if cell.c != ' ' && cell.c != '\0' {
                hi = i;
            }
        }
        hi
    }

    /// total logical lines: scrollback history plus the live screen
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.lines.len()
    }

    /// case-insensitive substring search across scrollback and the live screen;
    /// returns (global_line_index, col) for each match start, in top-to-bottom
    /// order. global indices span scrollback (0..len) then live lines
    pub fn search(&self, needle: &str) -> Vec<(usize, usize)> {
        let needle: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        let scan = |cells: &Line, gi: usize, out: &mut Vec<(usize, usize)>| {
            if cells.len() < needle.len() {
                return;
            }
            let lc: Vec<char> = cells.iter().map(|c| c.c.to_ascii_lowercase()).collect();
            for start in 0..=(lc.len() - needle.len()) {
                if lc[start..start + needle.len()] == needle[..] {
                    out.push((gi, start));
                }
            }
        };
        for (i, line) in self.scrollback.iter().enumerate() {
            scan(line, i, &mut out);
        }
        let base = self.scrollback.len();
        for (i, line) in self.lines.iter().enumerate() {
            scan(line, base + i, &mut out);
        }
        out
    }

    /// viewport row currently displaying global line `g`, or None if off-screen
    pub fn global_to_viewport(&self, g: usize) -> Option<usize> {
        let total = self.total_lines();
        let start = total.saturating_sub(self.rows + self.view_offset);
        if g >= start && g < start + self.rows {
            Some(g - start)
        } else {
            None
        }
    }

    /// scroll so global line `g` sits roughly centered in the viewport
    pub fn scroll_to_global(&mut self, g: usize) {
        let total = self.total_lines();
        let target_start = g.saturating_sub(self.rows / 2);
        let vo = total.saturating_sub(self.rows).saturating_sub(target_start);
        self.view_offset = vo.min(self.scrollback.len());
    }

    pub fn scroll_view(&mut self, delta: isize) {
        let max = self.scrollback.len();
        let cur = self.view_offset as isize;
        self.view_offset = (cur + delta).clamp(0, max as isize) as usize;
    }

    /// linear text within [start, end] (row, col) over the visible viewport,
    /// trailing blanks trimmed per row. rows are viewport-relative, so this
    /// reads through line_at to stay correct when scrolled into history
    pub fn selected_text(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let (mut a, mut b) = (start, end);
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        let mut out = String::new();
        for row in a.0..=b.0.min(self.rows.saturating_sub(1)) {
            let line = self.line_at(row);
            // clamp both ends to the line length: a resize can shrink lines
            // between mouse-down and copy, leaving start col past the new width
            let len = line.len();
            let from = (if row == a.0 { a.1 } else { 0 }).min(len);
            let to = (if row == b.0 { (b.1 + 1).min(self.cols) } else { self.cols }).min(len);
            let mut s: String = line[from..to.max(from)]
                .iter()
                .map(|c| c.c)
                .filter(|c| *c != '\0')
                .collect();
            while s.ends_with(' ') {
                s.pop();
            }
            out.push_str(&s);
            if row != b.0 {
                out.push('\n');
            }
        }
        out
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }

        // on a width change, rewrap soft-wrapped logical lines (scrollback + the
        // live screen) to the new width before the row-count adjustment below
        if cols != self.cols {
            self.reflow(cols);
        }

        // adjust the live lines to the new width (put_char indexes by col);
        // leave scrollback lines at their captured width so shrinking doesn't
        // destroy history — draw_grid reads cells via get() and tolerates any
        // length (clipped if longer, blank-padded if shorter)
        for line in &mut self.lines {
            line.resize(cols, Cell::default());
        }

        if rows < self.rows {
            // drop blank rows below the cursor first (keep the prompt + content),
            // only evicting the top into scrollback once the bottom is exhausted
            let mut excess = self.rows - rows;
            let below = self.lines.len().saturating_sub(self.cursor.row + 1);
            let from_bottom = excess.min(below);
            for _ in 0..from_bottom {
                self.lines.pop();
            }
            excess -= from_bottom;
            for _ in 0..excess {
                let line = self.lines.remove(0);
                self.push_scrollback(line);
                self.cursor.row = self.cursor.row.saturating_sub(1);
            }
        } else if rows > self.rows {
            for _ in 0..(rows - self.rows) {
                self.lines.push(blank_line(cols));
            }
        }

        self.rows = rows;
        self.cols = cols;
        self.region_top = 0;
        self.region_bottom = rows - 1;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.view_offset = self.view_offset.min(self.scrollback.len());
    }

    /// rewrap soft-wrapped logical lines to `new_cols` across scrollback + the
    /// live screen, preserving the cursor's logical position. wide glyphs that
    /// straddle the new boundary may split (rare). sets self.cols
    fn reflow(&mut self, new_cols: usize) {
        let cur_abs = self.scrollback.len() + self.cursor.row;
        let cur_col = self.cursor.col;
        let mut physical: Vec<Line> = Vec::with_capacity(self.scrollback.len() + self.lines.len());
        physical.extend(self.scrollback.drain(..));
        physical.append(&mut self.lines);

        // drop blank lines below the content/cursor so empty screen space can't
        // push real content into scrollback when the width shrinks
        let last_content = physical
            .iter()
            .rposition(|l| l.iter().any(|c| *c != Cell::default()))
            .unwrap_or(0);
        physical.truncate(last_content.max(cur_abs) + 1);

        // join physical lines into logical lines across soft-wraps; record the
        // cursor's logical line index + character offset within it
        let mut logical: Vec<Vec<Cell>> = Vec::new();
        let (mut cur_logical, mut cur_offset, mut found) = (0usize, 0usize, false);
        let mut i = 0;
        while i < physical.len() {
            let li = logical.len();
            let mut cells: Vec<Cell> = Vec::new();
            loop {
                if i == cur_abs && !found {
                    cur_logical = li;
                    cur_offset = cells.len() + cur_col;
                    found = true;
                }
                let wrapped = physical[i].wrapped;
                cells.extend_from_slice(&physical[i].cells);
                i += 1;
                if !wrapped || i >= physical.len() {
                    break;
                }
            }
            // trim trailing blanks, but never past the cursor on its own line
            let keep = if found && cur_logical == li { cur_offset } else { 0 };
            while cells.len() > keep && cells.last() == Some(&Cell::default()) {
                cells.pop();
            }
            logical.push(cells);
        }
        if !found {
            cur_logical = logical.len().saturating_sub(1);
            cur_offset = logical.get(cur_logical).map_or(0, Vec::len);
        }

        // re-split each logical line at the new width
        let mut np: Vec<Line> = Vec::new();
        let mut new_cur_abs = 0usize;
        for (li, cells) in logical.iter().enumerate() {
            let start = np.len();
            if cells.is_empty() {
                np.push(blank_line(new_cols));
            } else {
                let mut j = 0;
                while j < cells.len() {
                    let end = (j + new_cols).min(cells.len());
                    let mut seg: Vec<Cell> = cells[j..end].to_vec();
                    seg.resize(new_cols, Cell::default());
                    np.push(Line { cells: seg, wrapped: end < cells.len() });
                    j = end;
                }
            }
            if li == cur_logical {
                new_cur_abs = start + cur_offset / new_cols;
            }
        }
        if np.is_empty() {
            np.push(blank_line(new_cols));
        }
        new_cur_abs = new_cur_abs.min(np.len() - 1);

        // the last `rows` physical lines become the live screen; rest -> scrollback
        let rows = self.rows;
        let live_start = np.len().saturating_sub(rows);
        self.lines = np.split_off(live_start);
        self.scrollback = np.into();
        while self.lines.len() < rows {
            self.lines.push(blank_line(new_cols));
        }

        self.cursor.row = new_cur_abs.saturating_sub(live_start).min(rows - 1);
        self.cursor.col = (cur_offset % new_cols).min(new_cols - 1);
        self.cursor.wrap_pending = false;
        self.cols = new_cols;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
        // prompt marks reference pre-reflow line indices; reset rather than mis-jump
        self.prompts.clear();
        self.total_scrolled = self.scrollback.len() as u64;
        self.view_offset = 0;
    }

    fn push_scrollback(&mut self, line: Line) {
        self.scrollback.push_back(line);
        self.total_scrolled += 1;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
        self.prune_prompts();
    }

    /// absolute index of the oldest retained line (scrollback front)
    fn prompt_base(&self) -> u64 {
        self.total_scrolled - self.scrollback.len() as u64
    }

    fn prune_prompts(&mut self) {
        let base = self.prompt_base();
        if self.prompts.first().is_some_and(|&p| p < base) {
            self.prompts.retain(|&p| p >= base);
        }
    }

    /// record a prompt start (OSC 133 ;A) at the current cursor row; keeps the
    /// list strictly ascending, dropping any later marks an in-place redraw
    /// (e.g. a screen clear) has invalidated
    pub fn mark_prompt(&mut self) {
        let abs = self.total_scrolled + self.cursor.row as u64;
        while self.prompts.last().is_some_and(|&l| l >= abs) {
            self.prompts.pop();
        }
        self.prompts.push(abs);
    }

    /// scroll to the next (forward) or previous prompt mark relative to the
    /// current viewport top; returns true if the view moved
    pub fn jump_prompt(&mut self, forward: bool) -> bool {
        if self.prompts.is_empty() {
            return false;
        }
        let base = self.prompt_base();
        let total = self.total_lines() as u64;
        let top_g = total.saturating_sub((self.rows + self.view_offset) as u64);
        // reference the viewport center, where scroll_to_global parks the focused
        // prompt — stepping off the top would re-select the same mark
        let cur_abs = base + top_g + (self.rows / 2) as u64;
        let target = if forward {
            self.prompts.iter().copied().find(|&p| p > cur_abs)
        } else {
            self.prompts.iter().copied().rev().find(|&p| p < cur_abs)
        };
        if let Some(abs) = target {
            let g = abs.saturating_sub(base) as usize;
            self.scroll_to_global(g);
            true
        } else {
            false
        }
    }

    /// set the scrollback cap, trimming existing history so a lower limit
    /// takes effect immediately rather than only on the next eviction
    pub fn set_scrollback_limit(&mut self, n: usize) {
        self.scrollback_limit = n;
        while self.scrollback.len() > n {
            self.scrollback.pop_front();
        }
        self.prune_prompts();
        self.view_offset = self.view_offset.min(self.scrollback.len());
    }

    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        // xterm ignores an invalid region (top >= bottom) entirely — no region
        // change and no cursor move
        if top >= bottom {
            return;
        }
        self.region_top = top;
        self.region_bottom = bottom;
        self.cursor.row = self.region_top;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
    }

    pub fn put_char(&mut self, c: char) {
        self.view_offset = 0; // snap to bottom on new output
        let w = char_width(c);
        // zero-width (combining marks, ZWJ, variation selectors): drop for now
        if w == 0 {
            return;
        }
        if self.cursor.wrap_pending {
            self.lines[self.cursor.row].wrapped = true;
            self.cursor.col = 0;
            self.linefeed();
            self.cursor.wrap_pending = false;
        }
        // a double-width glyph that won't fit in the last column wraps first
        if w == 2 && self.cursor.col + 2 > self.cols {
            self.lines[self.cursor.row].wrapped = true;
            self.cursor.col = 0;
            self.linefeed();
        }
        let row = self.cursor.row;
        let col = self.cursor.col;
        let (fg, bg, attrs, link) = (self.cursor.fg, self.cursor.bg, self.cursor.attrs, self.cursor.link);
        // reconcile a wide pair we're partially overwriting so no orphan lead or
        // continuation cell is left behind to render as a phantom gap
        if col + 1 < self.cols && self.lines[row][col + 1].c == '\0' {
            self.lines[row][col + 1] = Cell { c: ' ', fg, bg, attrs, link };
        }
        if col > 0 && self.lines[row][col].c == '\0' {
            self.lines[row][col - 1] = Cell { c: ' ', fg, bg, attrs, link };
        }
        self.lines[row][col] = Cell { c, fg, bg, attrs, link };
        if w == 2 && col + 1 < self.cols {
            // continuation cell marks the second half of a wide glyph
            self.lines[row][col + 1] = Cell { c: '\0', fg, bg, attrs, link };
        }
        if col + w >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.wrap_pending = true;
        } else {
            self.cursor.col += w;
        }
    }

    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    pub fn backspace(&mut self) {
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
        } else {
            self.cursor.col = self.cursor.col.saturating_sub(1);
        }
    }

    pub fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// move down one line, scrolling the region if at the bottom (LF/IND)
    pub fn linefeed(&mut self) {
        self.view_offset = 0;
        if self.cursor.row == self.region_bottom {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
        self.cursor.wrap_pending = false;
    }

    /// move up one line, scrolling the region down if at the top (RI)
    pub fn reverse_index(&mut self) {
        if self.cursor.row == self.region_top {
            self.scroll_down(1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.cursor.wrap_pending = false;
    }

    pub fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.region_bottom - self.region_top + 1);
        for _ in 0..n {
            let line = self.lines.remove(self.region_top);
            if self.region_top == 0 {
                self.push_scrollback(line);
            }
            self.lines
                .insert(self.region_bottom, blank_line(self.cols));
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.region_bottom - self.region_top + 1);
        for _ in 0..n {
            self.lines.remove(self.region_bottom);
            self.lines.insert(self.region_top, blank_line(self.cols));
        }
    }

    pub fn goto(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    pub fn move_up(&mut self, n: usize) {
        // cuu stops at the top margin only when the cursor starts at or below
        // it; from above the margin it stops at the top of the screen (xterm)
        let floor = if self.cursor.row >= self.region_top { self.region_top } else { 0 };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(floor);
        self.cursor.wrap_pending = false;
    }

    pub fn move_down(&mut self, n: usize) {
        // symmetric to move_up: cud stops at the bottom margin only when the
        // cursor starts at or above it, else at the bottom of the screen
        let ceil = if self.cursor.row <= self.region_bottom {
            self.region_bottom
        } else {
            self.rows - 1
        };
        self.cursor.row = (self.cursor.row + n).min(ceil);
        self.cursor.wrap_pending = false;
    }

    pub fn move_left(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
        self.cursor.wrap_pending = false;
    }

    pub fn move_right(&mut self, n: usize) {
        self.cursor.col = (self.cursor.col + n).min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// erase in line: 0 = cursor..end, 1 = start..=cursor, 2 = whole line
    pub fn erase_in_line(&mut self, mode: u16) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let blank = self.blank_cell();
        match mode {
            0 => {
                for c in col..self.cols {
                    self.lines[row][c] = blank;
                }
            }
            1 => {
                for c in 0..=col.min(self.cols - 1) {
                    self.lines[row][c] = blank;
                }
            }
            2 => {
                for c in 0..self.cols {
                    self.lines[row][c] = blank;
                }
            }
            _ => {}
        }
    }

    /// erase in display: 0 = cursor..end, 1 = start..=cursor, 2/3 = whole screen
    pub fn erase_in_display(&mut self, mode: u16) {
        let row = self.cursor.row;
        let blank = self.blank_cell();
        match mode {
            0 => {
                self.erase_in_line(0);
                for r in (row + 1)..self.rows {
                    for c in 0..self.cols {
                        self.lines[r][c] = blank;
                    }
                }
            }
            1 => {
                self.erase_in_line(1);
                for r in 0..row {
                    for c in 0..self.cols {
                        self.lines[r][c] = blank;
                    }
                }
            }
            2 | 3 => {
                for r in 0..self.rows {
                    for c in 0..self.cols {
                        self.lines[r][c] = blank;
                    }
                }
            }
            _ => {}
        }
    }

    /// insert n blank chars at cursor, shifting the rest of the line right (ICH)
    pub fn insert_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let blank = self.blank_cell();
        let n = n.min(self.cols - col);
        for _ in 0..n {
            self.lines[row].insert(col, blank);
            self.lines[row].truncate(self.cols);
        }
    }

    /// delete n chars at cursor, shifting the rest of the line left (DCH)
    pub fn delete_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let blank = self.blank_cell();
        let n = n.min(self.cols - col);
        for _ in 0..n {
            self.lines[row].remove(col);
            self.lines[row].push(blank);
        }
    }

    /// erase n chars from the cursor in place (ECH); fills with the current
    /// sgr background so erase-with-bce matches every other erase op
    pub fn erase_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let blank = self.blank_cell();
        let end = (col + n).min(self.cols);
        for c in col..end {
            self.lines[row][c] = blank;
        }
    }

    /// insert n blank lines at cursor row within the region (IL)
    pub fn insert_lines(&mut self, n: usize) {
        let row = self.cursor.row;
        if row < self.region_top || row > self.region_bottom {
            return;
        }
        let n = n.min(self.region_bottom - row + 1);
        for _ in 0..n {
            self.lines.remove(self.region_bottom);
            self.lines.insert(row, blank_line(self.cols));
        }
    }

    /// delete n lines at cursor row within the region (DL)
    pub fn delete_lines(&mut self, n: usize) {
        let row = self.cursor.row;
        if row < self.region_top || row > self.region_bottom {
            return;
        }
        let n = n.min(self.region_bottom - row + 1);
        for _ in 0..n {
            self.lines.remove(row);
            self.lines.insert(self.region_bottom, blank_line(self.cols));
        }
    }

    fn blank_cell(&self) -> Cell {
        // erasing uses the current background color per the VT spec
        Cell {
            c: ' ',
            fg: self.cursor.fg,
            bg: self.cursor.bg,
            attrs: Attrs::default(),
            link: 0,
        }
    }

    /// set the active OSC 8 hyperlink (None or empty ends it); printed cells
    /// carry the interned id
    pub fn set_link(&mut self, uri: Option<&str>) {
        self.cursor.link = match uri {
            Some(u) if !u.is_empty() => self.intern_link(u),
            _ => 0,
        };
    }

    fn intern_link(&mut self, uri: &str) -> u16 {
        if let Some(i) = self.links.iter().position(|l| l == uri) {
            return i as u16;
        }
        if self.links.len() >= u16::MAX as usize {
            return 0;
        }
        self.links.push(uri.to_string());
        (self.links.len() - 1) as u16
    }

    /// the URI a cell's link id points at, if any
    pub fn link_uri(&self, id: u16) -> Option<&str> {
        if id == 0 {
            return None;
        }
        self.links.get(id as usize).map(|s| s.as_str())
    }

    /// the contiguous run of cells on `row` sharing hyperlink `id`, as a
    /// [start, end) column range (end exclusive, matching url_at)
    pub fn link_span(&self, row: usize, col: usize, id: u16) -> (usize, usize) {
        let line = self.line_at(row);
        let mut start = col;
        while start > 0 && line.get(start - 1).map(|c| c.link) == Some(id) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < self.cols && line.get(end + 1).map(|c| c.link) == Some(id) {
            end += 1;
        }
        (start, end + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_last_column() {
        let mut g = Grid::new(3, 4);
        for c in "abcd".chars() {
            g.put_char(c);
        }
        // after 4 chars in a 4-wide grid, wrap is pending, cursor still row 0
        assert_eq!(g.cursor.row, 0);
        assert!(g.cursor.wrap_pending);
        g.put_char('e');
        assert_eq!(g.cursor.row, 1);
        assert_eq!(g.cursor.col, 1);
        assert_eq!(g.lines[1][0].c, 'e');
    }

    #[test]
    fn linefeed_scrolls_and_fills_scrollback() {
        let mut g = Grid::new(2, 3);
        for c in "ab".chars() {
            g.put_char(c);
        }
        g.cursor.row = 1;
        g.linefeed(); // at bottom -> scroll
        assert_eq!(g.scrollback.len(), 1);
        assert_eq!(g.scrollback[0][0].c, 'a');
    }

    #[test]
    fn erase_display_clears() {
        let mut g = Grid::new(2, 2);
        g.put_char('x');
        g.erase_in_display(2);
        assert_eq!(g.lines[0][0].c, ' ');
    }

    #[test]
    fn ech_fills_with_current_background() {
        let mut g = Grid::new(2, 5);
        g.cursor.bg = Color::Indexed(4);
        g.erase_chars(3);
        for c in 0..3 {
            assert_eq!(g.lines[0][c].c, ' ');
            assert_eq!(g.lines[0][c].bg, Color::Indexed(4));
        }
        // past the erased run keeps the default background
        assert_eq!(g.lines[0][3].bg, Color::DefaultBg);
    }

    #[test]
    fn cuu_cud_stop_at_screen_edge_when_outside_region() {
        let mut g = Grid::new(10, 4);
        g.set_scroll_region(3, 7);
        // above the top margin: cuu must reach the top of the screen, not snap
        // down into the region
        g.goto(1, 0);
        g.move_up(5);
        assert_eq!(g.cursor.row, 0);
        // below the bottom margin: cud must reach the bottom of the screen
        g.goto(8, 0);
        g.move_down(5);
        assert_eq!(g.cursor.row, 9);
        // inside the region the margins still bound the motion
        g.goto(5, 0);
        g.move_up(10);
        assert_eq!(g.cursor.row, 3);
        g.goto(5, 0);
        g.move_down(10);
        assert_eq!(g.cursor.row, 7);
    }

    #[test]
    fn wide_char_writes_continuation_and_advances_two() {
        let mut g = Grid::new(2, 6);
        g.put_char('世');
        assert_eq!(g.lines[0][0].c, '世');
        assert_eq!(g.lines[0][1].c, '\0');
        assert_eq!(g.cursor.col, 2);
    }

    #[test]
    fn overwriting_wide_lead_clears_orphan_continuation() {
        let mut g = Grid::new(2, 6);
        g.put_char('世');
        g.cursor.col = 0;
        g.put_char('a');
        assert_eq!(g.lines[0][0].c, 'a');
        // the stale continuation must be blanked, not left as a phantom gap
        assert_eq!(g.lines[0][1].c, ' ');
    }

    #[test]
    fn overwriting_wide_tail_clears_orphan_lead() {
        let mut g = Grid::new(2, 6);
        g.put_char('世');
        g.cursor.col = 1;
        g.put_char('b');
        assert_eq!(g.lines[0][1].c, 'b');
        // the now-orphaned lead must be blanked
        assert_eq!(g.lines[0][0].c, ' ');
    }

    #[test]
    fn url_at_detects_web_link_and_trims_punctuation() {
        let mut g = Grid::new(2, 40);
        for c in "see http://a.com/x.".chars() {
            g.put_char(c);
        }
        // cursor inside the url -> detected, trailing '.' trimmed
        let (s, e, url) = g.url_at(0, 10).expect("url under cursor");
        assert_eq!(url, "http://a.com/x");
        assert_eq!(s, 4);
        assert_eq!(e, 4 + "http://a.com/x".len());
        // cursor over the leading "see" is not a link
        assert!(g.url_at(0, 1).is_none());
    }

    #[test]
    fn selected_text_reads_scrollback_when_scrolled() {
        let mut g = Grid::new(2, 4);
        for line in ["aa", "bb", "cc", "dd"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // older rows pushed to scrollback; scroll the view to the top
        g.scroll_view(g.scrollback.len() as isize);
        // viewport rows now show scrollback, and a selection over them must copy
        // what is visible, not the live screen
        assert_eq!(g.selected_text((0, 0), (0, 1)), "aa");
        assert_eq!(g.selected_text((1, 0), (1, 1)), "bb");
        assert_eq!(g.selected_text((0, 0), (1, 1)), "aa\nbb");
    }

    #[test]
    fn prompt_marks_prune_and_jump() {
        let mut g = Grid::new(3, 8);
        g.set_scrollback_limit(100);
        // five prompts separated by several line feeds so they land in distinct
        // scroll positions
        for _ in 0..5 {
            g.mark_prompt();
            for _ in 0..5 {
                g.linefeed();
            }
        }
        // generous limit: nothing evicted, every mark within the retained window
        assert_eq!(g.prompts.len(), 5);
        assert!(g.prompts.iter().all(|&p| p >= g.prompt_base()));
        // from the live bottom, jumping back reaches a prompt and scrolls up
        assert!(g.jump_prompt(false));
        let v1 = g.view_offset;
        assert!(v1 > 0);
        // jumping back again reaches an earlier prompt, further up
        assert!(g.jump_prompt(false));
        let v2 = g.view_offset;
        assert!(v2 > v1);
        // forward brings the view back down toward the live screen
        assert!(g.jump_prompt(true));
        assert!(g.view_offset < v2);
    }

    #[test]
    fn jump_prompt_without_marks_is_noop() {
        let mut g = Grid::new(3, 8);
        assert!(!g.jump_prompt(false));
        assert!(!g.jump_prompt(true));
        assert_eq!(g.view_offset, 0);
    }

    fn row_text(g: &Grid, r: usize) -> String {
        g.lines[r].iter().map(|c| c.c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn reflow_rejoins_and_resplits_on_width_increase() {
        let mut g = Grid::new(5, 20);
        for c in "0123456789ABCDEFGHIJabcdefghij0123456789ABCDEFGHIJ".chars() {
            g.put_char(c);
        }
        g.resize(5, 40);
        assert_eq!(row_text(&g, 0), "0123456789ABCDEFGHIJabcdefghij0123456789");
        assert_eq!(row_text(&g, 1), "ABCDEFGHIJ");
        // cursor stays at the logical end (offset 50 -> row 1, col 10)
        assert_eq!((g.cursor.row, g.cursor.col), (1, 10));
    }

    #[test]
    fn reflow_splits_on_width_decrease_without_losing_content() {
        let mut g = Grid::new(5, 40);
        for c in "012345678901234567890123456789".chars() {
            g.put_char(c);
        }
        g.resize(5, 20);
        assert_eq!(row_text(&g, 0), "01234567890123456789");
        assert_eq!(row_text(&g, 1), "0123456789");
    }

    #[test]
    fn reflow_preserves_hard_newlines() {
        let mut g = Grid::new(5, 20);
        for c in "AAAA".chars() {
            g.put_char(c);
        }
        g.carriage_return();
        g.linefeed();
        for c in "BBBBBBBBBBBBBBBBBBBBBBBBB".chars() {
            g.put_char(c);
        }
        g.resize(5, 40);
        assert_eq!(row_text(&g, 0), "AAAA");
        assert_eq!(row_text(&g, 1), "BBBBBBBBBBBBBBBBBBBBBBBBB");
    }

    #[test]
    fn reflow_preserves_scrollback_content() {
        let mut g = Grid::new(4, 10);
        g.set_scrollback_limit(100);
        // ten distinct hard-newline lines on a 4-row screen, so six scroll into
        // scrollback; "line-NN" is 7 chars so nothing wraps at width 10
        for i in 0..10 {
            for ch in format!("line-{i:02}").chars() {
                g.put_char(ch);
            }
            g.carriage_return();
            g.linefeed();
        }
        // rejoin soft-wrapped physical rows into logical lines before reading,
        // so the check holds even at a width where the labels wrap
        let collect_labels = |g: &Grid| -> Vec<String> {
            let mut out = Vec::new();
            let mut cur = String::new();
            for l in g.scrollback.iter().chain(g.lines.iter()) {
                cur.extend(l.iter().map(|c| c.c));
                if !l.wrapped {
                    let t = cur.trim_end().to_string();
                    if t.starts_with("line-") {
                        out.push(t);
                    }
                    cur.clear();
                }
            }
            out
        };
        let expect: Vec<String> = (0..10).map(|i| format!("line-{i:02}")).collect();
        // widen then narrow: reflow drains + rejoins + redistributes scrollback
        // each time, and must never lose, duplicate, or reorder a line
        g.resize(4, 20);
        assert_eq!(collect_labels(&g), expect, "after widen");
        g.resize(4, 6);
        assert_eq!(collect_labels(&g), expect, "after narrow (each line wraps)");
        g.resize(4, 30);
        assert_eq!(collect_labels(&g), expect, "after re-widen");
    }
}
