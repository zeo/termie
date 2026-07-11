use crate::fxhash::FxHashMap;
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
    /// SGR 53 overline
    pub overline: bool,
    /// SGR 58/59 underline color; Default = draw decorations in the cell's fg
    pub ul: crate::color::Color,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// OSC 8 hyperlink id into Grid::links; 0 = no link
    pub link: u16,
    /// grapheme-cluster id into Grid::clusters (combining marks / ZWJ / VS);
    /// 0 = the cell is just its base char `c`
    pub cluster: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::DefaultBg,
            attrs: Attrs::default(),
            link: 0,
            cluster: 0,
        }
    }
}

/// a placed kitty image: anchored to an absolute line (col, abs_line) so it
/// scrolls with the text. image_id indexes the owning Terminal's ImageStore;
/// the image draws at its native pixel size from that cell, unless the client
/// asked to fit it to a cell box (kitty c=/r= keys; 0 = natural size)
#[derive(Clone, Copy)]
pub struct Placement {
    pub image_id: u32,
    pub abs_line: u64,
    pub col: usize,
    pub cols: u16,
    pub rows: u16,
    /// kitty z=: negative stacks beneath the pane's text, 0+ above it
    pub z: i32,
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
    /// DECSCUSR's blink bit: Some(false) for the steady variants (2/4/6),
    /// Some(true) for the blinking ones; None = follow the configured default
    pub shape_blink: Option<bool>,
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
            shape_blink: None,
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

fn word_class_at(line: &Line, col: usize) -> u8 {
    let c = line.get(col).map_or(' ', |cell| cell.c);
    let c = if c == '\0' && col > 0 { line[col - 1].c } else { c };
    if c == ' ' || c == '\0' {
        0
    } else if c.is_alphanumeric() || "_./-:~@+".contains(c) {
        1
    } else {
        2
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
    /// DECOM origin mode: CUP/VPA address relative to the scroll region top
    pub origin_mode: bool,
    /// IRM insert mode (ANSI mode 4): prints shift the rest of the line right
    pub insert_mode: bool,
    /// DECAWM autowrap (mode 7): off pins prints at the right margin instead
    /// of wrapping to the next line
    pub autowrap: bool,
    /// origin mode saved by DECSC, restored by DECRC
    saved_origin: bool,
    /// scrollback view offset (lines scrolled up from the live bottom)
    pub view_offset: usize,
    /// total lines ever pushed into scrollback (monotonic); lets prompt marks
    /// use stable absolute indices that survive eviction
    total_scrolled: u64,
    /// bumped whenever reflow re-bases the absolute line-id space, so anything
    /// anchored to it (the app's selection) knows its coordinates went stale
    pub reflow_gen: u32,
    /// absolute line indices of OSC 133 prompt starts, ascending; pruned as
    /// history is evicted
    prompts: Vec<PromptMark>,
    /// horizontal tab stops, one flag per column (HTS/TBC); defaults every 8
    tab_stops: Vec<bool>,
    /// OSC 8 hyperlink targets; a cell's link id indexes here, 0 = none and
    /// links[0] is the empty sentinel
    pub links: Vec<String>,
    /// grapheme clusters; a cell's cluster id indexes here, 0 = none and
    /// clusters[0] is the empty sentinel
    clusters: Vec<String>,
    /// reverse index (cluster text -> id) so interning is O(1) instead of a
    /// linear scan of `clusters` — matters for varied non-Latin text where the
    /// table grows toward its cap
    cluster_index: FxHashMap<String, u32>,
    /// reused scratch for building a cluster's text in append_combining, so the
    /// common (already-interned) combining char costs no per-mark allocation
    cluster_scratch: String,
    /// kitty graphics placements anchored to absolute lines (scroll with text)
    placements: Vec<Placement>,
}

fn blank_line(cols: usize) -> Line {
    Line {
        cells: vec![Cell::default(); cols],
        wrapped: false,
    }
}

fn default_tab_stops(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0).collect()
}

/// an OSC 133 prompt mark: the absolute line of the prompt start plus the
/// command's exit code once its `D` arrives (None while running, or when the
/// shell omits the code)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PromptMark {
    pub line: u64,
    pub exit: Option<i32>,
}

/// terminal cell width of a char: 0 (combining/zero-width), 1 (normal), or 2
/// (East-Asian wide / fullwidth / emoji). a compact built-in table (no deps)
pub(crate) fn char_width(c: char) -> usize {
    let cp = c as u32;
    // fast path: printable ASCII is width 1 and dominates shell output — return
    // before the ~38 combining/wide range checks below
    if (0x20..0x7f).contains(&cp) {
        return 1;
    }
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
    // wide: East Asian Wide/Fullwidth + common emoji blocks. the scattered
    // singletons below 0x2800 are the emoji-presentation-by-default symbols
    // EastAsianWidth.txt marks W (watch, hourglass, zodiac, ball sports...)
    if matches!(cp,
        0x1100..=0x115F | 0x231A..=0x231B | 0x2329 | 0x232A | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653
        | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5
        | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
        | 0x1F200..=0x1F251 | 0x1F300..=0x1F64F | 0x1F680..=0x1F6C5 | 0x1F6CC
        | 0x1F6D0..=0x1F6D2 | 0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF
        | 0x1F6EB..=0x1F6EC | 0x1F6F4..=0x1F6FC | 0x1F7E0..=0x1F7EB | 0x1F7F0
        | 0x1F900..=0x1FAFF | 0x20000..=0x3FFFD)
    {
        return 2;
    }
    1
}

/// narrow-by-default bases of Unicode's emoji-variation-sequences.txt: a VS16
/// (U+FE0F) after one of these requests emoji presentation, and the cell
/// promotes to double width. kitty, ghostty, and rio settled on promotion;
/// alacritty and WT leave the color glyph overflowing its single cell
pub(crate) fn emoji_vs_base(c: char) -> bool {
    matches!(c as u32,
        0x23 | 0x2A | 0x30..=0x39 | 0xA9 | 0xAE | 0x203C | 0x2049 | 0x2122
        | 0x2139 | 0x2194..=0x2199 | 0x21A9..=0x21AA | 0x2328 | 0x23CF
        | 0x23ED..=0x23EF | 0x23F1..=0x23F2 | 0x23F8..=0x23FA | 0x24C2
        | 0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FC | 0x2600..=0x2604
        | 0x260E | 0x2611 | 0x2618 | 0x261D | 0x2620 | 0x2622..=0x2623 | 0x2626
        | 0x262A | 0x262E..=0x262F | 0x2638..=0x263A | 0x2640 | 0x2642
        | 0x265F..=0x2660 | 0x2663 | 0x2665..=0x2666 | 0x2668 | 0x267B | 0x267E
        | 0x2692..=0x2697 | 0x2699 | 0x269B..=0x269C | 0x26A0 | 0x26A7
        | 0x26B0..=0x26B1 | 0x26C8 | 0x26CF | 0x26D1 | 0x26D3 | 0x26E9
        | 0x26F0..=0x26F1 | 0x26F4 | 0x26F7..=0x26F9 | 0x2702 | 0x2708..=0x2709
        | 0x270C..=0x270D | 0x270F | 0x2712 | 0x2714 | 0x2716 | 0x271D | 0x2721
        | 0x2733..=0x2734 | 0x2744 | 0x2747 | 0x2763..=0x2764 | 0x27A1
        | 0x2934..=0x2935 | 0x2B05..=0x2B07 | 0x2B1B..=0x2B1C | 0x3030 | 0x303D
        | 0x3297 | 0x3299 | 0x1F170..=0x1F171 | 0x1F17E..=0x1F17F | 0x1F6CB
        | 0x1F6CD..=0x1F6CF | 0x1F6E0..=0x1F6E5 | 0x1F6E9 | 0x1F6F0 | 0x1F6F3)
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
            clusters: vec![String::new()],
            cluster_index: FxHashMap::default(),
            cluster_scratch: String::new(),
            placements: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            region_top: 0,
            region_bottom: rows - 1,
            origin_mode: false,
            insert_mode: false,
            autowrap: true,
            saved_origin: false,
            view_offset: 0,
            total_scrolled: 0,
            reflow_gen: 0,
            prompts: Vec::new(),
            tab_stops: default_tab_stops(cols),
        }
    }

    /// base of the absolute line-id space shared with prompt marks: the id of
    /// the oldest retained line. ids are stable across scrollback eviction and
    /// live/scrollback moves; only reflow re-bases them (see reflow_gen)
    pub fn abs_base(&self) -> u64 {
        self.total_scrolled - self.scrollback.len() as u64
    }

    /// absolute line id shown at viewport row `r` (clamped to the last line)
    pub fn viewport_to_abs(&self, r: usize) -> u64 {
        let total = self.total_lines();
        let start = total.saturating_sub(self.rows + self.view_offset);
        self.abs_base() + ((start + r).min(total.saturating_sub(1))) as u64
    }

    /// viewport row currently displaying absolute line `abs`, or None when it
    /// scrolled off-screen or was evicted
    pub fn abs_to_viewport(&self, abs: u64) -> Option<usize> {
        let g = abs.checked_sub(self.abs_base())?;
        self.global_to_viewport(g as usize)
    }

    /// the retained line with absolute id `abs`, if history still holds it
    pub fn line_at_abs(&self, abs: u64) -> Option<&Line> {
        let g = abs.checked_sub(self.abs_base())? as usize;
        if g < self.scrollback.len() {
            Some(&self.scrollback[g])
        } else {
            self.lines.get(g - self.scrollback.len())
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
        // word-class includes punctuation that usually reads as part of a path,
        // url, flag, or identifier in terminal output
        let here = word_class_at(line, col);
        let mut lo = col;
        while lo > 0 && word_class_at(line, lo - 1) == here {
            lo -= 1;
        }
        let mut hi = col;
        while hi + 1 < n && word_class_at(line, hi + 1) == here {
            hi += 1;
        }
        (lo, hi)
    }

    /// ctrl+arrow target for keyboard selection, anchored to retained content
    pub fn word_boundary(&self, pos: (u64, usize), forward: bool) -> (u64, usize) {
        let base = self.abs_base();
        let cells = self.total_lines().saturating_mul(self.cols);
        let last = cells.saturating_sub(1);
        let row = pos.0.saturating_sub(base).min(self.total_lines().saturating_sub(1) as u64);
        let mut at = (row as usize)
            .saturating_mul(self.cols)
            .saturating_add(pos.1.min(self.cols.saturating_sub(1)))
            .min(last);
        let start = at;
        let class = |i: usize| {
            self.line_at_abs(base + (i / self.cols) as u64)
                .map_or(0, |line| word_class_at(line, i % self.cols))
        };
        let joined = |a: usize, b: usize| {
            a / self.cols == b / self.cols
                || self
                    .line_at_abs(base + (a.min(b) / self.cols) as u64)
                    .is_some_and(|line| line.wrapped)
        };
        let step = |i: usize| {
            if forward && i < last {
                Some(i + 1)
            } else if !forward && i > 0 {
                Some(i - 1)
            } else {
                None
            }
        };
        let finish = |i: usize| {
            (base + (i / self.cols) as u64, i % self.cols)
        };
        let here = class(at);
        if here != 0 {
            while let Some(next) = step(at) {
                if class(next) != here || !joined(at, next) {
                    break;
                }
                at = next;
            }
            if at != start {
                return finish(at);
            }
        }
        let mut found = if here == 0 { Some(at) } else { step(at) };
        while let Some(next) = found {
            if class(next) != 0 {
                at = next;
                break;
            }
            at = next;
            found = step(at);
        }
        let target_class = class(at);
        if target_class == 0 {
            return finish(at);
        }
        while let Some(next) = step(at) {
            if class(next) != target_class || !joined(at, next) {
                break;
            }
            at = next;
        }
        finish(at)
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
    /// returns (global_line_index, col, char_len) per match, in top-to-bottom
    /// order. global indices span scrollback (0..len) then live lines.
    /// soft-wrapped runs are joined (same as copy), so a long URL that wrapped
    /// still matches; wide-glyph continuation cells (`\0`) are skipped
    pub fn search(&self, needle: &str) -> Vec<(usize, usize, usize)> {
        let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
        let needle: Vec<char> = needle.chars().map(fold).collect();
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        self.each_logical_run(|text, map| {
            if text.len() >= needle.len() {
                for start in 0..=(text.len() - needle.len()) {
                    if text[start..start + needle.len()] == needle[..] {
                        let (line, col) = map[start];
                        out.push((line, col, needle.len()));
                    }
                }
            }
        });
        out
    }

    /// regex search over the same corpus as `search`, same result shape; the
    /// shared budget keeps a pathological pattern from freezing the UI
    pub fn search_regex(&self, re: &crate::regex::Regex) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        let mut budget = 4_000_000usize;
        self.each_logical_run(|text, map| {
            for (s, e) in re.find_all(text, &mut budget) {
                let (line, col) = map[s];
                out.push((line, col, e - s));
            }
        });
        out
    }

    /// walk every logical run (a row plus its soft-wrapped continuations) as
    /// case-folded chars, with text[i] mapping back to (global_line, col).
    /// one-to-one folding (first char of to_lowercase) so É matches é and
    /// Cyrillic/Greek fold too, while every char keeps mapping to one cell
    fn each_logical_run(&self, mut f: impl FnMut(&[char], &[(usize, usize)])) {
        let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
        let total = self.total_lines();
        let line_at_global = |gi: usize| -> &Line {
            if gi < self.scrollback.len() {
                &self.scrollback[gi]
            } else {
                &self.lines[gi - self.scrollback.len()]
            }
        };
        let mut text: Vec<char> = Vec::new();
        let mut map: Vec<(usize, usize)> = Vec::new();
        let mut gi = 0;
        while gi < total {
            text.clear();
            map.clear();
            loop {
                let line = line_at_global(gi);
                for (col, cell) in line.iter().enumerate() {
                    // skip the empty half of a wide glyph so "a漢b" still matches
                    if cell.c == '\0' {
                        continue;
                    }
                    text.push(fold(cell.c));
                    map.push((gi, col));
                }
                if !line.wrapped || gi + 1 >= total {
                    break;
                }
                gi += 1;
            }
            f(&text, &map);
            gi += 1;
        }
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

    /// linear text within [start, end] as (absolute line id, col), trailing
    /// blanks trimmed per row. absolute anchoring means the result is what the
    /// user highlighted no matter how the view scrolled since; lines evicted
    /// between mouse-down and copy are skipped
    pub fn selected_text(&self, start: (u64, usize), end: (u64, usize), block: bool) -> String {
        let (mut a, mut b) = (start, end);
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        // a block selection spans the same column range on every row
        let (bc0, bc1) = (a.1.min(b.1), a.1.max(b.1));
        let mut out = String::new();
        let base = self.abs_base();
        let last = base + self.total_lines().saturating_sub(1) as u64;
        let lo = a.0.max(base);
        let hi = b.0.min(last);
        if lo > hi {
            return out;
        }
        for abs in lo..=hi {
            let Some(line) = self.line_at_abs(abs) else {
                continue;
            };
            // clamp both ends to the line length: a resize can shrink lines
            // between mouse-down and copy, leaving start col past the new width
            let len = line.len();
            let (from, to) = if block {
                (bc0.min(len), (bc1 + 1).min(self.cols).min(len))
            } else {
                (
                    (if abs == a.0 { a.1 } else { 0 }).min(len),
                    (if abs == b.0 { (b.1 + 1).min(self.cols) } else { self.cols }).min(len),
                )
            };
            let mut s = String::new();
            for cell in &line[from..to.max(from)] {
                if cell.cluster != 0 {
                    s.push_str(self.cluster_str(cell.cluster));
                } else if cell.c != '\0' {
                    s.push(cell.c);
                }
            }
            // a soft-wrapped row flows straight into the next: the break was
            // the terminal's, not the text's, so copy it unbroken and keep its
            // exact cells (trimming could eat spaces mid-logical-line). block
            // mode is a column extract — every row stays its own line there
            let wrapped = !block && line.wrapped && abs != hi;
            if !wrapped {
                while s.ends_with(' ') {
                    s.pop();
                }
            }
            out.push_str(&s);
            if abs != hi && !wrapped {
                out.push('\n');
            }
        }
        out
    }

    /// inclusive bounds of all retained text, excluding blank screen rows after
    /// the last printed cell
    pub fn full_span(&self) -> Option<((u64, usize), (u64, usize))> {
        let base = self.abs_base();
        (0..self.total_lines()).rev().find_map(|i| {
            let line = self.line_at_abs(base + i as u64)?;
            let col = line
                .iter()
                .rposition(|cell| cell.cluster != 0 || !matches!(cell.c, ' ' | '\0'))?;
            Some(((base, 0), (base + i as u64, col)))
        })
    }

    /// all of history plus the live screen as plain text: soft-wrapped runs
    /// join into one logical line (same rule as copy), hard lines keep their
    /// breaks, trailing blank screen rows are dropped
    pub fn full_text(&self) -> String {
        self.full_span().map_or_else(String::new, |(start, end)| self.selected_text(start, end, false))
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
        // keep custom stops where columns survive; new columns get the default cadence
        if cols < self.tab_stops.len() {
            self.tab_stops.truncate(cols);
        } else {
            for c in self.tab_stops.len()..cols {
                self.tab_stops.push(c % 8 == 0);
            }
        }
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
        let prompt_base = self.prompt_base();
        let mut physical: Vec<Line> = Vec::with_capacity(self.scrollback.len() + self.lines.len());
        physical.extend(self.scrollback.drain(..));
        physical.append(&mut self.lines);
        // translate each retained prompt into its row in the old physical grid
        // reflow turns that row into a logical-line offset, then back into a
        // physical row at the new width
        let prompt_rows: Vec<(usize, Option<i32>)> = self
            .prompts
            .iter()
            .filter_map(|m| m.line.checked_sub(prompt_base).map(|row| (row, m.exit)))
            .filter_map(|(row, exit)| (row < physical.len() as u64).then_some((row as usize, exit)))
            .collect();
        // image placements ride the same translation, so a width change keeps
        // kitty graphics anchored to their content instead of dropping them
        let mut placement_rows: Vec<(usize, Placement)> = self
            .placements
            .drain(..)
            .filter_map(|pl| {
                pl.abs_line
                    .checked_sub(prompt_base)
                    .filter(|&row| row < physical.len() as u64)
                    .map(|row| (row as usize, pl))
            })
            .collect();
        placement_rows.sort_by_key(|&(row, pl)| (row, pl.col));

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
        let mut prompt_offsets: Vec<(usize, usize, Option<i32>)> = Vec::with_capacity(prompt_rows.len());
        let mut placement_offsets: Vec<(usize, usize, Placement)> =
            Vec::with_capacity(placement_rows.len());
        let mut next_prompt = 0usize;
        let mut next_pl = 0usize;
        let (mut cur_logical, mut cur_offset, mut found) = (0usize, 0usize, false);
        let mut i = 0;
        while i < physical.len() {
            let li = logical.len();
            let mut cells: Vec<Cell> = Vec::new();
            loop {
                while let Some(&(row, exit)) = prompt_rows.get(next_prompt) {
                    if row != i {
                        break;
                    }
                    prompt_offsets.push((li, cells.len(), exit));
                    next_prompt += 1;
                }
                while let Some(&(row, pl)) = placement_rows.get(next_pl) {
                    if row != i {
                        break;
                    }
                    placement_offsets.push((li, cells.len() + pl.col, pl));
                    next_pl += 1;
                }
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
        let mut new_prompts = Vec::with_capacity(prompt_offsets.len());
        let mut new_placements: Vec<Placement> = Vec::with_capacity(placement_offsets.len());
        let mut next_prompt = 0usize;
        let mut next_pl = 0usize;
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
            while let Some(&(prompt_li, offset, exit)) = prompt_offsets.get(next_prompt) {
                if prompt_li != li {
                    break;
                }
                let segments = np.len() - start;
                new_prompts.push(PromptMark {
                    line: (start + (offset / new_cols).min(segments - 1)) as u64,
                    exit,
                });
                next_prompt += 1;
            }
            while let Some(&(pl_li, offset, pl)) = placement_offsets.get(next_pl) {
                if pl_li != li {
                    break;
                }
                let segments = np.len() - start;
                new_placements.push(Placement {
                    abs_line: (start + (offset / new_cols).min(segments - 1)) as u64,
                    col: offset % new_cols,
                    ..pl
                });
                next_pl += 1;
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
        let mut evicted = 0u64;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
            evicted += 1;
        }
        self.prompts.clear();
        for m in new_prompts
            .into_iter()
            .filter_map(|m| m.line.checked_sub(evicted).map(|line| PromptMark { line, ..m }))
        {
            if self.prompts.last().map(|p| p.line) != Some(m.line) {
                self.prompts.push(m);
            }
        }
        // placements were remapped alongside the prompts; drop only the ones
        // whose lines the eviction above trimmed away
        self.placements = new_placements
            .into_iter()
            .filter_map(|pl| {
                pl.abs_line.checked_sub(evicted).map(|line| Placement { abs_line: line, ..pl })
            })
            .collect();
        self.total_scrolled = self.scrollback.len() as u64;
        self.reflow_gen = self.reflow_gen.wrapping_add(1);
        self.view_offset = 0;
    }

    fn push_scrollback(&mut self, line: Line) {
        self.scrollback.push_back(line);
        self.total_scrolled += 1;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
        // a view scrolled into history stays anchored to the text being read:
        // each line entering scrollback pushes the offset up with it (typing
        // snaps back to the live bottom — see the app's input path)
        if self.view_offset > 0 {
            self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
        }
        self.prune_prompts();
    }

    /// absolute index of the oldest retained line (scrollback front)
    fn prompt_base(&self) -> u64 {
        self.abs_base()
    }

    fn prune_prompts(&mut self) {
        let base = self.prompt_base();
        if self.prompts.first().is_some_and(|m| m.line < base) {
            self.prompts.retain(|m| m.line >= base);
        }
    }

    /// record a prompt start (OSC 133 ;A) at the current cursor row; keeps the
    /// list strictly ascending, dropping any later marks an in-place redraw
    /// (e.g. a screen clear) has invalidated
    pub fn mark_prompt(&mut self) {
        let abs = self.total_scrolled + self.cursor.row as u64;
        while self.prompts.last().is_some_and(|m| m.line >= abs) {
            self.prompts.pop();
        }
        self.prompts.push(PromptMark { line: abs, exit: None });
    }

    /// stamp the newest prompt mark with its command's exit code (OSC 133 ;D)
    pub fn set_last_prompt_exit(&mut self, code: Option<i32>) {
        if code.is_some()
            && let Some(m) = self.prompts.last_mut()
        {
            m.exit = code;
        }
    }

    /// retained OSC 133 prompt rows as indices into the complete visible history
    /// (scrollback first, then the live screen) with each command's exit code,
    /// in chronological order
    pub fn prompt_rows(&self) -> impl Iterator<Item = (usize, Option<i32>)> + '_ {
        let base = self.prompt_base();
        let total = self.total_lines();
        self.prompts.iter().filter_map(move |m| {
            m.line
                .checked_sub(base)
                .and_then(|row| (row < total as u64).then_some((row as usize, m.exit)))
        })
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
            self.prompts.iter().map(|m| m.line).find(|&p| p > cur_abs)
        } else {
            self.prompts.iter().map(|m| m.line).rev().find(|&p| p < cur_abs)
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
        self.saved_origin = self.origin_mode;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.origin_mode = self.saved_origin;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
    }

    pub fn put_char(&mut self, c: char) {
        let w = char_width(c);
        // zero-width (combining marks, ZWJ, variation selectors): fold into the
        // previous cell's grapheme cluster instead of dropping
        if w == 0 {
            self.append_combining(c);
            return;
        }
        // a glyph right after a ZWJ continues the previous cell's emoji
        // sequence (family / profession clusters) instead of opening a new cell
        if self.join_zwj_sequence(c) {
            return;
        }
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
            // DECAWM off pins the cursor at the margin: prints overwrite the
            // last column instead of wrapping
            if self.autowrap {
                self.lines[self.cursor.row].wrapped = true;
                self.cursor.col = 0;
                self.linefeed();
            }
        }
        // a double-width glyph that won't fit in the last column wraps first
        if w == 2 && self.cursor.col + 2 > self.cols {
            if self.autowrap {
                self.lines[self.cursor.row].wrapped = true;
                self.cursor.col = 0;
                self.linefeed();
            } else {
                self.cursor.col = self.cols.saturating_sub(2);
            }
        }
        // IRM: shift the rest of the line right so the glyph is inserted, not
        // overwritten (the blanks it opens are filled by the write below)
        if self.insert_mode {
            self.insert_chars(w);
        }
        let row = self.cursor.row;
        let col = self.cursor.col;
        let (fg, bg, attrs, link) = (self.cursor.fg, self.cursor.bg, self.cursor.attrs, self.cursor.link);
        // reconcile a wide pair we're partially overwriting so no orphan lead or
        // continuation cell is left behind to render as a phantom gap
        if col + 1 < self.cols && self.lines[row][col + 1].c == '\0' {
            self.lines[row][col + 1] = Cell { c: ' ', fg, bg, attrs, link, cluster: 0 };
        }
        if col > 0 && self.lines[row][col].c == '\0' {
            self.lines[row][col - 1] = Cell { c: ' ', fg, bg, attrs, link, cluster: 0 };
        }
        self.lines[row][col] = Cell { c, fg, bg, attrs, link, cluster: 0 };
        if w == 2 && col + 1 < self.cols {
            // continuation cell marks the second half of a wide glyph
            self.lines[row][col + 1] = Cell { c: '\0', fg, bg, attrs, link, cluster: 0 };
        }
        if col + w >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.wrap_pending = self.autowrap;
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
        self.tab_forward(1);
    }

    /// CHT / HT: advance to the nth next tab stop, pinned at the last column
    pub fn tab_forward(&mut self, n: usize) {
        let mut col = self.cursor.col;
        for _ in 0..n.max(1) {
            match (col + 1..self.cols).find(|&c| self.tab_stops.get(c).copied().unwrap_or(false)) {
                Some(c) => col = c,
                None => {
                    col = self.cols - 1;
                    break;
                }
            }
        }
        self.cursor.col = col;
        self.cursor.wrap_pending = false;
    }

    /// CBT: move back to the nth previous tab stop, pinned at column 0
    pub fn tab_backward(&mut self, n: usize) {
        let mut col = self.cursor.col;
        for _ in 0..n.max(1) {
            match (0..col).rev().find(|&c| self.tab_stops.get(c).copied().unwrap_or(false)) {
                Some(c) => col = c,
                None => {
                    col = 0;
                    break;
                }
            }
        }
        self.cursor.col = col;
        self.cursor.wrap_pending = false;
    }

    /// DECALN: fill the screen with 'E', reset the margins and origin mode,
    /// and home the cursor — the vttest alignment pattern
    pub fn screen_alignment_test(&mut self) {
        let cell = Cell {
            c: 'E',
            ..Cell::default()
        };
        for line in &mut self.lines {
            line.cells.iter_mut().for_each(|c| *c = cell);
            line.wrapped = false;
        }
        self.region_top = 0;
        self.region_bottom = self.rows - 1;
        self.origin_mode = false;
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    /// HTS: set a tab stop at the cursor column
    pub fn set_tab_stop(&mut self) {
        let col = self.cursor.col;
        if col < self.tab_stops.len() {
            self.tab_stops[col] = true;
        }
    }

    /// TBC: 0 clears the stop at the cursor, 3 clears every stop
    pub fn clear_tab_stops(&mut self, mode: u16) {
        match mode {
            0 => {
                let col = self.cursor.col;
                if col < self.tab_stops.len() {
                    self.tab_stops[col] = false;
                }
            }
            3 => self.tab_stops.iter_mut().for_each(|s| *s = false),
            _ => {}
        }
    }

    /// move down one line, scrolling the region if at the bottom (LF/IND)
    pub fn linefeed(&mut self) {
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

    /// cursor addressing (CUP / HVP / VPA) honoring DECOM: in origin mode the
    /// row is relative to the scroll region top and clamped within it
    pub fn goto_addressed(&mut self, row: usize, col: usize) {
        self.cursor.row = if self.origin_mode {
            (self.region_top + row).min(self.region_bottom)
        } else {
            row.min(self.rows - 1)
        };
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// DECOM toggle; per spec it homes the cursor to the addressable area's top
    pub fn set_origin_mode(&mut self, on: bool) {
        self.origin_mode = on;
        self.cursor.row = if on { self.region_top } else { 0 };
        self.cursor.col = 0;
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
                // a full screen clear also removes any image placements
                self.placements.clear();
                // ED 3 (xterm) additionally erases the saved-lines buffer —
                // `clear`, `printf '\e[3J'`, and shell clear-scrollback rely on it
                if mode == 3 {
                    self.clear_scrollback();
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
            cluster: 0,
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

    fn intern_cluster(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.cluster_index.get(s) {
            return i;
        }
        // cap the table so a long session of distinct combining/ZWJ sequences
        // can't grow it without bound; past the cap a cell falls back to its base
        // char (cluster 0)
        if self.clusters.len() >= 16384 {
            return 0;
        }
        let id = self.clusters.len() as u32;
        self.clusters.push(s.to_string());
        self.cluster_index.insert(s.to_string(), id);
        id
    }

    /// the grapheme string for a cluster id (empty for 0 or out of range)
    pub fn cluster_str(&self, id: u32) -> &str {
        if id == 0 {
            return "";
        }
        self.clusters.get(id as usize).map(String::as_str).unwrap_or("")
    }

    /// the cell the last glyph landed in: the cursor column when a wrap is
    /// pending (e.g. a wide glyph at end-of-line), else the cell just before
    /// the cursor, stepping off a wide glyph's '\0' continuation to its lead
    fn prev_glyph_cell(&self) -> Option<(usize, usize)> {
        let row = self.cursor.row;
        let mut col = if self.cursor.wrap_pending {
            self.cursor.col
        } else if self.cursor.col == 0 {
            return None;
        } else {
            self.cursor.col - 1
        };
        if col > 0 && self.lines.get(row).and_then(|l| l.get(col)).map(|x| x.c) == Some('\0') {
            col -= 1;
        }
        Some((row, col))
    }

    /// attach a zero-width char (combining mark / ZWJ / variation selector) to
    /// the grapheme cluster of the most recently written cell, preserving it for
    /// copy and (Part B) composition. a leading combiner with no base is dropped
    fn append_combining(&mut self, c: char) {
        // never attach to the '\0' continuation of a wide glyph: step to its lead,
        // or the mark renders blank and leaks a NUL into copy / accessibility
        let Some((row, col)) = self.prev_glyph_cell() else {
            return;
        };
        let Some(cell) = self.lines.get(row).and_then(|l| l.get(col)).copied() else {
            return;
        };
        // build the new cluster text in a reused buffer (take it out so it doesn't
        // borrow self while interning) — an already-interned cluster then costs no
        // allocation, only the buffer's reused capacity + an index probe
        let mut s = std::mem::take(&mut self.cluster_scratch);
        s.clear();
        let existing = self.cluster_str(cell.cluster);
        if existing.is_empty() {
            s.push(cell.c);
        } else {
            s.push_str(existing);
        }
        // cap so a flood of combiners can't grow one cluster unbounded
        if s.chars().count() < 16 {
            s.push(c);
            let id = self.intern_cluster(&s);
            self.lines[row][col].cluster = id;
        }
        self.cluster_scratch = s; // hand the buffer back for the next mark
        // VS16 requests emoji presentation: promote the narrow base to a wide
        // cell so the color glyph owns both columns. only when the selector
        // directly follows its base mid-line — at the right margin there is no
        // second column to claim, so the base stays narrow there
        if c == '\u{FE0F}'
            && char_width(cell.c) == 1
            && emoji_vs_base(cell.c)
            && !self.cursor.wrap_pending
            && self.cursor.col == col + 1
        {
            let (fg, bg, attrs, link) = (cell.fg, cell.bg, cell.attrs, cell.link);
            // the column being claimed may hold the lead of a wide pair: blank
            // its orphaned continuation like put_char does on partial overwrite
            if col + 2 < self.cols && self.lines[row][col + 2].c == '\0' {
                self.lines[row][col + 2] = Cell { c: ' ', fg, bg, attrs, link, cluster: 0 };
            }
            self.lines[row][col + 1] = Cell { c: '\0', fg, bg, attrs, link, cluster: 0 };
            if col + 2 >= self.cols {
                self.cursor.col = self.cols - 1;
                self.cursor.wrap_pending = self.autowrap;
            } else {
                self.cursor.col = col + 2;
            }
        }
    }

    /// fold a printable char into the previous cell when that cell's cluster
    /// ends with a ZWJ and its base glyph is wide — the emoji-sequence case
    /// (family, profession, heart couples). narrow bases (e.g. arabic letters,
    /// which also use ZWJ) keep their own cells. false = write a normal cell
    fn join_zwj_sequence(&mut self, c: char) -> bool {
        let Some((row, col)) = self.prev_glyph_cell() else {
            return false;
        };
        let cell = self.lines[row][col];
        if !self.cluster_str(cell.cluster).ends_with('\u{200D}') {
            return false;
        }
        // a VS16-promoted base (the ❤️ in ❤️‍🔥) is narrow by char_width but
        // owns two columns; its continuation cell marks it wide
        let wide = char_width(cell.c) == 2
            || (col + 1 < self.cols && self.lines[row][col + 1].c == '\0');
        if !wide {
            return false;
        }
        let mut s = std::mem::take(&mut self.cluster_scratch);
        s.clear();
        s.push_str(self.cluster_str(cell.cluster));
        let joined = if s.chars().count() < 16 {
            s.push(c);
            match self.intern_cluster(&s) {
                0 => false, // table capped: keep the old cluster, write a cell
                id => {
                    self.lines[row][col].cluster = id;
                    true
                }
            }
        } else {
            false
        };
        self.cluster_scratch = s;
        joined
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

    /// place a kitty image at the cursor, anchored to the current absolute line
    /// so it scrolls with the surrounding text. cols/rows = the client's
    /// requested cell box (kitty c=/r=), 0 = draw at native pixel size
    pub fn place_image(&mut self, image_id: u32, cols: u16, rows: u16, z: i32) {
        self.place_image_at(image_id, self.cursor.row, self.cursor.col, cols, rows, z);
    }

    /// place an image at an explicit screen position (sixel display mode pins
    /// its image to the top-left instead of the cursor)
    pub fn place_image_at(&mut self, image_id: u32, row: usize, col: usize, cols: u16, rows: u16, z: i32) {
        let abs_line = self.total_scrolled + row as u64;
        self.placements.push(Placement {
            image_id,
            abs_line,
            col,
            cols,
            rows,
            z,
        });
        if self.placements.len() > 1024 {
            self.placements.remove(0);
        }
    }

    /// erase the saved-lines buffer, keeping the live screen (ED 3 and the
    /// "clear scrollback" action)
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.view_offset = 0;
        self.prune_prompts();
    }

    /// drop only the placements of a given image (kitty a=d for one image id)
    pub fn remove_placements(&mut self, image_id: u32) {
        self.placements.retain(|p| p.image_id != image_id);
    }

    /// drop every image placement (kitty bare a=d delete-all, no id specified)
    pub fn clear_placements(&mut self) {
        self.placements.clear();
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// the on-screen row of an absolute line as a signed offset (negative =
    /// above the viewport top, >= rows = below the bottom). lets the image
    /// renderer crop a placement that straddles a viewport edge
    pub fn screen_row_signed(&self, abs_line: u64) -> i64 {
        abs_line as i64 - self.total_scrolled as i64 + self.view_offset as i64
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
    fn origin_mode_addresses_relative_to_region() {
        let mut g = Grid::new(10, 5);
        g.set_scroll_region(3, 7);
        g.set_origin_mode(true);
        assert_eq!(g.cursor.row, 3); // homed to the region top
        g.goto_addressed(0, 0);
        assert_eq!(g.cursor.row, 3); // row 1 -> region top
        g.goto_addressed(100, 0);
        assert_eq!(g.cursor.row, 7); // clamped to the bottom margin
        g.set_origin_mode(false);
        g.goto_addressed(0, 0);
        assert_eq!(g.cursor.row, 0); // absolute addressing again
    }

    #[test]
    fn combining_mark_attaches_to_base_cell_cluster() {
        let mut g = Grid::new(2, 8);
        g.put_char('e');
        g.put_char('\u{0301}'); // combining acute accent
        // base cell still shows 'e'; its cluster carries the full grapheme
        assert_eq!(g.lines[0][0].c, 'e');
        assert_eq!(g.cluster_str(g.lines[0][0].cluster), "e\u{0301}");
        // the combiner did not advance into a new cell
        assert_eq!(g.cursor.col, 1);
        // copy returns the whole grapheme, not just the base char
        assert_eq!(g.selected_text((0, 0), (0, 0), false), "e\u{0301}");
        // a leading combiner with no base is dropped (no panic, no cluster)
        let mut g2 = Grid::new(2, 8);
        g2.put_char('\u{0301}');
        assert_eq!(g2.lines[0][0].cluster, 0);
    }

    // an emoji ZWJ sequence occupies one wide cell whose cluster carries the
    // whole sequence; ZWJ between narrow chars (arabic shaping) does not join
    #[test]
    fn zwj_emoji_sequence_joins_into_one_wide_cell() {
        let mut g = Grid::new(2, 12);
        for c in "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".chars() {
            g.put_char(c);
        }
        assert_eq!(g.cursor.col, 2); // one wide cell, not three
        assert_eq!(
            g.cluster_str(g.lines[0][0].cluster),
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
        );
        assert_eq!(g.selected_text((0, 0), (0, 1), false), "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}");
        // narrow base + ZWJ + narrow char: two cells, no join
        let mut g2 = Grid::new(2, 12);
        g2.put_char('\u{0628}'); // arabic beh
        g2.put_char('\u{200D}');
        g2.put_char('\u{0644}'); // arabic lam
        assert_eq!(g2.cursor.col, 2);
        assert_eq!(g2.lines[0][1].c, '\u{0644}');
    }

    // a variation selector (VS16) folds into the base cell's cluster like a
    // mark, and promotes an emoji-variation base to a wide cell
    #[test]
    fn variation_selector_folds_into_cluster() {
        let mut g = Grid::new(2, 8);
        g.put_char('#');
        g.put_char('\u{FE0F}'); // VS16 (emoji presentation)
        assert_eq!(g.lines[0][0].c, '#');
        assert_eq!(g.cluster_str(g.lines[0][0].cluster), "#\u{FE0F}");
        assert_eq!(g.lines[0][1].c, '\0'); // promoted: continuation claimed
        assert_eq!(g.cursor.col, 2);
        // a non-emoji base (mongolian VS one on a letter) stays narrow
        let mut g2 = Grid::new(2, 8);
        g2.put_char('a');
        g2.put_char('\u{FE00}'); // VS1
        assert_eq!(g2.cluster_str(g2.lines[0][0].cluster), "a\u{FE00}");
        assert_eq!(g2.cursor.col, 1);
    }

    // VS16 promotion overwriting the lead of an old wide pair blanks the
    // orphaned continuation, and a base at the right margin stays narrow
    #[test]
    fn vs16_promotion_edges() {
        let mut g = Grid::new(2, 8);
        g.put_char('x');
        g.put_char('世'); // wide pair at cols 1-2
        g.carriage_return();
        g.put_char('\u{2764}'); // heart over the x
        g.put_char('\u{FE0F}'); // promotion claims col 1, 世's lead
        assert_eq!(g.lines[0][1].c, '\0'); // heart's continuation
        assert_eq!(g.lines[0][2].c, ' '); // 世's orphan blanked
        assert_eq!(g.cursor.col, 2);
        // margin: base lands in the last column, nothing to claim
        let mut g2 = Grid::new(2, 4);
        for c in "abc\u{2764}\u{FE0F}".chars() {
            g2.put_char(c);
        }
        assert_eq!(g2.lines[0][3].c, '\u{2764}');
        assert_eq!(g2.cluster_str(g2.lines[0][3].cluster), "\u{2764}\u{FE0F}");
        assert!(g2.cursor.wrap_pending); // still parked at the margin
    }

    // ❤️‍🔥: a VS16-promoted base continues a ZWJ sequence like a natively
    // wide one
    #[test]
    fn vs16_promoted_base_joins_zwj_sequence() {
        let mut g = Grid::new(2, 12);
        for c in "\u{2764}\u{FE0F}\u{200D}\u{1F525}".chars() {
            g.put_char(c);
        }
        assert_eq!(g.cursor.col, 2); // one wide cell
        assert_eq!(
            g.cluster_str(g.lines[0][0].cluster),
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}"
        );
    }

    // interning is capped: past 16384 distinct clusters a cell falls back to its
    // base char (cluster 0), and the O(1) index agrees with the table
    #[test]
    fn intern_cluster_caps_and_indexes() {
        let mut g = Grid::new(2, 8);
        // distinct clusters get distinct rising ids, and a repeat returns the same
        let a = g.intern_cluster("a\u{0301}");
        let b = g.intern_cluster("b\u{0301}");
        assert_ne!(a, b);
        assert_eq!(a, g.intern_cluster("a\u{0301}")); // index hit, same id
        // fill to the cap; further distinct clusters fall back to 0
        for i in 0..16384u32 {
            g.intern_cluster(&format!("z{i}"));
        }
        assert_eq!(g.intern_cluster("definitely-new-after-cap"), 0);
    }

    // direct coverage of char_width, including the ASCII fast path + its boundaries
    #[test]
    fn char_width_classifies_each_class() {
        // printable ASCII fast path -> 1, at both ends of the range
        assert_eq!(char_width(' '), 1); // 0x20
        assert_eq!(char_width('A'), 1);
        assert_eq!(char_width('~'), 1); // 0x7e, last in the fast-path range
        // NUL -> 0
        assert_eq!(char_width('\0'), 0);
        // DEL (0x7f) is outside the fast path but falls through to width 1
        assert_eq!(char_width('\u{7f}'), 1);
        // combining mark / ZWJ / variation selector -> 0
        assert_eq!(char_width('\u{0301}'), 0); // combining acute
        assert_eq!(char_width('\u{200D}'), 0); // ZWJ
        assert_eq!(char_width('\u{FE0F}'), 0); // VS16
        // East Asian wide + emoji -> 2
        assert_eq!(char_width('世'), 2);
        assert_eq!(char_width('\u{1F600}'), 2); // grinning face emoji
        // emoji-presentation-by-default singletons EAW marks W
        assert_eq!(char_width('\u{231A}'), 2); // watch
        assert_eq!(char_width('\u{26A1}'), 2); // high voltage
        assert_eq!(char_width('\u{1F680}'), 2); // rocket
        assert_eq!(char_width('\u{1F7E0}'), 2); // orange circle
        // nearby text-presentation symbols stay narrow
        assert_eq!(char_width('\u{2764}'), 1); // heart (text default)
        assert_eq!(char_width('\u{26A0}'), 1); // warning sign
        // an ordinary Latin-1 letter (outside the ASCII fast path) is width 1
        assert_eq!(char_width('é'), 1);
    }

    #[test]
    fn placements_anchor_remove_and_clear() {
        let mut g = Grid::new(4, 8);
        g.put_char('x'); // cursor advances to col 1
        g.place_image(7, 0, 0, 0);
        let p = g.placements();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].image_id, p[0].abs_line, p[0].col), (7, 0, 1));
        // natural size unless the client asked for a cell box
        assert_eq!((p[0].cols, p[0].rows), (0, 0));
        // on-screen line (no scroll, no view offset): signed row == abs_line
        assert_eq!(g.screen_row_signed(0), 0);
        // id-scoped removal keeps the others
        g.place_image(9, 10, 5, -3);
        assert_eq!((g.placements()[1].cols, g.placements()[1].rows), (10, 5));
        assert_eq!(g.placements()[1].z, -3);
        g.remove_placements(7);
        assert_eq!(g.placements().iter().map(|p| p.image_id).collect::<Vec<_>>(), vec![9]);
        // clear-all empties them
        g.clear_placements();
        assert!(g.placements().is_empty());
    }

    #[test]
    fn reflow_keeps_placements_anchored_to_content() {
        let mut g = Grid::new(4, 8);
        g.set_scrollback_limit(100);
        // a marker line, then the image on the line below it
        for c in "mark".chars() {
            g.put_char(c);
        }
        g.linefeed();
        g.carriage_return();
        g.place_image_at(1, 1, 2, 0, 0, 0);
        assert_eq!(g.placements().len(), 1);
        // width change reflows; the placement survives on its content line
        g.resize(4, 12);
        let pl = g.placements()[0];
        assert_eq!(pl.image_id, 1);
        assert_eq!(pl.col, 2);
        // "mark" still fits one row at the new width, so the image stays on
        // the physical line right after it
        assert_eq!(pl.abs_line, g.abs_base() + 1);
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
    fn word_boundary_moves_within_and_between_runs() {
        let mut g = Grid::new(2, 32);
        for c in "one  two/three !! four".chars() {
            g.put_char(c);
        }
        let row = g.abs_base();
        assert_eq!(g.word_boundary((row, 1), false), (row, 0));
        assert_eq!(g.word_boundary((row, 1), true), (row, 2));
        assert_eq!(g.word_boundary((row, 2), true), (row, 13));
        assert_eq!(g.word_boundary((row, 5), false), (row, 0));
        assert_eq!(g.word_boundary((row, 13), true), (row, 16));
        assert_eq!(g.word_boundary((row, 16), true), (row, 21));
    }

    #[test]
    fn word_boundary_crosses_wraps_but_respects_hard_lines() {
        let mut wrapped = Grid::new(3, 6);
        for c in "alpha beta".chars() {
            wrapped.put_char(c);
        }
        let row = wrapped.abs_base();
        assert_eq!(wrapped.word_boundary((row, 0), true), (row, 4));
        assert_eq!(wrapped.word_boundary((row, 4), true), (row + 1, 3));
        assert_eq!(wrapped.word_boundary((row + 1, 3), false), (row + 1, 0));
        assert_eq!(wrapped.word_boundary((row + 1, 0), false), (row, 0));

        let mut hard = Grid::new(3, 5);
        for c in "abcde".chars() {
            hard.put_char(c);
        }
        hard.linefeed();
        hard.carriage_return();
        for c in "fgh".chars() {
            hard.put_char(c);
        }
        let row = hard.abs_base();
        assert_eq!(hard.word_boundary((row, 0), true), (row, 4));
        assert_eq!(hard.word_boundary((row, 4), true), (row + 1, 2));
        assert_eq!(hard.word_boundary((row + 1, 2), false), (row + 1, 0));
        assert_eq!(hard.word_boundary((row + 1, 0), false), (row, 0));
    }

    #[test]
    fn word_class_keeps_a_wide_cell_with_its_run() {
        let mut g = Grid::new(2, 8);
        for c in "世a".chars() {
            g.put_char(c);
        }
        let row = g.abs_base();
        assert_eq!(g.word_bounds(0, 1), (0, 2));
        assert_eq!(g.word_boundary((row, 0), true), (row, 2));
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
        assert_eq!(g.selected_text((0, 0), (0, 1), false), "aa");
        assert_eq!(g.selected_text((1, 0), (1, 1), false), "bb");
        assert_eq!(g.selected_text((0, 0), (1, 1), false), "aa\nbb");
    }

    #[test]
    fn selection_anchors_to_content_not_the_viewport() {
        let mut g = Grid::new(2, 4);
        for line in ["aa", "bb", "cc", "dd"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // anchor "bb" by absolute id while scrolled to the top
        g.scroll_view(g.scrollback.len() as isize);
        let abs = g.viewport_to_abs(1);
        let sel = ((abs, 0), (abs, 1));
        assert_eq!(g.selected_text(sel.0, sel.1, false), "bb");
        // the same anchors keep meaning "bb" wherever the view goes
        g.scroll_view(-(g.view_offset as isize));
        assert_eq!(g.selected_text(sel.0, sel.1, false), "bb");
        // and new output scrolling past doesn't move them either
        for c in "ee".chars() {
            g.put_char(c);
        }
        g.linefeed();
        assert_eq!(g.selected_text(sel.0, sel.1, false), "bb");
    }

    #[test]
    fn full_span_covers_history_and_skips_trailing_blank_rows() {
        let mut g = Grid::new(2, 4);
        assert_eq!(g.full_span(), None);
        for line in ["aa", "bb", "cc", "dd"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        let (start, end) = g.full_span().expect("printed text has a span");
        assert_eq!(g.selected_text(start, end, false), "aa\nbb\ncc\ndd");
        assert_eq!(g.full_text(), "aa\nbb\ncc\ndd");
    }

    #[test]
    fn selection_spans_more_than_one_screen() {
        let mut g = Grid::new(2, 4);
        g.set_scrollback_limit(100);
        for line in ["aa", "bb", "cc", "dd", "ee"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // five content lines on a 2-row screen: an absolute-anchored range
        // covers all of them at once, which viewport coords never could
        let base = g.abs_base();
        assert_eq!(g.selected_text((base, 0), (base + 4, 1), false), "aa\nbb\ncc\ndd\nee");
    }

    #[test]
    fn selection_skips_evicted_lines_and_round_trips_the_view() {
        let mut g = Grid::new(2, 4);
        g.set_scrollback_limit(2);
        for line in ["aa", "bb", "cc", "dd", "ee"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // a selection whose start line has been evicted copies what remains
        assert_eq!(g.selected_text((0, 0), (g.abs_base() + 1, 1), false), "cc\ndd");
        // abs <-> viewport round-trip while scrolled into history
        g.scroll_view(g.scrollback.len() as isize);
        for r in 0..g.rows {
            assert_eq!(g.abs_to_viewport(g.viewport_to_abs(r)), Some(r));
        }
        // an id below the retained window maps nowhere
        assert_eq!(g.abs_to_viewport(g.abs_base().wrapping_sub(1)), None);
    }

    #[test]
    fn block_selection_copies_a_column_rectangle() {
        let mut g = Grid::new(4, 8);
        for line in ["abcd", "efgh", "ijkl"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // a 2-wide rectangle over the middle columns of three rows
        assert_eq!(g.selected_text((0, 1), (2, 2), true), "bc\nfg\njk");
        // endpoints given in any corner order select the same rectangle
        assert_eq!(g.selected_text((2, 2), (0, 1), true), "bc\nfg\njk");
        assert_eq!(g.selected_text((0, 2), (2, 1), true), "bc\nfg\njk");
        // stream mode over the same endpoints runs through line ends instead
        assert_eq!(g.selected_text((0, 1), (2, 2), false), "bcd\nefgh\nijk");
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
        assert!(g.prompts.iter().all(|m| m.line >= g.prompt_base()));
        assert_eq!(g.prompt_rows().count(), 5);
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

    // exit codes stamp the newest mark, survive reflow, and come back out of
    // prompt_rows in order
    #[test]
    fn prompt_marks_carry_exit_codes() {
        let mut g = Grid::new(3, 10);
        g.set_scrollback_limit(100);
        for (i, code) in [Some(0), Some(1), None].iter().enumerate() {
            g.mark_prompt();
            for ch in format!("prompt-{i}").chars() {
                g.put_char(ch);
            }
            g.set_last_prompt_exit(*code);
            g.carriage_return();
            g.linefeed();
        }
        let exits: Vec<_> = g.prompt_rows().map(|(_, e)| e).collect();
        assert_eq!(exits, vec![Some(0), Some(1), None]);
        // a D with no payload leaves the previous stamp alone
        g.set_last_prompt_exit(None);
        assert_eq!(g.prompt_rows().last().unwrap().1, None);
        // reflow to a narrower width keeps each mark's stamp
        g.resize(3, 7);
        let exits: Vec<_> = g.prompt_rows().map(|(_, e)| e).collect();
        assert_eq!(exits, vec![Some(0), Some(1), None]);
    }

    #[test]
    fn copy_joins_soft_wrapped_lines() {
        let mut g = Grid::new(3, 4);
        // 6 chars into a 4-col grid: "abcd" soft-wraps into "ef"
        for c in "abcdef".chars() {
            g.put_char(c);
        }
        assert!(g.lines[0].wrapped);
        // the soft wrap is the terminal's, not the text's: copy unbroken
        assert_eq!(g.selected_text((0, 0), (1, 3), false), "abcdef");
        // a real newline still breaks
        g.linefeed();
        g.carriage_return();
        for c in "gh".chars() {
            g.put_char(c);
        }
        assert_eq!(g.selected_text((0, 0), (2, 3), false), "abcdef\ngh");
        // block mode keeps one line per row (a column extract)
        assert_eq!(g.selected_text((0, 0), (1, 1), true), "ab\nef");
    }

    #[test]
    fn ed3_clears_scrollback_but_ed2_keeps_it() {
        let mut g = Grid::new(2, 4);
        for line in ["aa", "bb", "cc", "dd"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        assert!(!g.scrollback.is_empty());
        g.erase_in_display(2);
        assert!(!g.scrollback.is_empty(), "ED 2 clears the screen only");
        g.erase_in_display(3);
        assert!(g.scrollback.is_empty(), "ED 3 erases saved lines");
        assert_eq!(g.view_offset, 0);
    }

    #[test]
    fn scrolled_view_stays_anchored_while_output_streams() {
        let mut g = Grid::new(2, 4);
        for line in ["aa", "bb", "cc", "dd"] {
            for c in line.chars() {
                g.put_char(c);
            }
            g.linefeed();
            g.carriage_return();
        }
        // scroll up to read history: the oldest line is on screen
        g.scroll_view(g.scrollback.len() as isize);
        assert_eq!(g.line_at(0)[0].c, 'a');
        let before = g.view_offset;
        // new output must not yank the view off the text being read: the
        // offset rides up with each line entering scrollback
        for c in "ee".chars() {
            g.put_char(c);
        }
        g.linefeed();
        g.carriage_return();
        assert_eq!(g.view_offset, before + 1);
        assert_eq!(g.line_at(0)[0].c, 'a');
        // returning to the live bottom still works
        g.scroll_view(-(g.view_offset as isize));
        assert_eq!(g.view_offset, 0);
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

    #[test]
    fn reflow_preserves_prompt_marks() {
        let mut g = Grid::new(3, 10);
        g.set_scrollback_limit(100);
        for i in 0..5 {
            g.mark_prompt();
            for ch in format!("prompt-{i}").chars() {
                g.put_char(ch);
            }
            g.carriage_return();
            g.linefeed();
        }
        assert_eq!(g.prompts.len(), 5);

        g.resize(3, 7);
        assert_eq!(g.prompts.len(), 5);
        assert!(g.prompts.iter().all(|m| m.line >= g.prompt_base()));
        for mark in &g.prompts {
            let row = (mark.line - g.prompt_base()) as usize;
            let line = if row < g.scrollback.len() {
                &g.scrollback[row]
            } else {
                &g.lines[row - g.scrollback.len()]
            };
            assert_eq!(line.iter().take(7).map(|cell| cell.c).collect::<String>(), "prompt-");
        }

        assert!(g.jump_prompt(false));
        let first_jump = g.view_offset;
        assert!(first_jump > 0);
        assert!(g.jump_prompt(false));
        assert!(g.view_offset > first_jump);

        g.resize(3, 14);
        assert_eq!(g.prompts.len(), 5);
        assert!(g.jump_prompt(false));
    }

    #[test]
    fn reflow_prunes_prompt_marks_evicted_by_limit() {
        let mut g = Grid::new(3, 10);
        g.set_scrollback_limit(4);
        for i in 0..5 {
            g.mark_prompt();
            for ch in format!("prompt-{i}").chars() {
                g.put_char(ch);
            }
            g.carriage_return();
            g.linefeed();
        }

        g.resize(3, 7);
        assert_eq!(g.prompts.len(), 3);
        assert!(g.prompts.iter().all(|m| m.line >= g.prompt_base()));
        for mark in &g.prompts {
            let row = (mark.line - g.prompt_base()) as usize;
            let line = if row < g.scrollback.len() {
                &g.scrollback[row]
            } else {
                &g.lines[row - g.scrollback.len()]
            };
            assert_eq!(line.iter().take(7).map(|cell| cell.c).collect::<String>(), "prompt-");
        }
    }

    #[test]
    fn search_is_case_insensitive_and_skips_empty() {
        let mut g = Grid::new(3, 20);
        for c in "Hello World".chars() {
            g.put_char(c);
        }
        assert!(g.search("").is_empty());
        assert_eq!(g.search("hello"), vec![(0, 0, 5)]);
        assert_eq!(g.search("WORLD"), vec![(0, 6, 5)]);
        assert_eq!(g.search("lo wo"), vec![(0, 3, 5)]);
        assert!(g.search("xyz").is_empty());
    }

    #[test]
    fn search_folds_case_beyond_ascii() {
        let mut g = Grid::new(3, 20);
        for c in "CAFÉ Straße ЛОГ".chars() {
            g.put_char(c);
        }
        assert_eq!(g.search("café"), vec![(0, 0, 4)]);
        assert_eq!(g.search("straße"), vec![(0, 5, 6)]);
        assert_eq!(g.search("лог"), vec![(0, 12, 3)]);
    }

    #[test]
    fn search_hits_scrollback_and_live_screen() {
        let mut g = Grid::new(2, 10);
        for c in "alpha".chars() {
            g.put_char(c);
        }
        g.carriage_return();
        g.linefeed();
        for c in "beta".chars() {
            g.put_char(c);
        }
        g.carriage_return();
        g.linefeed(); // alpha -> scrollback
        for c in "gamma".chars() {
            g.put_char(c);
        }
        // scrollback[0]=alpha, live[0]=beta, live[1]=gamma
        assert_eq!(g.scrollback.len(), 1);
        assert_eq!(g.search("alpha"), vec![(0, 0, 5)]);
        assert_eq!(g.search("beta"), vec![(1, 0, 4)]);
        assert_eq!(g.search("gamma"), vec![(2, 0, 5)]);
        // every named hit is present; full "a" scan covers both letters of alpha
        let a_hits = g.search("a");
        assert!(a_hits.contains(&(0, 0, 1)));
        assert!(a_hits.contains(&(0, 4, 1)));
        assert!(a_hits.contains(&(1, 3, 1)));
        assert!(a_hits.contains(&(2, 1, 1)));
    }

    #[test]
    fn search_spans_soft_wrapped_lines() {
        // 8-col grid: "FINDME!!" fills the first row and wraps the rest
        let mut g = Grid::new(4, 8);
        for c in "xxFINDME!!yy".chars() {
            g.put_char(c);
        }
        // "xxFINDME" on row 0 (wrapped), "!!yy" on row 1 — needle crosses the join
        assert!(g.lines[0].wrapped);
        assert_eq!(g.search("FINDME!!"), vec![(0, 2, 8)]);
        // a hard newline must still break the run
        g.carriage_return();
        g.linefeed();
        for c in "FINDME!!".chars() {
            g.put_char(c);
        }
        // the soft-wrap match on the first logical line, plus the hard-line one
        let hits = g.search("FINDME!!");
        assert!(hits.contains(&(0, 2, 8)));
        assert!(hits.len() >= 2);
    }
}
