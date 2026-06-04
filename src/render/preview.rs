//! dev-only: composite a Terminal's grid into a PNG using the real glyph atlas
//! and palette, so rendering changes can be seen without a GPU or window. the
//! glyph coverage and colors match what the GPU blits; the decoration geometry
//! mirrors the renderer in mod.rs. compiled out of release

use super::atlas::{FontId, GlyphAtlas, GlyphKey};
use crate::color::{Palette, Rgb, ThemeId};
use crate::term::Terminal;

pub fn render_png(
    term: &Terminal,
    theme: ThemeId,
    content_pt: f32,
    scale: f32,
    system_fonts: bool,
    path: &str,
) -> std::io::Result<(usize, usize)> {
    let pal = Palette::from_theme(theme);
    let mut atlas = GlyphAtlas::new(content_pt, content_pt, scale, None);
    // mirror the app's lazy fallback: with system fonts loaded, cosmic-text can
    // shape CJK/emoji the bundled font lacks instead of rendering tofu
    if system_fonts {
        atlas.load_system_fonts();
    }
    let m = atlas.metrics(FontId::Content);
    let cw = (m.cell_w.round() as usize).max(1);
    let chh = (m.cell_h.round() as usize).max(1);
    let ascent = m.ascent;
    let g = &term.grid;
    let iw = cw * g.cols;
    let ih = chh * g.rows;

    // linear-light framebuffer, cleared to the theme background
    let bg = lin3(pal.bg);
    let mut fb = vec![bg; iw * ih];

    let t = ((chh as f32) * 0.06).max(1.0).round() as usize;
    for r in 0..g.rows {
        for c in 0..g.cols {
            let cell = g.lines[r][c];
            if cell.attrs.hidden {
                continue;
            }
            let (mut fg_c, mut bg_c) = (cell.fg, cell.bg);
            if cell.attrs.inverse {
                std::mem::swap(&mut fg_c, &mut bg_c);
            }
            let mut fg = pal.resolve_fg(fg_c);
            let cbg = pal.resolve_bg(bg_c);
            if cell.attrs.dim {
                fg = Rgb::new(fg.r / 2, fg.g / 2, fg.b / 2);
            }
            let x0 = c * cw;
            let y0 = r * chh;

            if cbg != pal.bg {
                fill(&mut fb, (iw, ih),x0, y0, cw, chh, lin3(cbg));
            }

            // blinking cells render only their background on the off phase; the
            // preview is a single still frame, so model the off phase
            if cell.attrs.blink {
                continue;
            }

            let fgl = lin3(fg);
            if cell.c != ' ' && cell.c != '\0'
                && let Some(gl) = atlas.get(GlyphKey {
                    font: FontId::Content,
                    c: cell.c,
                    bold: cell.attrs.bold,
                    italic: cell.attrs.italic,
                })
            {
                let dim = atlas.dim as usize;
                let ax = (gl.uv_min[0] * atlas.dim as f32).round() as usize;
                let ay = (gl.uv_min[1] * atlas.dim as f32).round() as usize;
                let gw = gl.width as usize;
                let gh = gl.height as usize;
                let px_left = x0 as f32 + gl.left;
                let px_top = y0 as f32 + ascent - gl.top;
                for gy in 0..gh {
                    for gx in 0..gw {
                        let dx = px_left as i32 + gx as i32;
                        let dy = px_top as i32 + gy as i32;
                        if dx < 0 || dy < 0 || dx as usize >= iw || dy as usize >= ih {
                            continue;
                        }
                        let p = &mut fb[dy as usize * iw + dx as usize];
                        if gl.color {
                            // emoji: composite the glyph's own (srgb) color over
                            // the cell, straight alpha — fg is not applied
                            let i = ((ay + gy) * dim + ax + gx) * 4;
                            let a = atlas.color_data[i + 3] as f32 / 255.0;
                            if a <= 0.0 {
                                continue;
                            }
                            p[0] = p[0] * (1.0 - a) + srgb_to_lin(atlas.color_data[i]) * a;
                            p[1] = p[1] * (1.0 - a) + srgb_to_lin(atlas.color_data[i + 1]) * a;
                            p[2] = p[2] * (1.0 - a) + srgb_to_lin(atlas.color_data[i + 2]) * a;
                        } else {
                            let a = atlas.data[(ay + gy) * dim + ax + gx] as f32 / 255.0;
                            if a <= 0.0 {
                                continue;
                            }
                            p[0] = p[0] * (1.0 - a) + fgl[0] * a;
                            p[1] = p[1] * (1.0 - a) + fgl[1] * a;
                            p[2] = p[2] * (1.0 - a) + fgl[2] * a;
                        }
                    }
                }
            }

            // decorations use the shared geometry so they match the renderer
            super::underline_rects(cell.attrs.underline, cw as f32, chh as f32, t as f32, |rx, ry, rw, rh| {
                fill(
                    &mut fb,
                    (iw, ih),
                    x0 + rx.max(0.0) as usize,
                    y0 + ry.max(0.0) as usize,
                    (rw.ceil() as usize).max(1),
                    (rh.ceil() as usize).max(1),
                    fgl,
                );
            });
            if cell.attrs.strike {
                fill(&mut fb, (iw, ih), x0, y0 + chh / 2, cw, t, fgl);
            }
        }
    }

    let mut rgba = vec![0u8; iw * ih * 4];
    for (i, p) in fb.iter().enumerate() {
        rgba[i * 4] = srgb(p[0]);
        rgba[i * 4 + 1] = srgb(p[1]);
        rgba[i * 4 + 2] = srgb(p[2]);
        rgba[i * 4 + 3] = 255;
    }
    write_png(path, iw as u32, ih as u32, &rgba)?;
    Ok((iw, ih))
}

fn lin3(c: Rgb) -> [f32; 3] {
    let l = c.to_linear_f32();
    [l[0], l[1], l[2]]
}

/// one srgb-encoded byte to linear light (for color-emoji compositing)
fn srgb_to_lin(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn fill(fb: &mut [[f32; 3]], dims: (usize, usize), x: usize, y: usize, w: usize, h: usize, col: [f32; 3]) {
    let (iw, ih) = dims;
    for yy in y..(y + h).min(ih) {
        for xx in x..(x + w).min(iw) {
            fb[yy * iw + xx] = col;
        }
    }
}

fn srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let s = if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

// minimal dependency-free PNG writer: RGBA8, stored (uncompressed) zlib
pub(crate) fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut png: Vec<u8> = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);

    let stride = (w * 4) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0); // filter: none
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }
    chunk(&mut png, b"IDAT", &zlib_store(&raw));
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, &png)
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for &b in ty.iter().chain(data) {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    out.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0;
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while i < data.len() {
        let end = (i + 65535).min(data.len());
        let block = &data[i..end];
        let len = block.len() as u16;
        out.push(if end >= data.len() { 1 } else { 0 });
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
        i = end;
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}
