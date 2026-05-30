use std::collections::HashMap;
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

#[derive(Clone, Copy)]
pub struct AtlasGlyph {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub width: f32,
    pub height: f32,
    pub left: f32,
    pub top: f32,
}

#[derive(Clone, Copy)]
pub struct FontMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    px: f32,
    line_height: f32,
    family: &'static str,
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
    cursor_x: u32,
    cursor_y: u32,
    shelf_h: u32,
    /// whether system fonts have been scanned into the db yet (lazy)
    system_loaded: bool,

    cache: HashMap<GlyphKey, Option<AtlasGlyph>>,
}

const PAD: u32 = 1;

impl GlyphAtlas {
    pub fn new(content_pt: f32, chrome_pt: f32, scale: f32, content_override: Option<&'static str>) -> Self {
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
        let content_lh = (content_px * 1.32).round();
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
            },
            chrome: FontMetrics {
                cell_w: chrome_px * 0.5,
                cell_h: chrome_lh,
                ascent: chrome_px,
                px: chrome_px,
                line_height: chrome_lh,
                family: chrome_family,
            },
            default_family: maple,
            data: vec![0u8; 0],
            dim: 1024,
            dirty: true,
            dirty_y: None,
            cursor_x: PAD,
            cursor_y: PAD,
            shelf_h: 0,
            system_loaded: false,
            cache: HashMap::new(),
        };
        atlas.data = vec![0u8; (atlas.dim * atlas.dim) as usize];

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
    pub fn reconfigure(&mut self, content_pt: f32, chrome_pt: f32, scale: f32, content_override: Option<&'static str>) {
        let content_px = (content_pt * scale).round().max(6.0);
        let chrome_px = (chrome_pt * scale).round().max(6.0);
        let content_lh = (content_px * 1.32).round();
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
        };
        self.chrome = FontMetrics {
            cell_w: chrome_px * 0.5,
            cell_h: chrome_lh,
            ascent: chrome_px,
            px: chrome_px,
            line_height: chrome_lh,
            family: self.chrome.family,
        };
        self.content = self.measure(self.content);
        self.chrome = self.measure(self.chrome);
        // discard the old rasterized glyphs and repack from scratch
        self.cache.clear();
        self.cursor_x = PAD;
        self.cursor_y = PAD;
        self.shelf_h = 0;
        for b in self.data.iter_mut() {
            *b = 0;
        }
        self.dirty = true;
        self.dirty_y = None;
    }

    fn measure(&mut self, mut m: FontMetrics) -> FontMetrics {
        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        let attrs = Attrs::new().family(Family::Name(m.family));
        self.buffer
            .set_text("Mgjpq0", &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);
        // metrics come from the first (only) layout run
        if let Some(run) = self.buffer.layout_runs().next() {
            m.ascent = run.line_y;
            if let Some(g) = run.glyphs.first() {
                m.cell_w = g.w;
            }
        }
        m.cell_h = m.line_height;
        m
    }

    pub fn get(&mut self, key: GlyphKey) -> Option<AtlasGlyph> {
        if let Some(g) = self.cache.get(&key) {
            return *g;
        }
        let g = self.rasterize(key);
        self.cache.insert(key, g);
        g
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

    fn rasterize(&mut self, key: GlyphKey) -> Option<AtlasGlyph> {
        if key.c == ' ' || key.c.is_control() {
            return None;
        }

        let m = self.metrics(key.font);
        let mut attrs = Attrs::new().family(Family::Name(m.family));
        if key.bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        if key.italic {
            attrs = attrs.style(Style::Italic);
        }

        self.buffer.set_metrics(Metrics::new(m.px, m.line_height));
        let mut s = [0u8; 4];
        let text = key.c.encode_utf8(&mut s);
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        // the glyph's cache key comes from the first run's first glyph
        let cache_key = self
            .buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(|g| g.physical((0.0, 0.0), 1.0).cache_key))?;

        let (w, h, left, top, pixels) = {
            let image = self.swash.get_image(&mut self.font_system, cache_key);
            let image = image.as_ref()?;
            let w = image.placement.width;
            let h = image.placement.height;
            if w == 0 || h == 0 {
                return None;
            }
            let pixels = to_alpha(&image.data, w as usize, h as usize, image.content);
            (w, h, image.placement.left, image.placement.top, pixels)
        };

        let (x, y) = self.alloc(w, h)?;
        for row in 0..h {
            let dst = ((y + row) * self.dim + x) as usize;
            let src = (row * w) as usize;
            self.data[dst..dst + w as usize].copy_from_slice(&pixels[src..src + w as usize]);
        }
        self.mark_dirty_rows(y, y + h);

        let d = self.dim as f32;
        Some(AtlasGlyph {
            uv_min: [x as f32 / d, y as f32 / d],
            uv_max: [(x + w) as f32 / d, (y + h) as f32 / d],
            width: w as f32,
            height: h as f32,
            left: left as f32,
            top: top as f32,
        })
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
            for i in 0..(w * h) {
                out[i] = data.get(i * 4 + 3).copied().unwrap_or(0);
            }
            out
        }
        SwashContent::SubpixelMask => {
            let mut out = vec![0u8; w * h];
            for i in 0..(w * h) {
                let r = data.get(i * 4).copied().unwrap_or(0);
                let g = data.get(i * 4 + 1).copied().unwrap_or(0);
                let b = data.get(i * 4 + 2).copied().unwrap_or(0);
                out[i] = r.max(g).max(b);
            }
            out
        }
    }
}
