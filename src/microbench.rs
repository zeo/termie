//! private microbenchmark harness for the cpu hot paths. compiled out of the
//! shipped binary (feature-gated) so it never adds weight. run with:
//!   cargo run --release --features microbench -- --microbench [name-filter]
//! each bench builds its input once, warms up, then times N calls and prints
//! ns/op (plus MB/s where a byte count is meaningful). gpu/window/pty paths are
//! out of scope — everything here is pure cpu, since GlyphAtlas, Grid, Terminal
//! and ApcScanner all construct without a device

use crate::apc::ApcScanner;
use crate::grid::Grid;
use crate::image::ImageStore;
use crate::render::{FontId, GlyphAtlas, GlyphKey};
use crate::term::Terminal;
use std::hint::black_box;
use std::time::Instant;

pub fn maybe_run() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--microbench") {
        return false;
    }
    // optional substring filter: `--microbench scanner` runs only matching benches
    let filter = args
        .iter()
        .position(|a| a == "--microbench")
        .and_then(|i| args.get(i + 1))
        .filter(|s| !s.starts_with("--"))
        .cloned();

    println!("termie microbench (release={})", !cfg!(debug_assertions));
    println!("{:<42} {:>12} {:>14}", "bench", "ns/op", "throughput");
    println!("{}", "-".repeat(70));

    scanner_benches(&filter);
    parser_benches(&filter);
    grid_benches(&filter);
    atlas_benches(&filter);
    render_benches(&filter);
    image_benches(&filter);
    true
}

/// number of timed trials per bench; we report the FASTEST (min) to filter out
/// scheduling/thermal noise, which is the standard signal for a micro-benchmark
const TRIALS: u32 = 5;

/// time `f` over `iters` calls (after a warmup), best-of-TRIALS, and print ns/op;
/// `bytes` (if set) is the input size per op, reported as MB/s
fn bench(name: &str, filter: &Option<String>, bytes: Option<usize>, iters: u64, mut f: impl FnMut()) {
    if let Some(fl) = filter
        && !name.contains(fl.as_str())
    {
        return;
    }
    for _ in 0..(iters / 8).max(1) {
        f();
    }
    let mut best = std::time::Duration::MAX;
    for _ in 0..TRIALS {
        let t0 = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(t0.elapsed());
    }
    let el = best;
    let ns = el.as_nanos() as f64 / iters as f64;
    let tput = match bytes {
        Some(b) => {
            let mbps = (b as f64 * iters as f64) / el.as_secs_f64() / 1e6;
            format!("{mbps:>10.0} MB/s")
        }
        None => String::new(),
    };
    println!("{name:<42} {ns:>12.1} {tput:>14}");
}

// ---- input builders ----------------------------------------------------------

/// ~n bytes of typical printable shell output with CR/LF, no escape sequences
fn plain_chunk(n: usize) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog 0123456789 ABCDEFGHIJ\r\n";
    let mut v = Vec::with_capacity(n + line.len());
    while v.len() < n {
        v.extend_from_slice(line);
    }
    v
}

/// ~n bytes of CSI/SGR-heavy output (true-color spans, like a syntax-highlit TUI)
fn csi_chunk(n: usize) -> Vec<u8> {
    let line = b"\x1b[38;2;200;120;40mfn \x1b[38;2;120;200;160mmain\x1b[0m() { \x1b[38;2;180;180;90m// note\x1b[0m }\r\n";
    let mut v = Vec::with_capacity(n + line.len());
    while v.len() < n {
        v.extend_from_slice(line);
    }
    v
}

/// a plain chunk with one complete kitty graphics APC embedded in the middle
fn kitty_chunk(payload_bytes: usize) -> Vec<u8> {
    let mut v = plain_chunk(2048);
    v.extend_from_slice(b"\x1b_Ga=T,f=32,s=64,v=64;");
    v.extend(std::iter::repeat_n(b'A', payload_bytes));
    v.extend_from_slice(b"\x1b\\");
    v.extend_from_slice(&plain_chunk(2048));
    v
}

/// combining-mark-dense bytes: base letter + combining acute + dot-below, repeated
fn combining_chunk(n: usize) -> Vec<u8> {
    let cell = "e\u{0301}\u{0323}".as_bytes(); // e + combining acute + dot below
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        v.extend_from_slice(cell);
    }
    v.extend_from_slice(b"\r\n");
    v
}

// ---- benches ------------------------------------------------------------------

fn scanner_benches(filter: &Option<String>) {
    let plain = plain_chunk(64 * 1024);
    let mut s = ApcScanner::default();
    bench("scanner_feed_plain_64k", filter, Some(plain.len()), 4000, || {
        let (pass, kitty) = s.feed(black_box(&plain));
        black_box(pass.len());
        black_box(kitty.len());
    });

    let csi = csi_chunk(64 * 1024);
    let mut s = ApcScanner::default();
    bench("scanner_feed_csi_heavy_64k", filter, Some(csi.len()), 4000, || {
        let (pass, kitty) = s.feed(black_box(&csi));
        black_box(pass.len());
        black_box(kitty.len());
    });

    let kc = kitty_chunk(16 * 1024);
    let mut s = ApcScanner::default();
    bench("scanner_feed_with_kitty_apc", filter, Some(kc.len()), 4000, || {
        let (pass, kitty) = s.feed(black_box(&kc));
        black_box(pass.len());
        black_box(kitty.len());
    });
}

fn parser_benches(filter: &Option<String>) {
    let screen = plain_chunk(50 * 200 * 2);
    let mut term = Terminal::new(50, 200);
    let mut p = vte::Parser::new();
    bench("vte_parse_plain_stream", filter, Some(screen.len()), 3000, || {
        p.advance(&mut term, black_box(&screen));
    });

    let csi = csi_chunk(50 * 200 * 2);
    let mut term = Terminal::new(50, 200);
    let mut p = vte::Parser::new();
    bench("vte_parse_csi_stream", filter, Some(csi.len()), 3000, || {
        p.advance(&mut term, black_box(&csi));
    });

    let comb = combining_chunk(50 * 200 * 4);
    let mut term = Terminal::new(50, 200);
    let mut p = vte::Parser::new();
    bench("vte_parse_combining_stream", filter, Some(comb.len()), 3000, || {
        p.advance(&mut term, black_box(&comb));
    });
}

fn grid_benches(filter: &Option<String>) {
    let text: Vec<char> = plain_chunk(40 * 120)
        .iter()
        .map(|&b| b as char)
        .collect();
    let mut g = Grid::new(40, 120);
    bench("grid_put_char_plain", filter, None, 200, || {
        for &c in &text {
            g.put_char(black_box(c));
        }
    });
}

fn atlas_benches(filter: &Option<String>) {
    let mut atlas = GlyphAtlas::new(16.0, 13.0, 2.0, None, 1.32);
    atlas.prewarm_ascii();
    let keys: Vec<GlyphKey> = (' '..='~')
        .map(|c| GlyphKey { font: FontId::Content, c, bold: false, italic: false })
        .collect();
    let mut i = 0usize;
    bench("atlas_get_ascii_hit", filter, None, 2_000_000, || {
        let k = keys[i % keys.len()];
        i = i.wrapping_add(1);
        black_box(atlas.get(black_box(k)));
    });

    // warm a cluster so this measures the alloc-free cache-hit path (commit 233aeb0)
    let _ = atlas.get_cluster("e\u{0301}", false, false);
    bench("atlas_get_cluster_hit", filter, None, 2_000_000, || {
        black_box(atlas.get_cluster(black_box("e\u{0301}"), false, false));
    });
}

/// a Terminal with EVERY cell filled (worst-case render = rows*cols glyphs, like
/// a full-screen TUI), so the draw_grid benches are apples-to-apples and reflect
/// the heaviest real frame. mode 0 = plain, 1 = per-cell colored, 2 = combining
fn filled_terminal(rows: usize, cols: usize, mode: u8) -> Terminal {
    let mut buf = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            match mode {
                1 => {
                    if c % 8 == 0 {
                        buf.extend_from_slice(format!("\x1b[38;5;{}m", (r + c) % 256).as_bytes());
                    }
                    buf.push(b'A' + ((r + c) % 26) as u8);
                }
                2 => {
                    let base = char::from(b'a' + ((r + c) % 26) as u8);
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(base.encode_utf8(&mut tmp).as_bytes());
                    buf.extend_from_slice("\u{0301}".as_bytes()); // combining acute folds in
                }
                _ => buf.push(b'A' + ((r + c) % 26) as u8),
            }
        }
        // exactly cols chars per row wraps to fill the next row (no blank cells)
    }
    let mut t = Terminal::new(rows, cols);
    vte::Parser::new().advance(&mut t, &buf);
    t
}

fn render_benches(filter: &Option<String>) {
    let mut atlas = GlyphAtlas::new(16.0, 13.0, 2.0, None, 1.32);
    atlas.prewarm_ascii();

    let t_plain = filled_terminal(50, 200, 0);
    let (d, n) = best_render(&mut atlas, &t_plain, 4000);
    report_render("draw_grid_plain_fullscreen", filter, d, 4000, n);

    let t_csi = filled_terminal(50, 200, 1);
    let (d, n) = best_render(&mut atlas, &t_csi, 4000);
    report_render("draw_grid_colored_fullscreen", filter, d, 4000, n);

    let t_comb = filled_terminal(50, 200, 2);
    let (d, n) = best_render(&mut atlas, &t_comb, 4000);
    report_render("draw_grid_combining_fullscreen", filter, d, 4000, n);
}

/// best-of-TRIALS draw_grid timing (filters noise like bench() does)
fn best_render(atlas: &mut GlyphAtlas, term: &Terminal, iters: u64) -> (std::time::Duration, usize) {
    let mut best = std::time::Duration::MAX;
    let mut n = 0;
    for _ in 0..TRIALS {
        let (d, c) = crate::render::Renderer::bench_draw_grid(atlas, term, iters);
        best = best.min(d);
        n = c;
    }
    (best, n)
}

fn report_render(name: &str, filter: &Option<String>, d: std::time::Duration, iters: u64, instances: usize) {
    if let Some(fl) = filter
        && !name.contains(fl.as_str())
    {
        return;
    }
    let ns = d.as_nanos() as f64 / iters as f64;
    println!("{name:<42} {ns:>12.1} {:>10} inst", instances);
}

fn image_benches(filter: &Option<String>) {
    // low priority: images are rare; one number to bound decode cost
    let (w, h) = (256u32, 256u32);
    let data = vec![0x80u8; (w * h * 3) as usize];
    let mut id = 0u32;
    bench("image_decode_rgb_256", filter, Some(data.len()), 2000, || {
        let mut store = ImageStore::default();
        id = id.wrapping_add(1).max(1);
        black_box(store.transmit(id, 24, w, h, false, None, black_box(&data)));
    });
}
