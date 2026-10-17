//! dev-only headless UI capture: render the full app chrome (tab strip,
//! title-bar buttons, panes, status bar) to a PNG through an offscreen
//! renderer, so chrome and layout changes can be seen without opening a
//! window. invoked as `termie --uiview [--scene NAME] [--png PATH]`; compiled
//! out of release

use vte::Parser;

use crate::render::{
    self, ConfirmView, FindView, Hot, MarketRowView, MarketView, PaletteView, PaneDropSide,
    PaneMenuView, PaneView, RenameView,
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

    let mut r = render::Renderer::new_headless(w, h, 14.0, render::tokens::CHROME_PT, scale);
    let written = capture(&mut r, &scene, w, h, &val)
        .and_then(|rgba| render::preview::write_png(&out, w, h, &rgba));
    match written {
        Ok(()) => println!("wrote {out} (scene {scene}, {w}x{h})"),
        Err(e) => println!("uiview error: {e}"),
    }
    true
}

/// stage one named scene on a headless renderer and read the frame back as
/// rgba; `opt` answers the scene's optional flags (--theme, --query, ...)
pub fn capture(
    r: &mut render::Renderer,
    scene: &str,
    w: u32,
    h: u32,
    opt: &dyn Fn(&str) -> Option<String>,
) -> std::io::Result<Vec<u8>> {
    let val = opt;
    let scene = scene.to_string();
    r.settle_reveal();
    // a blinking cursor would make the frame depend on how long staging took
    r.set_cursor_blink(false);
    if let Some(theme) = val("--theme") {
        r.set_theme(crate::color::ThemeId::from_name(&theme));
    }
    r.set_tabs(vec!["backend".into(), "web-ui".into(), "infra".into()], 0);
    r.set_status(render::StatusView {
        cwd: Some("~/src/termie".into()),
        command: render::CommandState::Finished { code: Some(0), took: std::time::Duration::from_millis(18_400) },
    });

    // chrome state varies by scene so each capture isolates one feature
    match scene.as_str() {
        "hover" => {
            r.set_hovered(Some(Hot::SplitV));
        }
        "attention" => {
            // a bell rang in one background tab; the other finished a command
            // (one ok, shown green) — the full badge spread in one shot
            r.set_tab_status(vec![0, 3, 2]);
        }
        "notice" => {
            // an OSC 9 notification's text on the status bar
            r.set_tab_status(vec![0, 3, 0]);
            r.set_notice(Some("deploy: waiting for approval in web-ui".into()));
        }
        "link" => {
            r.set_link_preview(Some(
                "https://example.com/releases/termie?channel=stable#checksums",
            ));
        }
        "gear" => {
            r.set_hovered(Some(Hot::Gear));
        }
        "tab-hint" => {
            r.set_hovered(Some(Hot::Tab(0)));
        }
        "window-hint" => {
            r.set_hovered(Some(Hot::WindowTabs));
        }
        "panemode" => {
            r.set_pane_mode(true);
            r.set_hovered(Some(Hot::PaneMode));
        }
        "tab-drop" => {
            r.set_tab_drop(Some(1));
        }
        "tab-drag" => {
            r.set_tab_drop(Some(1));
            r.set_drag_preview(Some((520.0, 110.0, "backend".into(), "TAB")));
        }
        "window-drag" => {
            r.set_tab_drop(Some(3));
            r.set_drag_preview(Some((640.0, 110.0, "3 live tabs".into(), "WINDOW")));
        }
        "pane-tab-drop" => {
            r.set_pane_mode(true);
            r.set_tab_drop(Some(1));
        }
        "menu" => {
            r.set_pane_menu(Some(PaneMenuView {
                x: 90.0,
                y: 150.0,
                hovered: Some(0),
                items: render::PANE_MENU_ITEMS.iter().map(|s| s.to_string()).collect(),
                accents: Vec::new(),
            }));
            r.settle_overlay();
        }
        "readme" => {
            // a background tab just finished (its rail mid-hold), another
            // still running
            r.set_tab_status(vec![0, 0, 1]);
            r.set_tab_status(vec![0, 2, 1]);
            r.tab_signal_at(1, 1.2);
        }
        "welcome" => r.welcome_at(1.0),
        "welcome-wink" => r.welcome_at(0.68),
        "welcome-hints" => r.welcome_at(3.0),
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
            // a slice of the real command list with its default chords,
            // filtered and ranked by the app's own matcher
            let commands = [
                ("new tab", "Ctrl+T"),
                ("new window", "Ctrl+Shift+N"),
                ("duplicate tab", "Ctrl+Shift+D"),
                ("split vertical", "Ctrl+Shift+E"),
                ("split horizontal", "Ctrl+Shift+O"),
                ("close pane", "Ctrl+Shift+W"),
                ("find in scrollback", "Ctrl+Shift+F"),
                ("toggle broadcast", "Ctrl+Shift+B"),
                ("pane mode", "Ctrl+Shift+P"),
                ("mark mode", "Ctrl+Shift+M"),
                ("select all", "Ctrl+Shift+A"),
                ("settings", "Ctrl+,"),
            ];
            let query = val("--query").unwrap_or_else(|| "spl".into());
            let mut ranked: Vec<(i32, &str, &str)> = commands
                .iter()
                .filter_map(|&(label, keys)| crate::fuzzy_score(&query, label).map(|s| (s, label, keys)))
                .collect();
            if !query.trim().is_empty() {
                ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            }
            r.set_palette(Some(PaletteView {
                query,
                items: ranked.iter().map(|(_, label, _)| label.to_string()).collect(),
                hints: ranked.iter().map(|(_, _, keys)| keys.to_string()).collect(),
                selected: val("--selected").and_then(|v| v.parse().ok()).unwrap_or(0),
                scope: "commands",
            }));
            match val("--overlay-at").and_then(|v| v.parse().ok()) {
                Some(t) => r.overlay_at(t),
                None => r.settle_overlay(),
            }
        }
        "find" => {
            r.set_find(Some(FindView {
                query: "parser".into(),
                count: 3,
                current: 1,
                capped: false,
                matches: vec![],
                regex_on: false,
                bad: false,
            }));
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
        "preedit" => {
            // the composition is cjk; headless captures skip the lazy system
            // font scan the windowed app does, so tofu without this
            r.ensure_system_fonts();
        }
        _ => {}
    }

    let pad = 8.0f32;
    let tb = r.title_bar_h;
    let sb = r.status_bar_h;
    let content_h = h as f32 - tb - sb - pad * 2.0;

    // build the panes for the requested layout, sizing each terminal's grid to
    // its rect so the sample content fills it like the real app
    let single = scene == "satellite" || scene == "single" || scene.starts_with("welcome");
    let rects: Vec<(f32, f32, f32, f32)> = if single {
        vec![(pad, tb + pad, w as f32 - pad * 2.0, content_h)]
    } else {
        let cw = (w as f32 - pad * 3.0) / 2.0;
        vec![
            (pad, tb + pad, cw, content_h),
            (pad * 2.0 + cw, tb + pad, cw, content_h),
        ]
    };
    if scene == "pane-drop" {
        r.set_pane_mode(true);
        r.set_pane_drop(Some((rects[1], PaneDropSide::Left)));
        r.set_drag_preview(Some((w as f32 * 0.7, h as f32 * 0.45, "backend".into(), "PANE")));
    }

    let samples: [&[u8]; 2] = [
        b"\x1b[1;32m$\x1b[0m cargo build --release\r\n\x1b[2m   Compiling\x1b[0m termie v0.2.0\r\n\x1b[1;32m    Finished\x1b[0m `release` profile in 18.4s\r\n\x1b[1;32m$\x1b[0m cargo test\r\n\x1b[1;32m    Running\x1b[0m unittests src/main.rs\r\n\r\nrunning 165 tests\r\n\x1b[32m........................................\x1b[0m\r\n\x1b[32m..................\x1b[0m\x1b[33miiiii\x1b[0m\x1b[32m............\x1b[0m\r\n\r\ntest result: \x1b[1;32mok\x1b[0m. 160 passed; 0 failed; 5 ignored\r\n\r\n\x1b[1;32m$\x1b[0m cargo clippy --all-targets\r\n\x1b[1;32m    Finished\x1b[0m `dev` profile in 9.8s, no warnings\r\n\x1b[1;32m$\x1b[0m ",
        b"\x1b[36m>\x1b[0m git status -sb\r\n\x1b[32m## main...origin/main\x1b[0m\r\nnothing to commit, working tree clean\r\n\x1b[36m>\x1b[0m git log --oneline -5\r\n\x1b[33m0e5a185\x1b[0m fix: clip the browse button\r\n\x1b[33mf64cfcf\x1b[0m fix: ease the settings panel\r\n\x1b[33m083f635\x1b[0m fix: run the marketplace windowless\r\n\x1b[33m21a1abf\x1b[0m docs: rewrite the readme\r\n\x1b[33mb782251\x1b[0m release: v0.2.0\r\n\x1b[36m>\x1b[0m exa --tree --level=1 src\r\n\x1b[34msrc\x1b[0m\r\n\xe2\x94\x9c\xe2\x94\x80\xe2\x94\x80 main.rs  grid.rs  term.rs  pty.rs\r\n\xe2\x94\x9c\xe2\x94\x80\xe2\x94\x80 \x1b[34mrender/\x1b[0m  \x1b[34mplugin/\x1b[0m\r\n\xe2\x94\x94\xe2\x94\x80\xe2\x94\x80 input.rs  color.rs  win.rs\r\n\x1b[36m>\x1b[0m ",
    ];

    let mut terms: Vec<Terminal> = Vec::new();
    for (i, rect) in rects.iter().enumerate() {
        let (_, _, cols, rows) = r.pane_metrics(*rect);
        let mut t = Terminal::new(rows.max(1), cols.max(1));
        let mut p = Parser::new();
        // a first launch shows a fresh prompt, not a session's worth of output
        let sample: &[u8] = if scene.starts_with("welcome") { b"\x1b[1;32m~\x1b[0m \x1b[36m>\x1b[0m " } else { samples[i % samples.len()] };
        p.advance(&mut t, sample);
        if scene == "preedit" && i == 0 {
            // park the cursor mid-line so the composition draws from a real
            // prompt position, not col 0
            p.advance(&mut t, b"\x1b[1;32m$\x1b[0m echo ");
        }
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
            theme: None,
            // the attention scene also shows a pane's running badge
            status: if scene == "attention" && i == 1 { 1 } else { 0 },
            // a mid-conversion japanese composition: converted kanji head,
            // active clause (thick line) on the trailing kana
            preedit: (scene == "preedit" && i == 0).then_some(render::PreeditView {
                text: "日本語にゅうりょく",
                caret: Some((9, 27)),
            }),
        })
        .collect();

    r.settle_hover();
    r.render_rgba(&panes, true, false, false)
}

#[cfg(test)]
mod tests {
    use super::capture;
    use crate::color::ThemeId;

    /// every chrome surface worth guarding, each rendered in every theme
    const SCENES: [&str; 15] = [
        "split", "attention", "hover", "palette", "menu", "find", "confirm", "rename", "market",
        "settings", "settings2", "panemode", "notice", "welcome-hints", "readme",
    ];
    const W: u32 = 1100;
    const H: u32 = 680;
    /// goldens are 8x8 block averages of luminance: small enough to keep in the
    /// repo, coarse enough that gpu and font-hinting differences wash out, and
    /// still sharp enough that any moved, resized, or recolored element shows
    const BLOCK: u32 = 8;

    fn thumb(rgba: &[u8]) -> Vec<u8> {
        let (tw, th) = (W / BLOCK, H / BLOCK);
        let mut out = Vec::with_capacity((tw * th) as usize);
        for by in 0..th {
            for bx in 0..tw {
                let mut sum = 0u32;
                for y in by * BLOCK..(by + 1) * BLOCK {
                    for x in bx * BLOCK..(bx + 1) * BLOCK {
                        let i = ((y * W + x) * 4) as usize;
                        sum += (rgba[i] as u32 * 54 + rgba[i + 1] as u32 * 183 + rgba[i + 2] as u32 * 19) >> 8;
                    }
                }
                out.push((sum / (BLOCK * BLOCK)) as u8);
            }
        }
        out
    }

    fn golden_path(scene: &str, theme: ThemeId) -> std::path::PathBuf {
        let name = format!("{scene}-{}.png", theme.name().replace(' ', "-"));
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden-ui").join(name)
    }

    fn write_gray(path: &std::path::Path, px: &[u8]) {
        let file = std::fs::File::create(path).expect("create golden");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W / BLOCK, H / BLOCK);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().and_then(|mut w| w.write_image_data(px)).expect("write golden");
    }

    #[test]
    fn chrome_matches_golden_snapshots() {
        let bless = std::env::var_os("TERMIE_BLESS_UI").is_some();
        let mut failures = Vec::new();
        for theme in ThemeId::ALL {
            for scene in SCENES {
                let Some((mut r, _gpu)) = crate::render::headless_for_tests(W, H, 1.0) else {
                    return;
                };
                let theme_name = theme.name().to_string();
                let opt = |flag: &str| (flag == "--theme").then(|| theme_name.clone());
                let rgba = capture(&mut r, scene, W, H, &opt).expect("capture");
                let got = thumb(&rgba);
                let path = golden_path(scene, theme);
                if bless {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    write_gray(&path, &got);
                    continue;
                }
                let Some(want) = std::fs::read(&path).ok().and_then(|b| crate::image::decode_png(&b)) else {
                    failures.push(format!("{}: missing golden (run with TERMIE_BLESS_UI=1)", path.display()));
                    continue;
                };
                let want: Vec<u8> = want.rgba.chunks(4).map(|p| p[0]).collect();
                let diffs: Vec<u32> = got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b) as u32).collect();
                let mean = diffs.iter().sum::<u32>() as f32 / diffs.len() as f32;
                let off = diffs.iter().filter(|&&d| d > 6).count();
                // a cursor cell or a breathing dot may flicker a few blocks; a
                // shifted label or resized surface moves dozens
                if want.len() != got.len() || mean > 1.0 || off > 4 {
                    failures.push(format!("{scene} in {theme_name}: mean diff {mean:.2}, {off} blocks off"));
                }
            }
        }
        assert!(failures.is_empty(), "chrome drifted from its goldens:\n{}", failures.join("\n"));
    }
}
