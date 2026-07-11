use crate::fxhash::FxHashMap;
use std::path::PathBuf;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FontId {
    /// terminal grid content (Iosevka)
    Content,
    /// window chrome: wordmark, meta, controls (Departure Mono)
    Chrome,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub font: FontId,
    pub c: char,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasGlyph {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub width: f32,
    pub height: f32,
    pub left: f32,
    pub top: f32,
    /// true for a color (emoji) glyph: its pixels live in the RGBA color atlas
    /// and carry their own color, so the renderer must not tint them with fg
    pub color: bool,
}

#[derive(Clone, Copy)]
pub struct FontMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    /// font size in physical pixels (the em), for baseline-anchored decorations
    pub px: f32,
    line_height: f32,
    family: &'static str,
    /// base weight regular text shapes at; bold text takes at least 700
    weight: Weight,
}

pub struct GlyphAtlas {
    pub font_system: FontSystem,
    swash: SwashCache,
    buffer: Buffer,

    content: FontMetrics,
    chrome: FontMetrics,
    /// the bundled default content family, used when no override is present
    default_family: &'static str,

    pub data: Vec<u8>,
    pub dim: u32,
    pub dirty: bool,
    /// row band [y0, y1) needing GPU re-upload; None while dirty means upload
    /// the whole texture (used after a full repack/reconfigure)
    pub dirty_y: Option<(u32, u32)>,
    /// parallel RGBA atlas for color (emoji) glyphs, packed in the same coords
    /// as `data`; the renderer samples this when a glyph is color
    pub color_data: Vec<u8>,
    pub color_dirty: bool,
    pub color_dirty_y: Option<(u32, u32)>,
    cursor_x: u32,
    cursor_y: u32,
    shelf_h: u32,
    /// whether system fonts have been scanned into the db yet (lazy)
    system_loaded: bool,

    cache: FxHashMap<GlyphKey, Option<AtlasGlyph>>,
    /// composited grapheme-cluster glyphs, keyed by a string whose first char
    /// encodes bold/italic so the lookup borrows `&str` (no per-hit allocation)
    cluster_cache: FxHashMap<String, Option<AtlasGlyph>>,
    /// reused scratch for the cluster lookup key (style prefix + cluster text)
    cluster_key: String,
    /// kitty images packed into the color atlas, keyed by global image key
    image_cache: FxHashMap<u64, Option<AtlasGlyph>>,
}

const PAD: u32 = 1;

/// bold text shapes at least at 700; a heavier configured base keeps winning
fn bolded(base: Weight) -> Weight {
    if base.0 >= Weight::BOLD.0 { base } else { Weight::BOLD }
}

impl GlyphAtlas {
    pub fn new(content_pt: f32, chrome_pt: f32, scale: f32, content_override: Option<&'static str>, line_height: f32) -> Self {
        // start from an EMPTY db (no system-font scan) so startup is fast; only
        // the bundled fonts are loaded now. system fonts are scanned lazily via
        // load_system_fonts() once the window is up and the user needs them.
        let db = cosmic_text::fontdb::Database::new();
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        // load bundled fonts; fall back to generic monospace if unavailable
        let (maple, chrome_family) = load_bundled_fonts(&mut font_system);
        // content uses the chosen family when present, else the bundled default
        let content_family = match content_override {
            Some(name) if family_present(&font_system, name) => name,
            _ => maple,
        };

        let swash = SwashCache::new();

        let content_px = (content_pt * scale).round().max(6.0);
        let chrome_px = (chrome_pt * scale).round().max(6.0);
        let content_lh = (content_px * line_height.clamp(0.8, 3.0)).round();
        let chrome_lh = (chrome_px * 1.4).round();

        let mut buffer = Buffer::new(&mut font_system, Metrics::new(content_px, content_lh));
        buffer.set_size(None, None);

        let mut atlas = GlyphAtlas {
            font_system,
            swash,
            buffer,
            content: FontMetrics {
                cell_w: content_px * 0.5,
                cell_h: content_lh,
                ascent: content_px,
                px: content_px,
                line_height: content_lh,
                family: content_family,
                weight: Weight::NORMAL,
            },
            chrome: FontMetrics {
                cell_w: chrome_px * 0.5,
                cell_h: chrome_lh,
                ascent: chrome_px,
                px: chrome_px,
                line_height: chrome_lh,
                family: chrome_family,
                weight: Weight::NORMAL,
            },
            default_family: maple,
            data: vec![0u8; 0],
            dim: 1024,
            dirty: true,
            dirty_y: None,
            color_data: vec![0u8; 0],
            color_dirty: false,
            color_dirty_y: None,
            cursor_x: PAD,
            cursor_y: PAD,
            shelf_h: 0,
            system_loaded: false,
            cache: FxHashMap::default(),
            cluster_cache: FxHashMap::default(),
            cluster_key: String::new(),
            image_cache: FxHashMap::default(),
        };
        atlas.data = vec![0u8; (atlas.dim * atlas.dim) as usize];
        atlas.color_data = vec![0u8; (atlas.dim * atlas.dim * 4) as usize];

        atlas.content = atlas.measure(atlas.content);
        atlas.chrome = atlas.measure(atlas.chrome);
        atlas
    }

    pub fn metrics(&self, font: FontId) -> FontMetrics {
        match font {
            FontId::Content => self.content,
            FontId::Chrome => self.chrome,
        }
    }

    /// the content font family currently in use (the bundled default name)
    pub fn content_family(&self) -> &'static str {
        self.content.family
    }

    /// is a font family with this name available in the db?
    pub fn has_family(&self, name: &str) -> bool {
        family_present(&self.font_system, name)
    }

    /// every installed monospace family name, sorted and de-duplicated —
    /// what the font picker offers. only meaningful after load_system_fonts
    pub fn monospace_families(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .font_system
            .db()
            .faces()
            .filter(|f| f.monospaced)
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            // fontdb exposes @-prefixed vertical CJK variants; not pickable
            .filter(|n| !n.starts_with('@'))
            .collect();
        names.sort_by_key(|n| n.to_ascii_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        names
    }

    /// scan system fonts into the db on first call (deferred off the startup
    /// path). returns true only on the call that actually did the scan, so the
    /// caller can refresh anything derived from the font list
    pub fn load_system_fonts(&mut self) -> bool {
        if self.system_loaded {
            return false;
        }
        self.system_loaded = true;
        self.font_system.db_mut().load_system_fonts();
        true
    }

    /// drop cached missing-glyph (tofu) entries so they re-rasterize against
    /// fonts that have since been loaded; keeps already-packed glyphs intact
    pub fn invalidate_missing(&mut self) {
        let before = self.cache.len();
        self.cache.retain(|_, v| v.is_some());
        if self.cache.len() != before {
            self.dirty = true;
            self.dirty_y = None;
        }
    }

    /// extend the pending upload band to cover rows [y0, y1); a full upload
    /// already queued (dirty with no band) is left as a full upload
    fn mark_dirty_rows(&mut self, y0: u32, y1: u32) {
        if self.dirty && self.dirty_y.is_none() {
            // full upload already pending — keep it
        } else {
            self.dirty_y = Some(match self.dirty_y {
                Some((a, b)) => (a.min(y0), b.max(y1)),
                None => (y0, y1),
            });
        }
        self.dirty = true;
    }

    /// re-measure for new sizes/family and reset the glyph cache, REUSING the
    /// existing FontSystem (avoids re-scanning all system fonts on every change)
    pub fn reconfigure(&mut self, content_pt: f32, chrome_pt: f32, scale: f32, content_override: Option<&'static str>, line_height: f32, content_weight: u16) {
        let content_px = (content_pt * scale).round().max(6.0);
        let chrome_px = (chrome_pt * scale).round().max(6.0);
        let content_lh = (content_px * line_height.clamp(0.8, 3.0)).round();
        let chrome_lh = (chrome_px * 1.4).round();
        let content_family = match content_override {
            Some(name) if family_present(&self.font_system, name) => name,
            _ => self.default_family,
        };
        self.content = FontMetrics {
            cell_w: content_px * 0.5,
            cell_h: content_lh,
            ascent: content_px,
            px: content_px,
            line_height: content_lh,
            family: content_family,
            weight: Weight(content_weight.clamp(100, 900)),
        };
        self.chrome = FontMetrics {
            cell_w: chrome_px * 0.5,
            cell_h: chrome_lh,
            ascent: chrome_px,
            px: chrome_px,
            line_height: chrome_lh,
            family: self.chrome.family,
            weight: self.chrome.weight,
        };
        self.content = self.measure(self.content);
        self.chrome = self.measure(self.chrome);
        // discard the old rasterized glyphs and repack from scratch (same dim)
        self.repack_at(self.dim);
    }

    /// reset the shelf packing + glyph cache and clear the cpu atlas buffers,
    /// reallocating them when `dim` changes (the grow path); flags a full
    /// re-upload. shared by reconfigure (same dim) and the 1024 -> 2048 grow
    fn repack_at(&mut self, dim: u32) {
        if dim != self.dim || self.data.len() != (dim * dim) as usize {
            self.data = vec![0u8; (dim * dim) as usize];
            self.color_data = vec![0u8; (dim * dim * 4) as usize];
            self.dim = dim;
        } else {
            // same dim: zero in place (avoids a ~5MB realloc on a font/size change)
            self.data.iter_mut().for_each(|b| *b = 0);
            self.color_data.iter_mut().for_each(|b| *b = 0);
        }
        self.cache.clear();
        self.cluster_cache.clear();
        self.image_cache.clear();
        self.cursor_x = PAD;
        self.cursor_y = PAD;
        self.shelf_h = 0;
        self.dirty = true;
        self.dirty_y = None;
        self.color_dirty = true;
        self.color_dirty_y = None;
    }

    fn measure(&mut self, mut m: FontMetrics) -> FontMetrics {
        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        let attrs = Attrs::new().family(Family::Name(m.family)).weight(m.weight);
        self.buffer
            .set_text("Mgjpq0", &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);
        // metrics come from the first (only) layout run
        if let Some(run) = self.buffer.layout_runs().next() {
            m.ascent = run.line_y;
            if let Some(g) = run.glyphs.first() {
                // whole-pixel column pitch: a fractional advance lands every
                // column on a different subpixel phase, which reads as uneven
                // glyph weight and box-stem wobble across the grid
                m.cell_w = g.w.round().max(1.0);
            }
        }
        m.cell_h = m.line_height;
        m
    }

    pub fn get(&mut self, key: GlyphKey) -> Option<AtlasGlyph> {
        if let Some(g) = self.cache.get(&key) {
            return *g;
        }
        match self.rasterize(key) {
            RasterOutcome::Glyph(g) => {
                self.cache.insert(key, Some(g));
                Some(g)
            }
            // nothing to draw — caching None is correct here
            RasterOutcome::Empty => {
                self.cache.insert(key, None);
                None
            }
            // the shelf filled: grow 1024 -> 2048, or (already at max) clear and
            // repack for forward progress, then retry once. never cache a NoSpace
            // as None, or this glyph would render blank for the rest of the session
            RasterOutcome::NoSpace => {
                const MAX_DIM: u32 = 2048;
                self.repack_at(MAX_DIM);
                match self.rasterize(key) {
                    RasterOutcome::Glyph(g) => {
                        self.cache.insert(key, Some(g));
                        Some(g)
                    }
                    _ => None,
                }
            }
        }
    }

    /// rasterize printable ASCII for the content font (regular weight) up front
    /// so the first frames of shell output hit a warm cache instead of shaping
    /// ~95 glyphs through cosmic-text on the paint path. meant to run deferred,
    /// after the window is shown — cheap and idempotent (get() caches)
    pub fn prewarm_ascii(&mut self) {
        for c in ' '..='~' {
            let _ = self.get(GlyphKey {
                font: FontId::Content,
                c,
                bold: false,
                italic: false,
            });
        }
    }

    fn rasterize(&mut self, key: GlyphKey) -> RasterOutcome {
        if key.c == ' ' || key.c.is_control() {
            return RasterOutcome::Empty;
        }

        let m = self.metrics(key.font);

        // box-drawing & block elements are drawn to fill the whole cell, so
        // borders connect with no vertical seams at any line height (the font's
        // own glyphs are only ~1 em tall). unsupported ones fall through to the
        // font path below.
        let cw = m.cell_w.round().max(1.0) as u32;
        let ch = m.cell_h.round().max(1.0) as u32;
        if let Some(cov) = super::boxdraw::coverage(key.c, cw as usize, ch as usize, m.px / 12.0) {
            let (x, y) = match self.alloc(cw, ch) {
                Some(p) => p,
                None => return RasterOutcome::NoSpace,
            };
            for row in 0..ch {
                let dst = ((y + row) * self.dim + x) as usize;
                let src = (row * cw) as usize;
                self.data[dst..dst + cw as usize].copy_from_slice(&cov[src..src + cw as usize]);
            }
            self.mark_dirty_rows(y, y + ch);
            let d = self.dim as f32;
            return RasterOutcome::Glyph(AtlasGlyph {
                uv_min: [x as f32 / d, y as f32 / d],
                uv_max: [(x + cw) as f32 / d, (y + ch) as f32 / d],
                width: m.cell_w,
                height: m.cell_h,
                left: 0.0,
                top: m.ascent,
                color: false,
            });
        }

        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        let mut s = [0u8; 4];
        let text = key.c.encode_utf8(&mut s);
        let mut shaped = match self.shape_char_image(text, m.family, m.weight, key.bold, key.italic) {
            Some(i) => i,
            None => return RasterOutcome::Empty,
        };
        // a text-presentation-default symbol (emoji-variation-sequences narrow
        // base; a VS16 would have made an emoji cluster instead) that font
        // fallback resolved to a color emoji glyph re-routes through the
        // monochrome symbol font, so ❤ draws as a one-cell text glyph rather
        // than a color heart overflowing its cell
        if key.font == FontId::Content && crate::grid::emoji_vs_base(key.c) {
            if shaped.4
                && let Some(mono) =
                    self.shape_char_image(text, "Segoe UI Symbol", m.weight, key.bold, key.italic)
                && !mono.4
            {
                shaped = mono;
            }
            // these bases are one cell wide in the grid, but symbol fonts ink
            // them past cell_w and the overflow paints into the next cell (a
            // following '|' vanished under the heart). re-shape at a reduced
            // size so the ink genuinely fits — cleaner than resampling pixels
            let max_w = m.cell_w.floor().max(1.0);
            if shaped.0 as f32 > max_w {
                let family = if shaped.4 { m.family } else { "Segoe UI Symbol" };
                let px = m.px * max_w / shaped.0 as f32;
                self.buffer.set_metrics(Metrics::new(px, m.line_height));
                if let Some(small) = self.shape_char_image(text, family, m.weight, key.bold, key.italic) {
                    shaped = small;
                }
                self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
            }
        }
        let (w, h, left, top, is_color, pixels) = shaped;

        // a full shelf is not a missing glyph: signal NoSpace so get() can grow
        // or evict and retry (never cache it, or it renders blank forever)
        let (x, y) = match self.alloc(w, h) {
            Some(p) => p,
            None => return RasterOutcome::NoSpace,
        };
        if is_color {
            for row in 0..h {
                let dst = (((y + row) * self.dim + x) * 4) as usize;
                let src = (row * w * 4) as usize;
                let n = (w * 4) as usize;
                self.color_data[dst..dst + n].copy_from_slice(&pixels[src..src + n]);
            }
            self.mark_color_dirty(y, y + h);
        } else {
            for row in 0..h {
                let dst = ((y + row) * self.dim + x) as usize;
                let src = (row * w) as usize;
                self.data[dst..dst + w as usize].copy_from_slice(&pixels[src..src + w as usize]);
            }
            self.mark_dirty_rows(y, y + h);
        }

        let d = self.dim as f32;
        RasterOutcome::Glyph(AtlasGlyph {
            uv_min: [x as f32 / d, y as f32 / d],
            uv_max: [(x + w) as f32 / d, (y + h) as f32 / d],
            width: w as f32,
            height: h as f32,
            left: left as f32,
            top: top as f32,
            color: is_color,
        })
    }

    /// shape one char through `family` (plus system fallback) and pull its
    /// swash image as (w, h, left, top, is_color, pixels) — RGBA for color
    /// glyphs, coverage otherwise. the caller sets buffer metrics first
    fn shape_char_image(
        &mut self,
        text: &str,
        family: &str,
        base: Weight,
        bold: bool,
        italic: bool,
    ) -> Option<(u32, u32, i32, i32, bool, Vec<u8>)> {
        let mut attrs = Attrs::new()
            .family(Family::Name(family))
            .weight(if bold { bolded(base) } else { base });
        if italic {
            attrs = attrs.style(Style::Italic);
        }
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);
        // the glyph's cache key comes from the first run's first glyph
        let cache_key = self
            .buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(|g| g.physical((0.0, 0.0), 1.0).cache_key))?;
        let image = self.swash.get_image(&mut self.font_system, cache_key);
        let image = image.as_ref()?;
        let w = image.placement.width;
        let h = image.placement.height;
        if w == 0 || h == 0 {
            return None;
        }
        let is_color = matches!(image.content, SwashContent::Color);
        // color glyphs keep their RGBA; everything else collapses to coverage
        let pixels = if is_color {
            image.data.clone()
        } else {
            to_alpha(&image.data, w as usize, h as usize, image.content)
        };
        Some((w, h, image.placement.left, image.placement.top, is_color, pixels))
    }

    /// fetch (and cache) a composited glyph for a grapheme cluster (base char +
    /// combining marks). returns None for color/emoji clusters so the caller
    /// falls back to drawing the base char from the per-char path
    pub fn get_cluster(&mut self, text: &str, bold: bool, italic: bool) -> Option<AtlasGlyph> {
        // build the lookup key in a reused buffer so a cache hit allocates
        // nothing: a leading char folds in bold/italic, then the cluster text.
        // String: Borrow<str> lets get() probe by &str without an owned key
        self.cluster_key.clear();
        self.cluster_key.push((b'0' + (bold as u8) + ((italic as u8) << 1)) as char);
        self.cluster_key.push_str(text);
        if let Some(g) = self.cluster_cache.get(self.cluster_key.as_str()) {
            return *g;
        }
        match self.rasterize_cluster(text, bold, italic) {
            RasterOutcome::Glyph(g) => {
                let key = self.cluster_key.clone();
                self.cluster_cache.insert(key, Some(g));
                Some(g)
            }
            RasterOutcome::Empty => {
                let key = self.cluster_key.clone();
                self.cluster_cache.insert(key, None);
                None
            }
            RasterOutcome::NoSpace => {
                const MAX_DIM: u32 = 2048;
                self.repack_at(MAX_DIM);
                // repack cleared cluster_cache but left cluster_key intact
                match self.rasterize_cluster(text, bold, italic) {
                    RasterOutcome::Glyph(g) => {
                        let key = self.cluster_key.clone();
                        self.cluster_cache.insert(key, Some(g));
                        Some(g)
                    }
                    _ => None,
                }
            }
        }
    }

    /// fetch (and cache) a composited ligature-run strip: `text` (ascii
    /// punctuation, one cell per byte) shaped as one string so the font's
    /// calt/liga rules fire across cells. shares the cluster cache — the
    /// style prefix range '4'-'7' keeps run keys clear of cluster keys, and
    /// a grapheme cluster can never equal a multi-char ascii string anyway
    pub fn get_run(&mut self, text: &str, bold: bool, italic: bool) -> Option<AtlasGlyph> {
        self.cluster_key.clear();
        self.cluster_key.push((b'4' + (bold as u8) + ((italic as u8) << 1)) as char);
        self.cluster_key.push_str(text);
        if let Some(g) = self.cluster_cache.get(self.cluster_key.as_str()) {
            return *g;
        }
        match self.rasterize_run(text, bold, italic) {
            RasterOutcome::Glyph(g) => {
                let key = self.cluster_key.clone();
                self.cluster_cache.insert(key, Some(g));
                Some(g)
            }
            RasterOutcome::Empty => {
                let key = self.cluster_key.clone();
                self.cluster_cache.insert(key, None);
                None
            }
            RasterOutcome::NoSpace => {
                const MAX_DIM: u32 = 2048;
                self.repack_at(MAX_DIM);
                match self.rasterize_run(text, bold, italic) {
                    RasterOutcome::Glyph(g) => {
                        let key = self.cluster_key.clone();
                        self.cluster_cache.insert(key, Some(g));
                        Some(g)
                    }
                    _ => None,
                }
            }
        }
    }

    /// composite a ligature run into one strip `text.len()` cells wide,
    /// baseline-aligned like rasterize_cluster. each glyph anchors to its
    /// source cell (its byte offset — ascii input) instead of the shaped pen
    /// position, so identity-shaped chars land pixel-identical to the
    /// per-cell path and only true ligature glyphs span cell boundaries
    fn rasterize_run(&mut self, text: &str, bold: bool, italic: bool) -> RasterOutcome {
        let m = self.metrics(FontId::Content);
        let cell_w = m.cell_w.ceil() as u32;
        let ch = m.line_height.ceil() as u32;
        let cw = cell_w * text.len() as u32;
        if cell_w == 0 || ch == 0 || text.is_empty() {
            return RasterOutcome::Empty;
        }
        let baseline = m.ascent.round() as i32;
        let mut attrs = Attrs::new()
            .family(Family::Name(m.family))
            .weight(if bold { bolded(m.weight) } else { m.weight });
        if italic {
            attrs = attrs.style(Style::Italic);
        }
        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        let mut keys: Vec<(i32, _)> = Vec::new();
        if let Some(run) = self.buffer.layout_runs().next() {
            for g in run.glyphs.iter() {
                let p = g.physical((0.0, 0.0), 1.0);
                keys.push(((g.start as u32 * cell_w) as i32, p.cache_key));
            }
        }
        if keys.is_empty() {
            return RasterOutcome::Empty;
        }

        let mut canvas = vec![0u8; (cw * ch) as usize];
        let mut any = false;
        for (pen_x, ck) in keys {
            let extracted = {
                let img = self.swash.get_image(&mut self.font_system, ck);
                let Some(img) = img.as_ref() else {
                    continue;
                };
                if matches!(img.content, SwashContent::Color) {
                    return RasterOutcome::Empty;
                }
                let (gw, gh) = (img.placement.width, img.placement.height);
                if gw == 0 || gh == 0 {
                    continue;
                }
                let cov = to_alpha(&img.data, gw as usize, gh as usize, img.content);
                (gw, gh, img.placement.left, img.placement.top, cov)
            };
            let (gw, gh, gleft, gtop, cov) = extracted;
            let ox = pen_x + gleft;
            let oy = baseline - gtop;
            for gy in 0..gh as i32 {
                let cy = oy + gy;
                if cy < 0 || cy >= ch as i32 {
                    continue;
                }
                for gx in 0..gw as i32 {
                    let cx = ox + gx;
                    if cx < 0 || cx >= cw as i32 {
                        continue;
                    }
                    let s = cov[(gy as u32 * gw + gx as u32) as usize];
                    let di = (cy as u32 * cw + cx as u32) as usize;
                    if s > canvas[di] {
                        canvas[di] = s;
                        any = true;
                    }
                }
            }
        }
        if !any {
            return RasterOutcome::Empty;
        }

        let (x, y) = match self.alloc(cw, ch) {
            Some(p) => p,
            None => return RasterOutcome::NoSpace,
        };
        for row in 0..ch {
            let dst = ((y + row) * self.dim + x) as usize;
            let src = (row * cw) as usize;
            self.data[dst..dst + cw as usize].copy_from_slice(&canvas[src..src + cw as usize]);
        }
        self.mark_dirty_rows(y, y + ch);
        let d = self.dim as f32;
        RasterOutcome::Glyph(AtlasGlyph {
            uv_min: [x as f32 / d, y as f32 / d],
            uv_max: [(x + cw) as f32 / d, (y + ch) as f32 / d],
            width: cw as f32,
            height: ch as f32,
            left: 0.0,
            top: m.ascent,
            color: false,
        })
    }

    /// composite a grapheme cluster into one cell-sized coverage slot, baseline-
    /// aligned. emoji/color clusters return Empty (per-char fallback). the cell-
    /// aligned slot makes left=0, top=ascent so the caller's normal placement math
    /// drops the composited cell exactly where the base char would have gone
    fn rasterize_cluster(&mut self, text: &str, bold: bool, italic: bool) -> RasterOutcome {
        let m = self.metrics(FontId::Content);
        let cw = m.cell_w.ceil() as u32;
        let ch = m.line_height.ceil() as u32;
        if cw == 0 || ch == 0 {
            return RasterOutcome::Empty;
        }
        let baseline = m.ascent.round() as i32;

        // a ZWJ emoji sequence must shape inside the emoji font itself: through
        // the content font, fallback picks glyphs per codepoint and the GSUB
        // ligature never fires — the family renders as its first member. VS16
        // anywhere marks emoji presentation the same way (❤️, keycap #️⃣)
        let emoji_seq = (text.contains('\u{200D}')
            && text.chars().next().is_some_and(|c| c >= '\u{2600}'))
            || text.contains('\u{FE0F}');
        let family = if emoji_seq { "Segoe UI Emoji" } else { m.family };
        let mut attrs = Attrs::new()
            .family(Family::Name(family))
            .weight(if bold { bolded(m.weight) } else { m.weight });
        if italic {
            attrs = attrs.style(Style::Italic);
        }
        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        let mut keys: Vec<(i32, _)> = Vec::new();
        if let Some(run) = self.buffer.layout_runs().next() {
            for g in run.glyphs.iter() {
                let p = g.physical((0.0, 0.0), 1.0);
                keys.push((p.x, p.cache_key));
            }
        }
        if keys.is_empty() {
            return RasterOutcome::Empty;
        }

        // an emoji ZWJ sequence the font ligates arrives as one color glyph:
        // pack its RGBA like the per-char color path, so 👨‍👩‍👧 draws as a
        // single family glyph instead of falling back to the base char
        if keys.len() == 1 {
            let (w, h, left, top, pixels) = {
                let img = self.swash.get_image(&mut self.font_system, keys[0].1);
                match img.as_ref() {
                    Some(i) if matches!(i.content, SwashContent::Color) => (
                        i.placement.width,
                        i.placement.height,
                        i.placement.left,
                        i.placement.top,
                        i.data.clone(),
                    ),
                    _ => (0, 0, 0, 0, Vec::new()),
                }
            };
            if w > 0 && h > 0 {
                let (x, y) = match self.alloc(w, h) {
                    Some(p) => p,
                    None => return RasterOutcome::NoSpace,
                };
                for row in 0..h {
                    let dst = (((y + row) * self.dim + x) * 4) as usize;
                    let src = (row * w * 4) as usize;
                    let n = (w * 4) as usize;
                    self.color_data[dst..dst + n].copy_from_slice(&pixels[src..src + n]);
                }
                self.mark_color_dirty(y, y + h);
                let d = self.dim as f32;
                return RasterOutcome::Glyph(AtlasGlyph {
                    uv_min: [x as f32 / d, y as f32 / d],
                    uv_max: [(x + w) as f32 / d, (y + h) as f32 / d],
                    width: w as f32,
                    height: h as f32,
                    left: left as f32,
                    top: top as f32,
                    color: true,
                });
            }
        }

        let mut canvas = vec![0u8; (cw * ch) as usize];
        let mut any = false;
        for (pen_x, ck) in keys {
            let extracted = {
                let img = self.swash.get_image(&mut self.font_system, ck);
                let Some(img) = img.as_ref() else {
                    continue;
                };
                if matches!(img.content, SwashContent::Color) {
                    return RasterOutcome::Empty; // let the per-char path handle emoji
                }
                let (gw, gh) = (img.placement.width, img.placement.height);
                if gw == 0 || gh == 0 {
                    continue;
                }
                let cov = to_alpha(&img.data, gw as usize, gh as usize, img.content);
                (gw, gh, img.placement.left, img.placement.top, cov)
            };
            let (gw, gh, gleft, gtop, cov) = extracted;
            let ox = pen_x + gleft;
            let oy = baseline - gtop;
            for gy in 0..gh as i32 {
                let cy = oy + gy;
                if cy < 0 || cy >= ch as i32 {
                    continue;
                }
                for gx in 0..gw as i32 {
                    let cx = ox + gx;
                    if cx < 0 || cx >= cw as i32 {
                        continue;
                    }
                    let s = cov[(gy as u32 * gw + gx as u32) as usize];
                    let di = (cy as u32 * cw + cx as u32) as usize;
                    if s > canvas[di] {
                        canvas[di] = s;
                        any = true;
                    }
                }
            }
        }
        if !any {
            return RasterOutcome::Empty;
        }

        let (x, y) = match self.alloc(cw, ch) {
            Some(p) => p,
            None => return RasterOutcome::NoSpace,
        };
        for row in 0..ch {
            let dst = ((y + row) * self.dim + x) as usize;
            let src = (row * cw) as usize;
            self.data[dst..dst + cw as usize].copy_from_slice(&canvas[src..src + cw as usize]);
        }
        self.mark_dirty_rows(y, y + ch);
        let d = self.dim as f32;
        RasterOutcome::Glyph(AtlasGlyph {
            uv_min: [x as f32 / d, y as f32 / d],
            uv_max: [(x + cw) as f32 / d, (y + ch) as f32 / d],
            width: cw as f32,
            height: ch as f32,
            left: 0.0,
            top: m.ascent,
            color: false,
        })
    }

    /// pack a decoded RGBA image into the color atlas, cached by its global key.
    /// returns the uv rect, or None if it doesn't fit (too large, or the atlas is
    /// full this frame — we never repack mid-draw, which would corrupt already-
    /// emitted glyph instances)
    pub fn get_image(&mut self, key: u64, rgba: &[u8], w: u32, h: u32) -> Option<AtlasGlyph> {
        if let Some(g) = self.image_cache.get(&key) {
            return *g;
        }
        // an image over the atlas ceiling packs box-filtered down instead of
        // never rendering; the glyph keeps the ORIGINAL draw size, so quads
        // still cover the cells the protocol laid out and only sampling blurs
        const MAX_PACK: u32 = 2048 - PAD * 2;
        let needed = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if (w > MAX_PACK || h > MAX_PACK) && w > 0 && h > 0 && rgba.len() >= needed {
            let (scaled, sw, sh) = crate::image::downscale_rgba(rgba, w, h, MAX_PACK);
            let packed = self.get_image(key, &scaled, sw, sh).map(|mut g| {
                g.width = w as f32;
                g.height = h as f32;
                g
            });
            self.image_cache.insert(key, packed);
            return packed;
        }
        match self.pack_image(rgba, w, h) {
            ImagePack::Ok(g) => {
                self.image_cache.insert(key, Some(g));
                Some(g)
            }
            ImagePack::TooBig => {
                self.image_cache.insert(key, None);
                None
            }
            // atlas full (or the image is bigger than the current dim): grow
            // to the max — same move as the glyph path — and retry once. an
            // image between 1024 and 2048 px used to hit this every frame
            // forever, silently never rendering while re-packing each paint
            ImagePack::NoSpace => {
                const MAX_DIM: u32 = 2048;
                self.repack_at(MAX_DIM);
                match self.pack_image(rgba, w, h) {
                    ImagePack::Ok(g) => {
                        self.image_cache.insert(key, Some(g));
                        Some(g)
                    }
                    // still no room: cache the miss so the raster isn't
                    // retried every frame (the cache clears on the next
                    // repack, which is when space could actually appear)
                    _ => {
                        self.image_cache.insert(key, None);
                        None
                    }
                }
            }
        }
    }

    fn pack_image(&mut self, rgba: &[u8], w: u32, h: u32) -> ImagePack {
        if w == 0 || h == 0 {
            return ImagePack::TooBig;
        }
        let needed = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if rgba.len() < needed || w + PAD * 2 > 2048 || h + PAD * 2 > 2048 {
            return ImagePack::TooBig;
        }
        let (x, y) = match self.alloc(w, h) {
            Some(p) => p,
            None => return ImagePack::NoSpace,
        };
        for row in 0..h {
            let dst = (((y + row) * self.dim + x) * 4) as usize;
            let src = (row * w * 4) as usize;
            self.color_data[dst..dst + (w * 4) as usize]
                .copy_from_slice(&rgba[src..src + (w * 4) as usize]);
        }
        self.mark_color_dirty(y, y + h);
        let d = self.dim as f32;
        ImagePack::Ok(AtlasGlyph {
            uv_min: [x as f32 / d, y as f32 / d],
            uv_max: [(x + w) as f32 / d, (y + h) as f32 / d],
            width: w as f32,
            height: h as f32,
            left: 0.0,
            top: 0.0,
            color: true,
        })
    }

    /// extend the color atlas's pending-upload band, mirroring mark_dirty_rows
    fn mark_color_dirty(&mut self, y0: u32, y1: u32) {
        if !(self.color_dirty && self.color_dirty_y.is_none()) {
            self.color_dirty_y = Some(match self.color_dirty_y {
                Some((a, b)) => (a.min(y0), b.max(y1)),
                None => (y0, y1),
            });
        }
        self.color_dirty = true;
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w + PAD * 2 > self.dim {
            return None;
        }
        if self.cursor_x + w + PAD > self.dim {
            self.cursor_y += self.shelf_h + PAD;
            self.cursor_x = PAD;
            self.shelf_h = 0;
        }
        if self.cursor_y + h + PAD > self.dim {
            return None;
        }
        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += w + PAD;
        self.shelf_h = self.shelf_h.max(h);
        Some(pos)
    }
}

/// outcome of rasterizing one glyph: a packed glyph; a genuinely-empty glyph
/// (space / control / zero-size — safe to cache as None); or the shelf ran out
/// of room, which must NOT be cached so get() can grow/evict and retry
enum RasterOutcome {
    Glyph(AtlasGlyph),
    Empty,
    NoSpace,
}

enum ImagePack {
    Ok(AtlasGlyph),
    /// permanently can't fit (larger than the max atlas) — cache as a miss
    TooBig,
    /// the atlas is full this frame — skip without caching, retry next frame
    NoSpace,
}

/// load Iosevka (content) + Departure Mono (chrome) from the assets dir.
/// returns the family names to use (falling back to "monospace" if missing).
fn load_bundled_fonts(fs: &mut FontSystem) -> (&'static str, &'static str) {
    let dir = match assets_font_dir() {
        Some(d) => d,
        None => {
            log::warn!("assets/fonts not found; using system monospace");
            return ("monospace", "monospace");
        }
    };

    let db = fs.db_mut();
    // one mono throughout (capscr is strictly mono-only); chrome differs by size only.
    // load every Maple* face so this survives the exact NF filenames.
    let mut loaded = false;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_font = name.ends_with(".ttf") || name.ends_with(".otf");
            if is_font && name.contains("maple") {
                match db.load_font_file(&path) {
                    Ok(()) => loaded = true,
                    Err(e) => log::warn!("failed to load font {}: {e}", path.display()),
                }
            }
        }
    }

    // detect the actual family name (e.g. "Maple Mono NF") from the loaded faces
    let family: &'static str = if loaded {
        let detected = db.faces().find_map(|face| {
            face.families
                .iter()
                .map(|(n, _)| n.clone())
                .find(|n| n.to_ascii_lowercase().contains("maple"))
        });
        match detected {
            Some(name) => {
                log::info!("using bundled font family: {name}");
                Box::leak(name.into_boxed_str())
            }
            None => "monospace",
        }
    } else {
        log::warn!("no Maple font files found; using system monospace");
        "monospace"
    };
    (family, family)
}

/// true if a font family with this name is present in the db (case-insensitive)
fn family_present(fs: &FontSystem, name: &str) -> bool {
    fs.db()
        .faces()
        .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
}

fn assets_font_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            candidates.push(dir.join("assets/fonts"));
        }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fonts"));
    candidates.push(PathBuf::from("assets/fonts"));
    candidates.into_iter().find(|p| p.exists())
}

fn to_alpha(data: &[u8], w: usize, h: usize, content: SwashContent) -> Vec<u8> {
    match content {
        SwashContent::Mask => data.to_vec(),
        SwashContent::Color => {
            let mut out = vec![0u8; w * h];
            for (o, px) in out.iter_mut().zip(data.chunks(4)) {
                *o = px.get(3).copied().unwrap_or(0);
            }
            out
        }
        SwashContent::SubpixelMask => {
            let mut out = vec![0u8; w * h];
            for (o, px) in out.iter_mut().zip(data.chunks(4)) {
                let r = px.first().copied().unwrap_or(0);
                let g = px.get(1).copied().unwrap_or(0);
                let b = px.get(2).copied().unwrap_or(0);
                *o = r.max(g).max(b);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // on a machine with a color emoji font (Windows ships Segoe UI Emoji) a
    // color glyph must route to the RGBA atlas with non-zero pixels and leave
    // the alpha-atlas slot empty. where no emoji font exists (e.g. CI) the
    // glyph isn't color and there is nothing to assert — the test still passes
    #[test]
    fn color_emoji_routes_to_rgba_atlas() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        atlas.load_system_fonts();
        let Some(g) = atlas.get(GlyphKey {
            font: FontId::Content,
            c: '\u{1F680}', // rocket
            bold: false,
            italic: false,
        }) else {
            return;
        };
        if !g.color {
            return; // no color emoji font on this machine
        }
        let dim = atlas.dim as usize;
        let ax = (g.uv_min[0] * atlas.dim as f32).round() as usize;
        let ay = (g.uv_min[1] * atlas.dim as f32).round() as usize;
        let (mut color_alpha, mut alpha_cov) = (0u32, 0u32);
        for gy in 0..g.height as usize {
            for gx in 0..g.width as usize {
                let p = (ay + gy) * dim + ax + gx;
                color_alpha += atlas.color_data[p * 4 + 3] as u32;
                alpha_cov += atlas.data[p] as u32;
            }
        }
        assert!(color_alpha > 0, "color glyph must have non-zero rgba in the color atlas");
        assert_eq!(alpha_cov, 0, "color glyph must not also be stored in the alpha atlas");
    }

    // growing the atlas must REALLOCATE the cpu buffers, not just zero them:
    // rasterize copies rows at a dim stride, so a stale 1024-sized buffer would
    // index out of bounds the moment dim bumps to 2048
    #[test]
    fn oversized_image_grows_the_atlas() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        assert_eq!(atlas.dim, 1024);
        // taller than the 1024 atlas but within the 2048 max: must grow, not
        // silently fail-and-retry forever
        let (w, h) = (64u32, 1500u32);
        let rgba = vec![128u8; (w * h * 4) as usize];
        let g = atlas.get_image(7, &rgba, w, h);
        assert!(g.is_some(), "the atlas must grow to fit a 1024..2048 image");
        assert_eq!(atlas.dim, 2048);
        // an image over the 2048 ceiling packs downscaled but draws at its
        // original size (it used to silently never render)
        let (bw, bh) = (3000u32, 50u32);
        let big = vec![200u8; (bw * bh * 4) as usize];
        let g = atlas.get_image(8, &big, bw, bh).expect("oversized image still renders");
        assert_eq!((g.width, g.height), (3000.0, 50.0), "draw size stays the original");
        let packed_w = (g.uv_max[0] - g.uv_min[0]) * atlas.dim as f32;
        assert!(packed_w <= 2046.5, "packed pixels fit the atlas ceiling");
        // the cache serves the patched glyph on the second lookup too
        let again = atlas.get_image(8, &big, bw, bh).expect("cached");
        assert_eq!(again.width, 3000.0);
    }

    #[test]
    fn repack_at_reallocates_on_grow() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        assert_eq!(atlas.dim, 1024);
        atlas.repack_at(2048);
        assert_eq!(atlas.dim, 2048);
        assert_eq!(atlas.data.len(), 2048 * 2048);
        assert_eq!(atlas.color_data.len(), 2048 * 2048 * 4);
        assert_eq!((atlas.cursor_x, atlas.cursor_y), (PAD, PAD));
        assert!(atlas.dirty && atlas.dirty_y.is_none(), "grow must flag a full re-upload");
    }

    // exhausting the 1024 shelf must grow to 2048 and keep returning real glyphs
    // — never a permanently-blank slot from a cached alloc-failure. needs enough
    // distinct rasterizable glyphs to fill 1024, so it is gated on the host
    // actually having them (font-poor CI never trips the grow and still passes)
    #[test]
    fn atlas_grows_instead_of_blanking() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        atlas.load_system_fonts();
        // a wide CJK sweep so distinct glyphs pile up fast on a host with the fonts
        for cp in 0x4E00u32..0x4E00 + 6000 {
            if let Some(c) = char::from_u32(cp) {
                let _ = atlas.get(GlyphKey { font: FontId::Content, c, bold: false, italic: false });
            }
            if atlas.dim == 2048 {
                break;
            }
        }
        if atlas.dim != 2048 {
            return; // host lacks enough distinct glyphs to exhaust the 1024 atlas
        }
        // after the grow a normal glyph must resolve to a real packed slot, not a
        // cached blank (the no-cache-on-NoSpace invariant, observed end to end)
        let g = atlas.get(GlyphKey { font: FontId::Content, c: 'A', bold: false, italic: false });
        assert!(g.is_some(), "a normal glyph must still rasterize after the atlas grows");
    }

    // a ligature run composites into one strip exactly run-length cells wide,
    // anchored like a cluster (left 0, top = ascent); a refetch is the cached
    // strip, and bold caches separately from regular
    #[test]
    fn get_run_strip_spans_its_cells_and_caches() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        let cell_w = atlas.metrics(FontId::Content).cell_w.ceil();
        let first = atlas.get_run("==>", false, false).expect("strip");
        assert_eq!(first.width, cell_w * 3.0);
        assert_eq!(first.left, 0.0);
        assert_eq!(atlas.get_run("==>", false, false), Some(first));
        let bold = atlas.get_run("==>", true, false).expect("bold strip");
        assert_ne!(bold, first);
    }

    // a cluster cache hit returns the same glyph the first (rasterizing) call did
    #[test]
    fn get_cluster_cache_hit_is_stable() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        let first = atlas.get_cluster("e\u{0301}", false, false);
        let second = atlas.get_cluster("e\u{0301}", false, false);
        assert_eq!(first, second);
    }

    // the style prefix in the reused cache key keeps bold/italic/regular variants
    // of the SAME cluster text in distinct cache entries (no cross-style bleed)
    #[test]
    fn get_cluster_style_prefix_disambiguates() {
        let mut atlas = GlyphAtlas::new(16.0, 13.0, 1.0, None, 1.32);
        let reg = atlas.get_cluster("a\u{0300}", false, false);
        let bold = atlas.get_cluster("a\u{0300}", true, false);
        let ital = atlas.get_cluster("a\u{0300}", false, true);
        // re-fetching each style returns its own cached entry unchanged
        assert_eq!(reg, atlas.get_cluster("a\u{0300}", false, false));
        assert_eq!(bold, atlas.get_cluster("a\u{0300}", true, false));
        assert_eq!(ital, atlas.get_cluster("a\u{0300}", false, true));
    }
}
