//! dev-only headless UI capture: render the full app chrome (tab strip,
//! title-bar buttons, panes, status bar) to a PNG through an offscreen
//! renderer, so chrome and layout changes can be seen without opening a
//! window. invoked as `termie --uiview [--scene NAME] [--png PATH]`; compiled
//! out of release

use vte::Parser;

use crate::render::{self, Hot, PaneView};
use crate::term::Terminal;

/// returns true if `--uiview` was requested and handled (caller should exit)
pub fn maybe_run() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--uiview") {
        return false;
    }
    let val = |flag: &str| -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    };
    let scene = val("--scene").unwrap_or_else(|| "split".to_string());
    let out = val("--png").unwrap_or_else(|| "uiview.png".to_string());
    let (w, h) = (1100u32, 680u32);
    let scale = 2.0f32;

    let mut r = render::Renderer::new_headless(w, h, 14.0, 12.5, scale);
    r.set_tabs(vec!["backend".into(), "web-ui".into(), "infra".into()], 0);

    // chrome state varies by scene so each capture isolates one feature
    match scene.as_str() {
        "hover" => {
            r.set_hovered(Some(Hot::SplitV));
        }
        "gear" => {
            r.set_hovered(Some(Hot::Gear));
        }
        _ => {}
    }

    let pad = 8.0f32;
    let tb = r.title_bar_h;
    let sb = r.status_bar_h;
    let content_h = h as f32 - tb - sb - pad * 2.0;

    // build the panes for the requested layout, sizing each terminal's grid to
    // its rect so the sample content fills it like the real app
    let single = scene == "single";
    let rects: Vec<(f32, f32, f32, f32)> = if single {
        vec![(pad, tb + pad, w as f32 - pad * 2.0, content_h)]
    } else {
        let cw = (w as f32 - pad * 3.0) / 2.0;
        vec![
            (pad, tb + pad, cw, content_h),
            (pad * 2.0 + cw, tb + pad, cw, content_h),
        ]
    };

    let samples: [&[u8]; 2] = [
        b"\x1b[1;32m$\x1b[0m cargo build --release\r\n   \x1b[2mCompiling\x1b[0m termie v0.1.0\r\n\x1b[33mwarning\x1b[0m: unused variable\r\n    \x1b[1;34m-->\x1b[0m src/main.rs:42\r\n\x1b[1;32m    Finished\x1b[0m in 21.8s\r\n",
        b"\x1b[36m>\x1b[0m a TUI\r\n\x1b[2mbuilding...\x1b[0m\r\n\r\nbuild finished.\r\n\x1b[32m+ added\x1b[0m  \x1b[31m- removed\x1b[0m\r\n",
    ];

    let mut terms: Vec<Terminal> = Vec::new();
    for (i, rect) in rects.iter().enumerate() {
        let (_, _, cols, rows) = r.pane_metrics(*rect);
        let mut t = Terminal::new(rows.max(1), cols.max(1));
        let mut p = Parser::new();
        p.advance(&mut t, samples[i % samples.len()]);
        terms.push(t);
    }

    let panes: Vec<PaneView> = terms
        .iter()
        .zip(&rects)
        .enumerate()
        .map(|(i, (term, rect))| PaneView {
            term,
            rect: *rect,
            focused: i == 0,
            sel: None,
            flash: 0.0,
            link: None,
        })
        .collect();

    match r.render_png(&panes, true, false, &out) {
        Ok(()) => println!("wrote {out} (scene {scene}, {w}x{h})"),
        Err(e) => println!("uiview error: {e}"),
    }
    true
}
