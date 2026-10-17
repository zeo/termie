//! the installer's face: one frameless dark window in termie's instrument
//! style, painted entirely with GDI (no common controls, no theming, no GPU —
//! an installer must come up on any machine). the same paint function drives
//! the live window and the headless --preview PNG harness

use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetTextExtentPoint32W,
    InvalidateRect, SelectObject, SetBkMode, SetPixelV, SetTextCharacterExtra, SetTextColor,
    TextOutW, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, DIB_RGB_COLORS, FW_BOLD, FW_NORMAL, HBITMAP,
    HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, LoadCursorW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetTimer, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HTCAPTION,
    HTCLIENT, IDC_ARROW, KillTimer, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
    WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSW, WS_POPUP, WS_VISIBLE,
};

use crate::engine::{self, Options};
use crate::payload;

// the app's instrument ladder, shared by path so the installer can't drift
// from the theme it introduces; not every shade is used here
#[allow(dead_code)]
#[path = "../../src/instrument.rs"]
mod instrument;

// GDI COLORREF is 0x00BBGGRR
const INK0: COLORREF = cref(instrument::INK0);
const BG: COLORREF = cref(instrument::BG);
const SURFACE: COLORREF = cref(instrument::INK3);
const RULE: COLORREF = cref(instrument::RULE);
const RULE2: COLORREF = cref(instrument::RULE2);
const TEXT: COLORREF = cref(instrument::FG);
const TEXT2: COLORREF = cref(instrument::TEXT2);
const PAPER: COLORREF = cref(instrument::PAPER);
const CLOSE_RED: COLORREF = cref(instrument::CLOSE_RED);
// the app lifts muted text to 4.5:1 on its surface; this is that lift on BG
const MUTE: COLORREF = rgb(0x7e, 0x7e, 0x7e);
const PAPER_DIM: COLORREF = rgb(0xdc, 0xdc, 0xdc);
const WHITE: COLORREF = rgb(0xff, 0xff, 0xff);

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | r as u32)
}

const fn cref(c: [u8; 3]) -> COLORREF {
    rgb(c[0], c[1], c[2])
}

fn channels(c: COLORREF) -> [f32; 3] {
    [(c.0 & 0xff) as f32, ((c.0 >> 8) & 0xff) as f32, ((c.0 >> 16) & 0xff) as f32]
}

fn mix(under: COLORREF, over: COLORREF, a: f32) -> COLORREF {
    let (u, o) = (channels(under), channels(over));
    let m = |i: usize| (u[i] + (o[i] - u[i]) * a).round() as u8;
    rgb(m(0), m(1), m(2))
}

/// the face every label uses: the app's own bundled font, registered privately
/// from the payload this installer carries; Consolas when the payload is empty
/// (dev builds) or registration fails
fn face() -> PCWSTR {
    static FACE: OnceLock<Vec<u16>> = OnceLock::new();
    let name = FACE.get_or_init(|| {
        let mut loaded = false;
        for e in payload::entries() {
            if !(e.name.ends_with("MapleMono-NF-Regular.ttf") || e.name.ends_with("MapleMono-NF-Bold.ttf")) {
                continue;
            }
            if let Ok(bytes) = e.decompress() {
                let mut fonts = 0u32;
                // the font is copied into gdi's private table; the buffer can drop
                // gdi writes the face count through this pointer
                let h = unsafe { AddFontMemResourceEx(bytes.as_ptr().cast(), bytes.len() as u32, None, (&raw mut fonts).cast_const()) };
                loaded |= !h.0.is_null() && fonts > 0;
            }
        }
        let family = if loaded { "Maple Mono NF" } else { "Consolas" };
        family.encode_utf16().chain(std::iter::once(0)).collect()
    });
    PCWSTR(name.as_ptr())
}

pub const WIN_W: i32 = 560;
pub const WIN_H: i32 = 396;

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Options,
    Progress,
    Done,
    Error,
    RemoveConfirm,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Close,
    Install,
    TogglePath,
    ToggleStart,
    ToggleDesktop,
    ToggleCtx,
    Launch,
    Quit,
    Remove,
    Cancel,
}

pub struct State {
    pub page: Page,
    pub opts: Options,
    pub progress: f32,
    pub detail: String,
    pub error: String,
    pub old_msi: bool,
    /// /update runs headfirst into Progress and auto-launches at the end
    pub auto_update: bool,
    hover: Option<Hit>,
}

impl State {
    pub fn new(page: Page) -> Self {
        State {
            page,
            opts: Options::default(),
            progress: 0.0,
            detail: String::new(),
            error: String::new(),
            old_msi: false,
            auto_update: false,
            hover: None,
        }
    }
}

/// engine → ui progress mailbox; the worker locks it, the paint reads it
pub type Shared = Arc<Mutex<State>>;

struct Ctx {
    state: Shared,
    font_cache: Mutex<Vec<(i32, i32, isize)>>, // (px, weight) -> HFONT as isize
}

// one window per process: the wndproc reaches state through a global
static CTX: Mutex<Option<Arc<Ctx>>> = Mutex::new(None);

fn hit_rects(dpi: f32, s: &State) -> Vec<(Hit, RECT)> {
    let px = |v: i32| (v as f32 * dpi) as i32;
    let mut out = Vec::new();
    out.push((Hit::Close, RECT { left: px(WIN_W - 44), top: px(12), right: px(WIN_W - 12), bottom: px(44) }));
    match s.page {
        Page::Options => {
            for (i, h) in [Hit::TogglePath, Hit::ToggleStart, Hit::ToggleDesktop, Hit::ToggleCtx]
                .into_iter()
                .enumerate()
            {
                let y = 180 + i as i32 * 26;
                out.push((h, RECT { left: px(40), top: px(y - 4), right: px(WIN_W - 40), bottom: px(y + 20) }));
            }
            out.push((Hit::Install, RECT { left: px(WIN_W / 2 - 100), top: px(310), right: px(WIN_W / 2 + 100), bottom: px(350) }));
        }
        Page::Done => {
            out.push((Hit::Launch, RECT { left: px(WIN_W / 2 - 150), top: px(240), right: px(WIN_W / 2 - 10), bottom: px(280) }));
            out.push((Hit::Quit, RECT { left: px(WIN_W / 2 + 10), top: px(240), right: px(WIN_W / 2 + 150), bottom: px(280) }));
        }
        Page::Error | Page::Removed => {
            out.push((Hit::Quit, RECT { left: px(WIN_W / 2 - 70), top: px(300), right: px(WIN_W / 2 + 70), bottom: px(340) }));
        }
        Page::RemoveConfirm => {
            out.push((Hit::Remove, RECT { left: px(WIN_W / 2 - 150), top: px(240), right: px(WIN_W / 2 - 10), bottom: px(280) }));
            out.push((Hit::Cancel, RECT { left: px(WIN_W / 2 + 10), top: px(240), right: px(WIN_W / 2 + 150), bottom: px(280) }));
        }
        Page::Progress => {}
    }
    out
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ---- painting -----------------------------------------------------------------

struct Painter {
    hdc: HDC,
    dpi: f32,
}

#[derive(Clone, Copy)]
struct TextStyle {
    px_size: i32,
    color: COLORREF,
    bold: bool,
    track: i32,
}

#[derive(Clone, Copy)]
struct UiRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn text_style(px_size: i32, color: COLORREF, bold: bool, track: i32) -> TextStyle {
    TextStyle { px_size, color, bold, track }
}

impl Painter {
    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.dpi) as i32
    }

    fn fill(&self, x: i32, y: i32, w: i32, h: i32, c: COLORREF) {
        unsafe {
            let b = CreateSolidBrush(c);
            let r = RECT {
                left: self.px(x),
                top: self.px(y),
                right: self.px(x + w),
                bottom: self.px(y + h),
            };
            FillRect(self.hdc, &r, b);
            let _ = DeleteObject(b.into());
        }
    }

    /// hairline in device pixels so rules stay crisp at every scale
    fn rule(&self, x: i32, y: i32, w: i32, c: COLORREF) {
        unsafe {
            let b = CreateSolidBrush(c);
            let r = RECT {
                left: self.px(x),
                top: self.px(y),
                right: self.px(x + w),
                bottom: self.px(y) + (self.dpi.max(1.0) as i32),
            };
            FillRect(self.hdc, &r, b);
            let _ = DeleteObject(b.into());
        }
    }

    /// an anti-aliased rounded rect over a flat `under` color: the straight
    /// runs go through FillRect, and only the corner pixels are blended
    #[allow(clippy::too_many_arguments)]
    fn round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: COLORREF, under: COLORREF) {
        let (x, y, w, h, r) = (x * self.dpi, y * self.dpi, w * self.dpi, h * self.dpi, (r * self.dpi).min(w * self.dpi / 2.0).min(h * self.dpi / 2.0));
        let (x0, y0, x1, y1) = (x.round() as i32, y.round() as i32, (x + w).round() as i32, (y + h).round() as i32);
        let ri = r.ceil() as i32;
        unsafe {
            let b = CreateSolidBrush(c);
            for rect in [
                RECT { left: x0 + ri, top: y0, right: x1 - ri, bottom: y1 },
                RECT { left: x0, top: y0 + ri, right: x0 + ri, bottom: y1 - ri },
                RECT { left: x1 - ri, top: y0 + ri, right: x1, bottom: y1 - ri },
            ] {
                if rect.right > rect.left && rect.bottom > rect.top {
                    FillRect(self.hdc, &rect, b);
                }
            }
            let _ = DeleteObject(b.into());
            let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0, h / 2.0);
            let corners = [(x0, y0), (x1 - ri, y0), (x0, y1 - ri), (x1 - ri, y1 - ri)];
            for (ox, oy) in corners {
                for py in oy..oy + ri {
                    for px in ox..ox + ri {
                        let (qx, qy) = ((px as f32 + 0.5 - cx).abs() - (hw - r), (py as f32 + 0.5 - cy).abs() - (hh - r));
                        let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                        let a = (0.5 - d).clamp(0.0, 1.0);
                        if a > 0.0 {
                            let _ = SetPixelV(self.hdc, px, py, mix(under, c, a));
                        }
                    }
                }
            }
        }
    }

    /// a rounded outline one device pixel wide, filled with `inner`
    #[allow(clippy::too_many_arguments)]
    fn ring(&self, x: f32, y: f32, w: f32, h: f32, r: f32, edge: COLORREF, inner: COLORREF, under: COLORREF) {
        self.round(x, y, w, h, r, edge, under);
        let t = 1.0 / self.dpi;
        self.round(x + t, y + t, w - t * 2.0, h - t * 2.0, r - t, inner, edge);
    }

    /// an on/off switch: a pill track with a round knob
    fn switch(&self, x: i32, y: i32, on: bool, hovered: bool) {
        let (w, h) = (30.0, 16.0);
        let (x, y) = (x as f32, y as f32);
        let knob = h - 4.0;
        if on {
            self.round(x, y, w, h, h / 2.0, if hovered { PAPER } else { PAPER_DIM }, BG);
            self.round(x + w - knob - 2.0, y + 2.0, knob, knob, knob / 2.0, INK0, PAPER_DIM);
        } else {
            self.ring(x, y, w, h, h / 2.0, if hovered { MUTE } else { RULE2 }, SURFACE, BG);
            self.round(x + 2.0, y + 2.0, knob, knob, knob / 2.0, if hovered { TEXT } else { MUTE }, SURFACE);
        }
    }

    fn font(&self, px: i32, bold: bool) -> isize {
        unsafe {
            let f = CreateFontW(
                -self.px(px),
                0,
                0,
                0,
                if bold { FW_BOLD.0 as i32 } else { FW_NORMAL.0 as i32 },
                0,
                0,
                0,
                Default::default(),
                Default::default(),
                Default::default(),
                CLEARTYPE_QUALITY,
                Default::default(),
                face(),
            );
            f.0 as isize
        }
    }

    /// draw `text` at logical (x, y); tracking adds letterspacing in device px.
    /// returns the end x in logical units
    fn text(&self, x: i32, y: i32, text: &str, style: TextStyle) -> i32 {
        unsafe {
            let f = self.font(style.px_size, style.bold);
            let old: HGDIOBJ = SelectObject(self.hdc, HGDIOBJ(f as *mut core::ffi::c_void));
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, style.color);
            SetTextCharacterExtra(self.hdc, (style.track as f32 * self.dpi) as i32);
            let wtext: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(self.hdc, self.px(x), self.px(y), &wtext);
            let mut size = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(self.hdc, &wtext, &mut size);
            SelectObject(self.hdc, old);
            let _ = DeleteObject(HGDIOBJ(f as *mut core::ffi::c_void));
            x + (size.cx as f32 / self.dpi) as i32
        }
    }

    fn text_w(&self, text: &str, px_size: i32, bold: bool, track: i32) -> i32 {
        unsafe {
            let f = self.font(px_size, bold);
            let old = SelectObject(self.hdc, HGDIOBJ(f as *mut core::ffi::c_void));
            SetTextCharacterExtra(self.hdc, (track as f32 * self.dpi) as i32);
            let wtext: Vec<u16> = text.encode_utf16().collect();
            let mut size = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(self.hdc, &wtext, &mut size);
            SelectObject(self.hdc, old);
            let _ = DeleteObject(HGDIOBJ(f as *mut core::ffi::c_void));
            (size.cx as f32 / self.dpi) as i32
        }
    }

    fn text_centered(&self, cx: i32, y: i32, text: &str, style: TextStyle) {
        let w = self.text_w(text, style.px_size, style.bold, style.track);
        self.text(cx - w / 2, y, text, style);
    }

    fn button(&self, rect: UiRect, label: &str, primary: bool, hovered: bool) {
        let (x, y, w, h) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
        if primary {
            self.round(x, y, w, h, 6.0, if hovered { PAPER } else { PAPER_DIM }, BG);
            let tw = self.text_w(label, 13, false, 3);
            self.text(rect.x + (rect.w - tw) / 2, rect.y + (rect.h - 16) / 2, label, TextStyle { px_size: 13, color: INK0, bold: false, track: 3 });
        } else {
            self.ring(x, y, w, h, 6.0, if hovered { MUTE } else { RULE2 }, if hovered { SURFACE } else { BG }, BG);
            let tw = self.text_w(label, 13, false, 3);
            self.text(
                rect.x + (rect.w - tw) / 2,
                rect.y + (rect.h - 16) / 2,
                label,
                TextStyle { px_size: 13, color: if hovered { TEXT2 } else { MUTE }, bold: false, track: 3 },
            );
        }
    }
}

pub fn paint(hdc: HDC, dpi: f32, s: &State) {
    let p = Painter { hdc, dpi };

    // ground + footer band
    p.fill(0, 0, WIN_W, WIN_H, BG);
    p.fill(0, WIN_H - 36, WIN_W, 36, INK0);
    p.rule(0, WIN_H - 36, WIN_W, RULE);

    // wordmark: the app's inverted ">_<" badge + name (matches the title bar)
    p.round(24.0, 18.0, 40.0, 26.0, 5.0, PAPER, BG);
    p.text(31, 22, ">_<", text_style(13, INK0, true, 0));
    p.text(76, 21, "termie", text_style(17, TEXT2, false, 0));
    // close ×, red on hover like the app's own window control
    let close_hover = s.hover == Some(Hit::Close);
    if close_hover {
        p.round((WIN_W - 44) as f32, 12.0, 32.0, 32.0, 6.0, CLOSE_RED, BG);
    }
    p.text_centered(WIN_W - 28, 19, "\u{00d7}", text_style(15, if close_hover { WHITE } else { MUTE }, false, 0));

    p.rule(0, 64, WIN_W, RULE);
    p.text(24, 80, "GPU TERMINAL MULTIPLEXER", text_style(11, MUTE, false, 3));
    let vw = p.text_w(&payload::APP_VERSION.to_uppercase(), 11, false, 3);
    p.text(WIN_W - 24 - vw, 80, &payload::APP_VERSION.to_uppercase(), text_style(11, MUTE, false, 3));

    let footer = "MIT / APACHE-2.0 \u{b7} PER-USER \u{b7} NO ADMIN";
    p.text(24, WIN_H - 27, footer, text_style(10, MUTE, false, 2));
    let rot = "ROT";
    let rw = p.text_w(rot, 10, false, 2);
    p.text(WIN_W - 24 - rw, WIN_H - 27, rot, text_style(10, MUTE, false, 2));

    match s.page {
        Page::Options => {
            p.text(24, 122, "INSTALL TO", text_style(11, MUTE, false, 3));
            let dir = engine::install_dir();
            p.text(24, 142, &dir.to_string_lossy(), text_style(12, TEXT, false, 0));
            for (i, (hit, label, on)) in [
                (Hit::TogglePath, "add to PATH", s.opts.add_path),
                (Hit::ToggleStart, "start menu shortcut", s.opts.start_menu),
                (Hit::ToggleDesktop, "desktop shortcut", s.opts.desktop),
                (Hit::ToggleCtx, "\"open in termie\" on folders", s.opts.context_menu),
            ]
            .into_iter()
            .enumerate()
            {
                let y = 180 + i as i32 * 26;
                let hov = s.hover == Some(hit);
                p.switch(40, y - 1, on, hov);
                p.text(82, y - 1, label, text_style(13, if hov { TEXT2 } else { TEXT }, false, 0));
            }
            if s.old_msi {
                p.text(40, 288, "an older MSI install will be removed first", text_style(10, MUTE, false, 0));
            }
            p.button(UiRect { x: WIN_W / 2 - 100, y: 310, w: 200, h: 40 }, "INSTALL", true, s.hover == Some(Hit::Install));
        }
        Page::Progress => {
            p.text_centered(WIN_W / 2, 150, if s.auto_update { "UPDATING" } else { "INSTALLING" }, text_style(13, TEXT2, false, 4));
            // hairline track + paper fill
            let tx = 60;
            let tw = WIN_W - 120;
            p.round(tx as f32, 189.0, tw as f32, 4.0, 2.0, RULE, BG);
            let filled = tw as f32 * s.progress.clamp(0.0, 1.0);
            if filled >= 4.0 {
                p.round(tx as f32, 189.0, filled, 4.0, 2.0, PAPER, RULE);
            }
            p.text_centered(WIN_W / 2, 214, &s.detail, text_style(11, MUTE, false, 0));
        }
        Page::Done => {
            p.text_centered(WIN_W / 2, 140, "INSTALLED", text_style(13, TEXT2, false, 4));
            let dir = engine::install_dir();
            p.text_centered(WIN_W / 2, 168, &dir.to_string_lossy(), text_style(11, MUTE, false, 0));
            p.button(UiRect { x: WIN_W / 2 - 150, y: 240, w: 140, h: 40 }, "LAUNCH", true, s.hover == Some(Hit::Launch));
            p.button(UiRect { x: WIN_W / 2 + 10, y: 240, w: 140, h: 40 }, "CLOSE", false, s.hover == Some(Hit::Quit));
        }
        Page::Error => {
            p.text_centered(WIN_W / 2, 140, "INSTALL FAILED", text_style(13, TEXT2, false, 4));
            // naive wrap at ~64 chars keeps the message on the window
            let mut y = 172;
            let msg = s.error.clone();
            let mut line = String::new();
            for word in msg.split_whitespace() {
                if line.len() + word.len() + 1 > 64 {
                    p.text_centered(WIN_W / 2, y, &line, text_style(11, MUTE, false, 0));
                    y += 18;
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            if !line.is_empty() {
                p.text_centered(WIN_W / 2, y, &line, text_style(11, MUTE, false, 0));
            }
            p.button(UiRect { x: WIN_W / 2 - 70, y: 300, w: 140, h: 40 }, "CLOSE", false, s.hover == Some(Hit::Quit));
        }
        Page::RemoveConfirm => {
            p.text_centered(WIN_W / 2, 140, "REMOVE TERMIE?", text_style(13, TEXT2, false, 4));
            p.text_centered(WIN_W / 2, 168, "settings and session files are kept", text_style(11, MUTE, false, 0));
            p.button(UiRect { x: WIN_W / 2 - 150, y: 240, w: 140, h: 40 }, "REMOVE", true, s.hover == Some(Hit::Remove));
            p.button(UiRect { x: WIN_W / 2 + 10, y: 240, w: 140, h: 40 }, "CANCEL", false, s.hover == Some(Hit::Cancel));
        }
        Page::Removed => {
            p.text_centered(WIN_W / 2, 150, "REMOVED", text_style(13, TEXT2, false, 4));
            p.text_centered(WIN_W / 2, 178, "thanks for flying termie", text_style(11, MUTE, false, 0));
            p.button(UiRect { x: WIN_W / 2 - 70, y: 300, w: 140, h: 40 }, "CLOSE", false, s.hover == Some(Hit::Quit));
        }
    }
}

// ---- live window ------------------------------------------------------------------

const WM_ENGINE: u32 = WM_APP + 1;
const TIMER_AUTOCLOSE: usize = 7;

pub fn run(shared: Shared) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("termie_setup");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(Some(hinst.into()), PCWSTR(1 as _)).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        *CTX.lock().unwrap() = Some(Arc::new(Ctx { state: shared.clone(), font_cache: Mutex::new(Vec::new()) }));

        // size for the primary monitor's dpi; centered
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        // a first guess at 96dpi; WM_DPICHANGED isn't worth handling for a
        // short-lived installer — the window is created on the primary at its
        // effective dpi via the PMv2 manifest
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("termie setup"),
            WS_POPUP | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WIN_W,
            WIN_H,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap();
        let dpi = GetDpiForWindow(hwnd) as f32 / 96.0;
        let w = (WIN_W as f32 * dpi) as i32;
        let h = (WIN_H as f32 * dpi) as i32;
        let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(
            hwnd,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            true,
        );
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const core::ffi::c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);

        // /update starts the engine immediately
        let auto = shared.lock().unwrap().auto_update;
        if auto {
            start_engine(hwnd, shared.clone());
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        *CTX.lock().unwrap() = None;
    }
}

fn start_engine(hwnd: HWND, shared: Shared) {
    {
        let mut s = shared.lock().unwrap();
        s.page = Page::Progress;
        s.progress = 0.0;
    }
    let hwnd_val = hwnd.0 as isize;
    std::thread::spawn(move || {
        let opts = { shared.lock().unwrap().opts_snapshot() };
        let shared2 = shared.clone();
        let hw = HWND(hwnd_val as *mut core::ffi::c_void);
        let result = engine::install(&opts, |frac, label| {
            if let Ok(mut s) = shared2.lock() {
                s.progress = frac;
                s.detail = label.to_string();
            }
            unsafe {
                let _ = PostMessageW(Some(hw), WM_ENGINE, WPARAM(0), LPARAM(0));
            }
        });
        if let Ok(mut s) = shared.lock() {
            match result {
                Ok(()) => s.page = Page::Done,
                Err(e) => {
                    s.page = Page::Error;
                    s.error = e;
                }
            }
        }
        unsafe {
            let _ = PostMessageW(Some(hw), WM_ENGINE, WPARAM(1), LPARAM(0));
        }
    });
}

impl State {
    fn opts_snapshot(&self) -> Options {
        Options {
            add_path: self.opts.add_path,
            start_menu: self.opts.start_menu,
            desktop: self.opts.desktop,
            context_menu: self.opts.context_menu,
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ctx = CTX.lock().unwrap().clone();
    let Some(ctx) = ctx else {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    };
    let _ = &ctx.font_cache;
    match msg {
        WM_PAINT => unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            // double buffer: paint into a dib, blit once
            let mem = CreateCompatibleDC(Some(hdc));
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            if let Ok(bmp) = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                let old = SelectObject(mem, bmp.into());
                {
                    let s = ctx.state.lock().unwrap();
                    let dpi = GetDpiForWindow(hwnd) as f32 / 96.0;
                    paint(mem, dpi, &s);
                }
                let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
            }
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        },
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let x = (lp.0 & 0xffff) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            let mut changed = false;
            {
                let mut s = ctx.state.lock().unwrap();
                let hit = hit_rects(dpi, &s).into_iter().find(|(_, r)| in_rect(r, x, y)).map(|(h, _)| h);
                if s.hover != hit {
                    s.hover = hit;
                    changed = true;
                }
            }
            if changed {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lp.0 & 0xffff) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            let hit = {
                let s = ctx.state.lock().unwrap();
                hit_rects(dpi, &s).into_iter().find(|(_, r)| in_rect(r, x, y)).map(|(h, _)| h)
            };
            if let Some(hit) = hit {
                handle_click(hwnd, &ctx, hit);
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let r = unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
            if r.0 == HTCLIENT as isize {
                // the top band drags the window; controls there still hit-test
                // through WM_MOUSEMOVE/WM_LBUTTONUP because we only promote the
                // band left of the close button
                let mut pt = windows::Win32::Foundation::POINT {
                    x: (lp.0 & 0xffff) as i16 as i32,
                    y: ((lp.0 >> 16) & 0xffff) as i16 as i32,
                };
                let _ = unsafe {
                    windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt)
                };
                let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
                let band_h = (64.0 * dpi) as i32;
                let close_l = ((WIN_W - 52) as f32 * dpi) as i32;
                if pt.y < band_h && pt.x < close_l {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            r
        }
        WM_ENGINE => {
            {
                let s = ctx.state.lock().unwrap();
                if wp.0 == 1 && s.auto_update && s.page == Page::Done {
                    engine::launch_app();
                    unsafe {
                        SetTimer(Some(hwnd), TIMER_AUTOCLOSE, 600, None);
                    }
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_AUTOCLOSE {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_AUTOCLOSE);
                    PostQuitMessage(0);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn handle_click(hwnd: HWND, ctx: &Arc<Ctx>, hit: Hit) {
    let mut relaunch_engine = false;
    let mut do_uninstall = false;
    {
        let mut s = ctx.state.lock().unwrap();
        match hit {
            Hit::Close | Hit::Quit | Hit::Cancel => {
                // ignore close while the engine is mid-flight
                if s.page != Page::Progress {
                    unsafe { PostQuitMessage(0) };
                }
            }
            Hit::TogglePath => s.opts.add_path = !s.opts.add_path,
            Hit::ToggleStart => s.opts.start_menu = !s.opts.start_menu,
            Hit::ToggleDesktop => s.opts.desktop = !s.opts.desktop,
            Hit::ToggleCtx => s.opts.context_menu = !s.opts.context_menu,
            Hit::Install => relaunch_engine = true,
            Hit::Launch => {
                engine::launch_app();
                unsafe { PostQuitMessage(0) };
            }
            Hit::Remove => do_uninstall = true,
        }
    }
    if relaunch_engine {
        start_engine(hwnd, ctx.state.clone());
    }
    if do_uninstall {
        let r = engine::uninstall();
        let mut s = ctx.state.lock().unwrap();
        match r {
            Ok(()) => s.page = Page::Removed,
            Err(e) => {
                s.page = Page::Error;
                s.error = e;
            }
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

// ---- headless preview -----------------------------------------------------------

/// render one page into a PNG for design review without opening a window
pub fn preview(path: &str, state: &State, scale: f32) -> Result<(), String> {
    unsafe {
        let w = (WIN_W as f32 * scale) as i32;
        let h = (WIN_H as f32 * scale) as i32;
        let mem = CreateCompatibleDC(None);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp: HBITMAP = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| e.to_string())?;
        let old = SelectObject(mem, bmp.into());
        paint(mem, scale, state);
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();

        // dib rows are BGRA top-down (negative height)
        let n = (w * h) as usize;
        let px = std::slice::from_raw_parts(bits as *const u8, n * 4);
        let mut rgba = vec![0u8; n * 4];
        for i in 0..n {
            rgba[i * 4] = px[i * 4 + 2];
            rgba[i * 4 + 1] = px[i * 4 + 1];
            rgba[i * 4 + 2] = px[i * 4];
            rgba[i * 4 + 3] = 255;
        }
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        write_png(path, w as u32, h as u32, &rgba)
    }
}

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c: u32;
        let mut table = [0u32; 256];
        for (n, t) in table.iter_mut().enumerate() {
            c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *t = c;
        }
        c = 0xFFFF_FFFF;
        for &b in data {
            c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        c ^ 0xFFFF_FFFF
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_in = Vec::with_capacity(4 + data.len());
        crc_in.extend_from_slice(kind);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in 0..h as usize {
        raw.push(0); // filter none
        let s = row * w as usize * 4;
        raw.extend_from_slice(&rgba[s..s + w as usize * 4]);
    }
    let idat = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit rgba
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &idat);
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png).map_err(|e| e.to_string())
}
