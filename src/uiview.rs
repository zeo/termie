//! dev-only headless UI capture: render the full app chrome (tab strip,
//! title-bar buttons, panes, status bar) to a PNG through an offscreen
//! renderer, so chrome and layout changes can be seen without opening a
//! window. invoked as `termie --uiview [--scene NAME] [--png PATH]`; compiled
//! out of release

use vte::Parser;

use crate::render::{
    self, ConfirmView, FindView, Hot, MarketRowView, MarketView, PaletteView, PaneMenuView, PaneView, RenameView,
};
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
    // overridable so responsive layouts (narrow / wide / hidpi) can be captured
    let w = val("--w").and_then(|v| v.parse().ok()).unwrap_or(1100u32).clamp(320, 4000);
    let h = val("--h").and_then(|v| v.parse().ok()).unwrap_or(680u32).clamp(240, 3000);
    let scale = val("--scale").and_then(|v| v.parse().ok()).unwrap_or(2.0f32).clamp(1.0, 3.0);

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
        "panemode" => {
            r.set_pane_mode(true);
            r.set_hovered(Some(Hot::PaneMode));
        }
        "menu" => {
            r.set_pane_menu(Some(PaneMenuView { x: 90.0, y: 150.0, hovered: Some(0) }));
            r.settle_overlay();
        }
        "reveal" => {
            // restart the power-on clock so the capture lands mid-animation
            r.begin_reveal();
        }
        "settings" => {
            r.set_settings_panel(true, 1.0);
            r.set_plugins(vec![
                ("tamagotchi".into(), true),
                ("relay".into(), false),
                ("css-loader".into(), true),
            ]);
        }
        "palette" => {
            r.set_palette(Some(PaletteView {
                query: "spl".into(),
                items: vec![
                    "split vertical".into(),
                    "split horizontal".into(),
                    "toggle pane mode".into(),
                    "close pane".into(),
                    "new tab".into(),
                    "toggle broadcast".into(),
                ],
                selected: 1,
            }));
            r.settle_overlay();
        }
        "find" => {
            r.set_find(Some(FindView { query: "parser".into(), count: 3, current: 1, matches: vec![] }));
            r.settle_overlay();
        }
        "confirm" => {
            r.set_confirm(Some(ConfirmView {
                prompt: "close tab with 2 panes?".into(),
                hint: "enter confirm   esc cancel".into(),
            }));
            r.settle_overlay();
        }
        "rename" => {
            r.set_rename(Some(RenameView { buf: "backend".into() }));
            r.settle_overlay();
        }
        "market" => {
            r.set_market(Some(MarketView {
                rows: vec![
                    MarketRowView { name: "tamagotchi".into(), version: "1.2".into(), description: "a desktop pet that reacts to your terminal".into(), permissions: vec![], installed: true, enabled: true },
                    MarketRowView { name: "git-status".into(), version: "1.0".into(), description: "live branch + dirty-state widget in the dock".into(), permissions: vec!["read_output".into()], installed: true, enabled: false },
                    MarketRowView { name: "relay".into(), version: "0.4".into(), description: "bridge sessions over a local socket".into(), permissions: vec!["write_pty".into()], installed: false, enabled: false },
                    MarketRowView { name: "css-loader".into(), version: "2.0".into(), description: "hot-reload stylesheets while you edit".into(), permissions: vec![], installed: false, enabled: false },
                ],
                selected: 2,
                status: String::new(),
                loading: false,
                fetch_failed: false,
            }));
            r.settle_overlay();
        }
        "settings2" => {
            // settings scrolled down to the APPEARANCE section (font/pad/opacity/theme)
            r.set_settings_panel(true, 1.0);
            r.set_plugins(vec![("tamagotchi".into(), true), ("relay".into(), false)]);
            r.scroll_settings(400.0);
        }
        _ => {}
    }

    let pad = 8.0f32;
    let tb = r.title_bar_h;
    let sb = r.status_bar_h;
    let content_h = h as f32 - tb - sb - pad * 2.0;

    // build the panes for the requested layout, sizing each terminal's grid to
    // its rect so the sample content fills it like the real app
    // satellite = a torn-off pane in its own bare window (no chrome)
    let bare = scene == "satellite";
    let single = bare || scene == "single";
    let rects: Vec<(f32, f32, f32, f32)> = if bare {
        vec![(pad, pad, w as f32 - pad * 2.0, h as f32 - pad * 2.0)]
    } else if single {
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
        b"\x1b[36m>\x1b[0m git diff\r\n\x1b[2mdiff --git a/src/main.rs\x1b[0m\r\n\r\n\x1b[32m+ added line\x1b[0m\r\n\x1b[31m- removed line\x1b[0m\r\n",
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

    match r.render_png(&panes, true, false, bare, &out) {
        Ok(()) => println!("wrote {out} (scene {scene}, {w}x{h})"),
        Err(e) => println!("uiview error: {e}"),
    }
    true
}
