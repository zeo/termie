//! dev-only headless harness: feed escape sequences through the real Terminal
//! and dump the resulting screen + state as text, so changes can be observed
//! without a window. invoked as `termie --termview ...`; compiled out of release

use std::fmt::Write as _;

use vte::Parser;

use crate::color::Color;
use crate::grid::{Attrs, UnderlineStyle};
use crate::term::Terminal;

/// returns true if `--termview` was requested and handled (caller should exit)
pub fn maybe_run() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--termview") {
        return false;
    }
    let val = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let rows = val("--rows").and_then(|v| v.parse().ok()).unwrap_or(10usize).clamp(1, 100);
    let cols = val("--cols").and_then(|v| v.parse().ok()).unwrap_or(40usize).clamp(1, 200);

    let bytes: Vec<u8> = if let Some(name) = val("--scenario") {
        match scenario(&name) {
            Some(b) => b,
            None => {
                println!("unknown scenario: {name}");
                println!("available: {}", SCENARIOS.join(" "));
                return true;
            }
        }
    } else if let Some(path) = val("--file") {
        match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("read error {path}: {e}");
                return true;
            }
        }
    } else if let Some(seq) = val("--seq") {
        unescape(&seq)
    } else {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut buf);
        buf
    };

    let mut term = Terminal::new(rows, cols);
    let mut parser = Parser::new();
    parser.advance(&mut term, &bytes);

    // optionally resize after feeding, to exercise reflow: --resize COLSxROWS
    if let Some(rs) = val("--resize")
        && let Some((c, r)) = rs.split_once('x')
        && let (Ok(c), Ok(r)) = (c.parse::<usize>(), r.parse::<usize>())
    {
        term.resize(r, c);
    }

    if let Some(path) = val("--png") {
        let theme = match val("--theme").as_deref() {
            Some("koi") => crate::color::ThemeId::Koi,
            Some("paper") => crate::color::ThemeId::Paper,
            _ => crate::color::ThemeId::Instrument,
        };
        let pt = val("--pt").and_then(|v| v.parse().ok()).unwrap_or(16.0f32);
        let scale = val("--scale").and_then(|v| v.parse().ok()).unwrap_or(2.0f32);
        // --system-fonts loads installed fonts so CJK/emoji fall back like the app
        let system_fonts = args.iter().any(|a| a == "--system-fonts");
        match crate::render::preview::render_png(&term, theme, pt, scale, system_fonts, &path) {
            Ok((w, h)) => println!("wrote {path} ({w}x{h})"),
            Err(e) => println!("png error: {e}"),
        }
    } else {
        print!("{}", dump(&term, &bytes));
    }
    true
}

const SCENARIOS: &[&str] = &["sgr", "kitty", "altscreen", "osc", "wrap", "cursor", "erase"];

fn scenario(name: &str) -> Option<Vec<u8>> {
    let s: &[u8] = match name {
        "sgr" => b"\x1b[1;31mred-bold\x1b[0m \x1b[4munder\x1b[0m \x1b[7minv\x1b[0m \x1b[38;2;0;200;100mtruecol\x1b[0m",
        "kitty" => b"\x1b[>1u\x1b[?u",
        "altscreen" => b"primary text\x1b[?1049halt screen here",
        "osc" => b"\x1b]0;Hello Title\x1b\\\x1b]7;file:///c:/dev/termie\x1b\\hi there",
        "wrap" => b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "cursor" => b"\x1b[3;5Hx\x1b[1 q",
        "erase" => b"line one\r\nline two\r\nline three\x1b[2;1H\x1b[K",
        _ => return None,
    };
    Some(s.to_vec())
}

/// decode the common backslash escapes in a --seq argument
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'e' => {
                    out.push(0x1b);
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'0' => {
                    out.push(0);
                    i += 2;
                }
                b'a' => {
                    out.push(0x07);
                    i += 2;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'x' if i + 4 <= b.len() => {
                    if let Ok(h) = std::str::from_utf8(&b[i + 2..i + 4])
                        && let Ok(v) = u8::from_str_radix(h, 16)
                    {
                        out.push(v);
                        i += 4;
                    } else {
                        out.push(b[i]);
                        i += 1;
                    }
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn dump(t: &Terminal, fed: &[u8]) -> String {
    let g = &t.grid;
    let mut s = String::new();
    let _ = writeln!(s, "== termview ==");
    let _ = writeln!(s, "fed {} bytes: {:?}", fed.len(), String::from_utf8_lossy(fed));
    let _ = writeln!(
        s,
        "size {}x{}  cursor (r{}, c{}) shape={:?} vis={} wrap_pending={}  alt={}",
        g.rows, g.cols, g.cursor.row, g.cursor.col, g.cursor.shape, g.cursor.visible, g.cursor.wrap_pending, t.using_alt
    );
    let _ = writeln!(
        s,
        "kitty_flags={}  app_cursor={}  bracketed={}  mouse={:?} sgr={}  focus_ev={}  sync={}  bell={}",
        t.kbd_flags(), t.app_cursor_keys, t.bracketed_paste, t.mouse_proto, t.mouse_sgr, t.focus_events, t.sync_output, t.bell
    );
    let _ = writeln!(
        s,
        "title={:?}  cwd={:?}  scrollback={} lines  view_offset={}",
        t.title,
        t.cwd,
        g.scrollback.len(),
        g.view_offset
    );
    if !t.responses.is_empty() {
        let _ = writeln!(s, "responses: {}  ({:?})", hex(&t.responses), String::from_utf8_lossy(&t.responses));
    }
    if let Some(cb) = &t.clipboard {
        let _ = writeln!(s, "clipboard: {cb:?}");
    }
    if !t.color_queries.is_empty() {
        let _ = writeln!(s, "color_queries: {:?}", t.color_queries);
    }

    // column rulers (tens digit every 10, units below)
    let gut = 3usize;
    let pad = " ".repeat(gut + 1);
    let mut tens = pad.clone();
    let mut units = pad.clone();
    for c in 0..g.cols {
        tens.push(if c % 10 == 0 {
            char::from(b'0' + ((c / 10) % 10) as u8)
        } else {
            ' '
        });
        units.push(char::from(b'0' + (c % 10) as u8));
    }
    let _ = writeln!(s, "{tens}");
    let _ = writeln!(s, "{units}");

    let border = format!("{}+{}+", " ".repeat(gut), "-".repeat(g.cols));
    let _ = writeln!(s, "{border}");
    for r in 0..g.rows {
        let mut row = String::new();
        for c in 0..g.cols {
            let ch = g.lines[r][c].c;
            let disp = if ch == '\0' || ch == ' ' {
                ' '
            } else if ch.is_control() {
                '\u{00b7}'
            } else {
                ch
            };
            row.push(disp);
        }
        let mark = if r == g.cursor.row { " <-- cursor row" } else { "" };
        let _ = writeln!(s, "{r:>gut$}|{row}|{mark}");
    }
    let _ = writeln!(s, "{border}");

    // styled runs (cells that differ from the default cell)
    let mut styles = String::new();
    for r in 0..g.rows {
        let line = &g.lines[r];
        let mut c = 0;
        while c < g.cols {
            if is_default(&line[c]) {
                c += 1;
                continue;
            }
            let start = c;
            let key = (line[c].fg, line[c].bg, line[c].attrs);
            while c < g.cols && !is_default(&line[c]) && (line[c].fg, line[c].bg, line[c].attrs) == key {
                c += 1;
            }
            let _ = writeln!(
                styles,
                "  r{r} c{start}-{}: fg={} bg={} attrs={}",
                c - 1,
                col(key.0),
                col(key.1),
                attrs(key.2)
            );
        }
    }
    if styles.is_empty() {
        let _ = writeln!(s, "styles: (all default)");
    } else {
        let _ = writeln!(s, "styles:");
        s.push_str(&styles);
    }

    // OSC 8 hyperlink runs
    let mut links = String::new();
    for r in 0..g.rows {
        let line = &g.lines[r];
        let mut c = 0;
        while c < g.cols {
            let id = line[c].link;
            if id == 0 {
                c += 1;
                continue;
            }
            let start = c;
            while c < g.cols && line[c].link == id {
                c += 1;
            }
            let _ = writeln!(links, "  r{r} c{start}-{}: -> {}", c - 1, g.link_uri(id).unwrap_or(""));
        }
    }
    if !links.is_empty() {
        let _ = writeln!(s, "links:");
        s.push_str(&links);
    }
    s
}

fn is_default(cell: &crate::grid::Cell) -> bool {
    cell.fg == Color::Default && cell.bg == Color::DefaultBg && cell.attrs == Attrs::default()
}

fn col(c: Color) -> String {
    match c {
        Color::Default => "def".into(),
        Color::DefaultBg => "defbg".into(),
        Color::Indexed(n) => format!("idx{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn attrs(a: Attrs) -> String {
    let mut s = String::new();
    if a.bold {
        s.push('b');
    }
    if a.dim {
        s.push('d');
    }
    if a.italic {
        s.push('i');
    }
    match a.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => s.push('u'),
        UnderlineStyle::Double => s.push('2'),
        UnderlineStyle::Curly => s.push('~'),
        UnderlineStyle::Dotted => s.push('.'),
        UnderlineStyle::Dashed => s.push('='),
    }
    if a.inverse {
        s.push('v');
    }
    if a.strike {
        s.push('s');
    }
    if a.hidden {
        s.push('h');
    }
    if a.blink {
        s.push('*');
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

// golden-snapshot suite: render fixed scenarios through the real parser/grid
// and compare the dump to a checked-in text snapshot. this is how a terminal or
// rendering change can be reviewed headlessly — a regression shows up as a diff
// in the snapshot. run `BLESS=1 cargo test golden` to (re)write the snapshots
// after an intended change, then read the diff before committing
#[cfg(test)]
mod golden {
    use super::dump;
    use crate::term::Terminal;
    use vte::Parser;

    struct Case {
        name: &'static str,
        rows: usize,
        cols: usize,
        bytes: &'static [u8],
        // (rows, cols) applied after feeding, to exercise reflow
        resize: Option<(usize, usize)>,
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "sgr_styles",
                rows: 4,
                cols: 40,
                bytes: b"\x1b[1;31mred-bold\x1b[0m \x1b[4munder\x1b[0m \x1b[9mstrike\x1b[0m \x1b[5mblink\x1b[0m \x1b[38;2;0;200;100mtruecol\x1b[0m",
                resize: None,
            },
            // a TUI-style diff: bg-colored added/removed lines filled to the
            // edge with EL, plus a line that wraps — locks the diff-bar rendering
            // that the transposed-pty bug made look broken
            Case {
                name: "diff_bars",
                rows: 8,
                cols: 40,
                bytes: b"\x1b[48;2;30;70;40m+ added line of code\x1b[K\x1b[0m\r\n\x1b[48;2;90;30;30m- removed line here\x1b[K\x1b[0m\r\n\x1b[48;2;30;70;40m+ a very long added line that exceeds the forty column width\x1b[K\x1b[0m\r\nplain context line\r\n",
                resize: None,
            },
            Case {
                name: "soft_wrap",
                rows: 5,
                cols: 20,
                bytes: b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMN",
                resize: None,
            },
            Case {
                name: "reflow_grow",
                rows: 5,
                cols: 20,
                bytes: b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMN",
                resize: Some((5, 40)),
            },
            Case {
                name: "reflow_shrink",
                rows: 5,
                cols: 40,
                bytes: b"012345678901234567890123456789",
                resize: Some((5, 20)),
            },
            // background-color erase: EL/ED must fill with the active bg
            Case {
                name: "erase_bce",
                rows: 4,
                cols: 20,
                bytes: b"\x1b[44mfilled\x1b[K\r\nsecond\x1b[42m\x1b[2K",
                resize: None,
            },
            Case {
                name: "kitty_modes",
                rows: 3,
                cols: 16,
                bytes: b"\x1b[>1u\x1b[?u",
                resize: None,
            },
            Case {
                name: "osc_title_cwd",
                rows: 3,
                cols: 30,
                bytes: b"\x1b]0;My Title\x1b\\\x1b]7;file:///C:/dev/proj\x1b\\hello",
                resize: None,
            },
            Case {
                name: "cursor_cup_el",
                rows: 5,
                cols: 20,
                bytes: b"line1\r\nline2\r\nline3\x1b[2;1H\x1b[Kreplaced",
                resize: None,
            },
            Case {
                name: "underline_variants",
                rows: 3,
                cols: 40,
                bytes: b"\x1b[4:3mcurly\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:4mdotted\x1b[0m",
                resize: None,
            },
            // cell-width correctness: each line is 10 cells of content then a
            // `|` marker, which must land in the same column on every row.
            // ascii (w1), box-drawing (w1), CJK (w2 x5), symbols (w1) + ascii
            Case {
                name: "char_widths",
                rows: 6,
                cols: 16,
                bytes: b"0123456789|\r\n\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80|\r\n\xe4\xbd\xa0\xe5\xa5\xbd\xe4\xb8\x96\xe7\x95\x8c\xe4\xbd\xa0|\r\n\xe2\x97\x8f\xe2\x97\x86\xe2\x96\xa0\xe2\x96\xb2ABCDEF|",
                resize: None,
            },
            // a wide char that won't fit in the last column must wrap whole to
            // the next row, not split: col 9 stays blank and 你 starts row 1
            Case {
                name: "wide_at_edge",
                rows: 4,
                cols: 10,
                bytes: b"123456789\xe4\xbd\xa0ABC",
                resize: None,
            },
            // insert/delete chars: DCH shifts the tail left (blanks in at the
            // right edge), ICH shifts it right (truncating at the edge)
            Case {
                name: "ich_dch",
                rows: 3,
                cols: 14,
                bytes: b"hello world\x1b[1;1H\x1b[6P\x1b[2;1Habcdefg\x1b[2;1H\x1b[3@",
                resize: None,
            },
            // scroll region (DECSTBM): rows 2-5 scroll, rows 1 and 6 are frozen.
            // a linefeed at the bottom of the region scrolls only the region
            Case {
                name: "scroll_region",
                rows: 6,
                cols: 8,
                bytes: b"r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6\x1b[2;5r\x1b[5;1H\nNEW",
                resize: None,
            },
        ]
    }

    fn render(c: &Case) -> String {
        let mut term = Terminal::new(c.rows, c.cols);
        let mut parser = Parser::new();
        parser.advance(&mut term, c.bytes);
        if let Some((rows, cols)) = c.resize {
            term.resize(rows, cols);
        }
        dump(&term, c.bytes)
    }

    fn golden_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
    }

    #[test]
    fn golden_snapshots() {
        let dir = golden_dir();
        let bless = std::env::var_os("BLESS").is_some();
        if bless {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut mismatches = Vec::new();
        for c in cases() {
            let got = render(&c);
            let path = dir.join(format!("{}.txt", c.name));
            if bless {
                std::fs::write(&path, got).unwrap();
                continue;
            }
            let want = std::fs::read_to_string(&path).unwrap_or_default();
            // normalize line endings: the dump is LF, but an autocrlf checkout
            // could give the snapshot CRLF, which must not read as a mismatch
            if want.replace("\r\n", "\n") != got {
                mismatches.push((c.name, want, got));
            }
        }
        if bless {
            return;
        }
        for (name, want, got) in &mismatches {
            eprintln!("\n=== golden mismatch: {name} ===");
            let (wl, gl): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
            for i in 0..wl.len().max(gl.len()) {
                let (w, g) = (wl.get(i).copied().unwrap_or(""), gl.get(i).copied().unwrap_or(""));
                if w != g {
                    eprintln!("  -{w}");
                    eprintln!("  +{g}");
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} golden snapshot mismatch(es); re-run with BLESS=1 after verifying the change is intended",
            mismatches.len()
        );
    }
}
