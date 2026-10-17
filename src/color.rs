#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn from_triple(c: [u8; 3]) -> Self {
        Self::new(c[0], c[1], c[2])
    }

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// per-channel blend toward `other` by `t` in [0,1] for ui colour eases
    pub fn lerp(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb::new(mix(self.r, other.r), mix(self.g, other.g), mix(self.b, other.b))
    }

    pub fn to_linear_f32(self) -> [f32; 4] {
        // srgb -> linear via a precomputed 256-entry table: this runs per cell
        // per color every paint, so the per-channel powf was the hot path's main
        // transcendental cost. the table is bit-identical to the closed form
        let t = srgb_linear_lut();
        [t[self.r as usize], t[self.g as usize], t[self.b as usize], 1.0]
    }
}

/// WCAG relative luminance of an srgb color, via the renderer's lut
fn luminance(c: Rgb) -> f32 {
    let t = srgb_linear_lut();
    0.2126 * t[c.r as usize] + 0.7152 * t[c.g as usize] + 0.0722 * t[c.b as usize]
}

/// linear light back to an srgb-encoded channel in [0,1]
fn lin_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

/// raise fg's contrast against bg to at least `min` (a WCAG ratio, 1..21) by
/// blending fg toward white or black in linear light; luminance is linear in
/// the blend factor, so the target ratio is hit exactly and hue washes out
/// only as far as the ratio demands. text keeps its polarity (light text gets
/// lighter, dark gets darker) unless that side of bg can't deliver the ratio
/// and the other side does it better. pairs that already pass come back as-is
pub fn apply_min_contrast(fg: Rgb, bg: Rgb, min: f32) -> Rgb {
    let lf = luminance(fg);
    let lb = luminance(bg);
    let (hi, lo) = if lf >= lb { (lf, lb) } else { (lb, lf) };
    if (hi + 0.05) / (lo + 0.05) >= min {
        return fg;
    }
    let ratio_white = 1.05 / (lb + 0.05);
    let ratio_black = (lb + 0.05) / 0.05;
    let lighten = if lf >= lb {
        ratio_white >= min || ratio_white >= ratio_black
    } else {
        !(ratio_black >= min || ratio_black >= ratio_white)
    };
    let (toward, target) = if lighten {
        (1.0, (min * (lb + 0.05) - 0.05).min(1.0))
    } else {
        (0.0, ((lb + 0.05) / min - 0.05).max(0.0))
    };
    let t = if lighten {
        if lf >= 1.0 { 0.0 } else { (target - lf) / (1.0 - lf) }
    } else if lf <= 0.0 {
        0.0
    } else {
        (lf - target) / lf
    };
    let t = t.clamp(0.0, 1.0);
    let lut = srgb_linear_lut();
    let mix = |c: u8| {
        let lin = lut[c as usize] + (toward - lut[c as usize]) * t;
        (lin_to_srgb(lin) * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Rgb::new(mix(fg.r), mix(fg.g), mix(fg.b))
}

/// srgb -> linear for all 256 channel values, built once on first use
fn srgb_linear_lut() -> &'static [f32; 256] {
    use std::sync::OnceLock;
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0.0f32; 256];
        let mut i = 0;
        while i < 256 {
            let s = i as f32 / 255.0;
            t[i] = if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            };
            i += 1;
        }
        t
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    DefaultBg,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeId {
    Instrument,
    Koi,
    Paper,
    Catppuccin,
    Gruvbox,
    TokyoNight,
    Nord,
}

impl ThemeId {
    pub const ALL: [ThemeId; 7] = [
        ThemeId::Instrument,
        ThemeId::Koi,
        ThemeId::Paper,
        ThemeId::Catppuccin,
        ThemeId::Gruvbox,
        ThemeId::TokyoNight,
        ThemeId::Nord,
    ];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::Instrument => "instrument",
            ThemeId::Koi => "koi",
            ThemeId::Paper => "paper",
            ThemeId::Catppuccin => "catppuccin",
            ThemeId::Gruvbox => "gruvbox",
            ThemeId::TokyoNight => "tokyo night",
            ThemeId::Nord => "nord",
        }
    }

    pub fn from_name(s: &str) -> Self {
        match s {
            "koi" => ThemeId::Koi,
            "paper" => ThemeId::Paper,
            "catppuccin" => ThemeId::Catppuccin,
            "gruvbox" => ThemeId::Gruvbox,
            "tokyo night" | "tokyonight" => ThemeId::TokyoNight,
            "nord" => ThemeId::Nord,
            _ => ThemeId::Instrument,
        }
    }
}

/// dynamic color overrides a program set at runtime (OSC 4/10/11/12); they
/// layer over whatever palette the pane paints with and reset via OSC
/// 104/110-112 or a hard reset. ansi remaps are sparse — theme scripts touch
/// ~22 entries, never hundreds
#[derive(Default)]
pub struct DynColors {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub cursor: Option<Rgb>,
    ansi: Vec<(u8, Rgb)>,
}

impl DynColors {
    pub fn any(&self) -> bool {
        self.fg.is_some() || self.bg.is_some() || self.cursor.is_some() || !self.ansi.is_empty()
    }

    pub fn set_ansi(&mut self, n: u8, c: Rgb) {
        if let Some(e) = self.ansi.iter_mut().find(|(i, _)| *i == n) {
            e.1 = c;
        } else {
            self.ansi.push((n, c));
        }
    }

    pub fn ansi(&self, n: u8) -> Option<Rgb> {
        self.ansi.iter().find(|(i, _)| *i == n).map(|&(_, c)| c)
    }

    /// None resets every remapped entry (OSC 104 bare), Some(n) just that one
    pub fn reset_ansi(&mut self, n: Option<u8>) {
        match n {
            None => self.ansi.clear(),
            Some(n) => self.ansi.retain(|(i, _)| *i != n),
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// full palette: chrome ladder + terminal colors for one theme
#[derive(Clone)]
pub struct Palette {
    // chrome ladder (dark→light for dark themes)
    pub ink0: Rgb,
    pub ink1: Rgb,
    pub ink3: Rgb,
    pub ink4: Rgb,
    pub rule: Rgb,
    pub rule2: Rgb,
    pub mute: Rgb,
    pub text2: Rgb,
    pub paper: Rgb, // high-contrast accent (white on dark, dark on light, ember on koi)
    // terminal content
    pub fg: Rgb,
    pub bg: Rgb,
    pub bg2: Rgb,  // gradient companion to bg (subtle vertical wash)
    pub cursor: Rgb,
    pub sel: Rgb,  // text-selection tint
    ansi: [Rgb; 256],
}

impl Palette {
    /// clone with a program's dynamic OSC overrides applied. the gradient
    /// companion follows an overridden bg so the pane wash reads flat
    pub fn with_dyn(&self, d: &DynColors) -> Palette {
        let mut p = self.clone();
        if let Some(c) = d.fg {
            p.fg = c;
        }
        if let Some(c) = d.bg {
            p.bg = c;
            p.bg2 = c;
        }
        if let Some(c) = d.cursor {
            p.cursor = c;
        }
        for &(i, c) in &d.ansi {
            p.ansi[i as usize] = c;
        }
        p
    }

    pub fn from_theme(id: ThemeId) -> Self {
        match id {
            ThemeId::Instrument => Self::instrument(),
            ThemeId::Koi => Self::koi(),
            ThemeId::Paper => Self::paper(),
            ThemeId::Catppuccin => Self::catppuccin(),
            ThemeId::Gruvbox => Self::gruvbox(),
            ThemeId::TokyoNight => Self::tokyo_night(),
            ThemeId::Nord => Self::nord(),
        }
    }

    pub fn resolve_fg(&self, c: Color) -> Rgb {
        match c {
            Color::Default => self.fg,
            Color::DefaultBg => self.bg,
            Color::Indexed(i) => self.ansi[i as usize],
            Color::Rgb(r, g, b) => Rgb::new(r, g, b),
        }
    }

    pub fn resolve_bg(&self, c: Color) -> Rgb {
        match c {
            // a plain Default in the bg slot only happens via the SGR 7
            // inverse swap, so it names the theme foreground — mapping it to
            // bg painted inverse default text invisibly bg-on-bg
            Color::Default => self.fg,
            Color::DefaultBg => self.bg,
            Color::Indexed(i) => self.ansi[i as usize],
            Color::Rgb(r, g, b) => Rgb::new(r, g, b),
        }
    }

    /// bold text in a basic ansi color (0-7) maps to its bright variant (8-15)
    /// when enabled — the common xterm "bold = bright" behavior
    pub fn bold_bright(fg: Color, bold: bool, enabled: bool) -> Color {
        if enabled
            && bold
            && let Color::Indexed(i) = fg
            && i < 8
        {
            Color::Indexed(i + 8)
        } else {
            fg
        }
    }

    pub fn ansi_color(&self, i: u8) -> Rgb {
        self.ansi[i as usize]
    }

    /// patch palette fields by name from user color overrides (ansi0..ansi255
    /// target the 256-color table); unknown keys are ignored
    pub fn apply_overrides(&mut self, overrides: &[(String, Rgb)]) {
        for (k, c) in overrides {
            let c = *c;
            match k.as_str() {
                "fg" => self.fg = c,
                "bg" => self.bg = c,
                "bg2" => self.bg2 = c,
                "cursor" => self.cursor = c,
                "sel" => self.sel = c,
                "paper" => self.paper = c,
                "mute" => self.mute = c,
                "text2" => self.text2 = c,
                "ink0" => self.ink0 = c,
                "ink1" => self.ink1 = c,
                "ink3" => self.ink3 = c,
                "ink4" => self.ink4 = c,
                "rule" => self.rule = c,
                "rule2" => self.rule2 = c,
                _ => {
                    if let Some(n) = k.strip_prefix("ansi").and_then(|n| n.parse::<usize>().ok())
                        && n < 256
                    {
                        self.ansi[n] = c;
                    }
                }
            }
        }
    }

    fn instrument() -> Self {
        // restrained "instrument panel" signal colors on a hard greyscale ground:
        // clay / sage / brass / steel / mauve / teal, paired normal→bright
        let base16 = [
            Rgb::new(0x1a, 0x1a, 0x1a),
            Rgb::new(0xbf, 0x63, 0x60),
            Rgb::new(0x83, 0xa0, 0x6d),
            Rgb::new(0xc6, 0xa6, 0x67),
            Rgb::new(0x64, 0x86, 0xa6),
            Rgb::new(0xa0, 0x74, 0x8f),
            Rgb::new(0x66, 0xa3, 0xa0),
            Rgb::new(0xc8, 0xc8, 0xc8),
            Rgb::new(0x4f, 0x4f, 0x4f),
            Rgb::new(0xe0, 0x8a, 0x86),
            Rgb::new(0x9d, 0xbe, 0x86),
            Rgb::new(0xe6, 0xcd, 0x86),
            Rgb::new(0x8a, 0xa8, 0xcc),
            Rgb::new(0xc5, 0x96, 0xb4),
            Rgb::new(0x84, 0xc6, 0xc2),
            Rgb::new(0xf0, 0xf0, 0xf0),
        ];
        use crate::instrument as i;
        Palette {
            ink0: Rgb::from_triple(i::INK0),
            ink1: Rgb::from_triple(i::INK1),
            ink3: Rgb::from_triple(i::INK3),
            ink4: Rgb::from_triple(i::INK4),
            rule: Rgb::from_triple(i::RULE),
            rule2: Rgb::from_triple(i::RULE2),
            mute: Rgb::from_triple(i::MUTE),
            text2: Rgb::from_triple(i::TEXT2),
            paper: Rgb::from_triple(i::PAPER),
            fg: Rgb::from_triple(i::FG),
            bg: Rgb::from_triple(i::BG),
            bg2: Rgb::new(0x0d, 0x0e, 0x10),
            cursor: Rgb::new(0xf5, 0xf5, 0xf5),
            sel: Rgb::new(0x35, 0x52, 0x7a),
            ansi: fill_ansi(base16),
        }
    }

    fn koi() -> Self {
        // warm ember world: every hue pulled toward the fire, greens olive,
        // blues dusty, so the ff5b22 accent feels native
        let base16 = [
            Rgb::new(0x24, 0x1e, 0x16),
            Rgb::new(0xdc, 0x5a, 0x36),
            Rgb::new(0x7c, 0x96, 0x56),
            Rgb::new(0xd4, 0x9a, 0x3f),
            Rgb::new(0x5f, 0x82, 0x90),
            Rgb::new(0xa9, 0x72, 0x8c),
            Rgb::new(0x4f, 0x9b, 0x95),
            Rgb::new(0xd9, 0xce, 0xb9),
            Rgb::new(0x58, 0x4d, 0x3e),
            Rgb::new(0xff, 0x6a, 0x33),
            Rgb::new(0x99, 0xb5, 0x6e),
            Rgb::new(0xe8, 0xbe, 0x63),
            Rgb::new(0x84, 0xa0, 0xad),
            Rgb::new(0xc7, 0x94, 0xa6),
            Rgb::new(0x6c, 0xb0, 0xa8),
            Rgb::new(0xee, 0xe3, 0xce),
        ];
        Palette {
            ink0: Rgb::new(0x0a, 0x08, 0x06),
            ink1: Rgb::new(0x14, 0x11, 0x0d),
            ink3: Rgb::new(0x22, 0x1d, 0x17),
            ink4: Rgb::new(0x2e, 0x27, 0x1f),
            rule: Rgb::new(0x2e, 0x28, 0x20),
            rule2: Rgb::new(0x40, 0x37, 0x2c),
            mute: Rgb::new(0x8a, 0x7d, 0x6a),
            text2: Rgb::new(0xec, 0xe2, 0xcd),
            paper: Rgb::new(0xff, 0x5b, 0x22),
            fg: Rgb::new(0xd8, 0xcd, 0xb8),
            bg: Rgb::new(0x1a, 0x16, 0x12),
            bg2: Rgb::new(0x12, 0x0d, 0x09),
            cursor: Rgb::new(0xff, 0x5b, 0x22),
            sel: Rgb::new(0x6e, 0x46, 0x2a),
            ansi: fill_ansi(base16),
        }
    }

    fn paper() -> Self {
        // printed-ink colors on warm paper: vermilion / moss / ochre / indigo /
        // plum / pine, saturated enough to read as ink, not pastel
        let base16 = [
            Rgb::new(0x39, 0x34, 0x2b),
            Rgb::new(0xa8, 0x3a, 0x28),
            Rgb::new(0x4f, 0x6e, 0x34),
            Rgb::new(0x9a, 0x6e, 0x1c),
            Rgb::new(0x35, 0x60, 0x8a),
            Rgb::new(0x85, 0x46, 0x70),
            Rgb::new(0x2f, 0x6e, 0x6a),
            Rgb::new(0x4a, 0x46, 0x3e),
            Rgb::new(0x6a, 0x64, 0x56),
            Rgb::new(0xc2, 0x4a, 0x30),
            Rgb::new(0x5f, 0x85, 0x40),
            Rgb::new(0xa4, 0x78, 0x1f),
            Rgb::new(0x3f, 0x6e, 0xa0),
            Rgb::new(0x9a, 0x56, 0x80),
            Rgb::new(0x3a, 0x84, 0x80),
            Rgb::new(0x26, 0x20, 0x19),
        ];
        Palette {
            // on a light theme the "ink ladder" runs light→darker for chrome surfaces
            ink0: Rgb::new(0xd2, 0xcb, 0xbc), // statusbar
            ink1: Rgb::new(0xde, 0xd8, 0xcb), // titlebar
            ink3: Rgb::new(0xdd, 0xd6, 0xc7),
            ink4: Rgb::new(0xd0, 0xc8, 0xb6),
            rule: Rgb::new(0xc4, 0xbb, 0xa6),
            rule2: Rgb::new(0xb0, 0xa6, 0x8e),
            mute: Rgb::new(0x8a, 0x80, 0x6c),
            text2: Rgb::new(0x1c, 0x1a, 0x14),
            paper: Rgb::new(0x1c, 0x1a, 0x14), // dark accent on light
            fg: Rgb::new(0x33, 0x30, 0x2a),
            bg: Rgb::new(0xe8, 0xe3, 0xd6),
            bg2: Rgb::new(0xde, 0xd7, 0xc6),
            cursor: Rgb::new(0xb5, 0x53, 0x2a),
            sel: Rgb::new(0x7b, 0x9c, 0xd0),
            ansi: fill_ansi(base16),
        }
    }
}

impl Palette {
    fn catppuccin() -> Self {
        // catppuccin mocha, the canonical terminal mapping: surfaces for the
        // dim pair, subtext for white, lavender as the accent
        let base16 = [
            Rgb::new(0x45, 0x47, 0x5a),
            Rgb::new(0xf3, 0x8b, 0xa8),
            Rgb::new(0xa6, 0xe3, 0xa1),
            Rgb::new(0xf9, 0xe2, 0xaf),
            Rgb::new(0x89, 0xb4, 0xfa),
            Rgb::new(0xf5, 0xc2, 0xe7),
            Rgb::new(0x94, 0xe2, 0xd5),
            Rgb::new(0xba, 0xc2, 0xde),
            Rgb::new(0x58, 0x5b, 0x70),
            Rgb::new(0xf3, 0x8b, 0xa8),
            Rgb::new(0xa6, 0xe3, 0xa1),
            Rgb::new(0xf9, 0xe2, 0xaf),
            Rgb::new(0x89, 0xb4, 0xfa),
            Rgb::new(0xf5, 0xc2, 0xe7),
            Rgb::new(0x94, 0xe2, 0xd5),
            Rgb::new(0xa6, 0xad, 0xc8),
        ];
        Palette {
            ink0: Rgb::new(0x11, 0x11, 0x1b), // crust
            ink1: Rgb::new(0x18, 0x18, 0x25), // mantle
            ink3: Rgb::new(0x31, 0x32, 0x44), // surface0
            ink4: Rgb::new(0x45, 0x47, 0x5a), // surface1
            rule: Rgb::new(0x31, 0x32, 0x44),
            rule2: Rgb::new(0x45, 0x47, 0x5a),
            mute: Rgb::new(0x6c, 0x70, 0x86), // overlay0
            text2: Rgb::new(0xcd, 0xd6, 0xf4),
            paper: Rgb::new(0xb4, 0xbe, 0xfe), // lavender
            fg: Rgb::new(0xcd, 0xd6, 0xf4),
            bg: Rgb::new(0x1e, 0x1e, 0x2e),
            bg2: Rgb::new(0x18, 0x18, 0x25),
            cursor: Rgb::new(0xf5, 0xe0, 0xdc), // rosewater
            sel: Rgb::new(0x58, 0x5b, 0x70),
            ansi: fill_ansi(base16),
        }
    }

    fn gruvbox() -> Self {
        // gruvbox dark medium; the orange accent is the theme's signature
        let base16 = [
            Rgb::new(0x28, 0x28, 0x28),
            Rgb::new(0xcc, 0x24, 0x1d),
            Rgb::new(0x98, 0x97, 0x1a),
            Rgb::new(0xd7, 0x99, 0x21),
            Rgb::new(0x45, 0x85, 0x88),
            Rgb::new(0xb1, 0x62, 0x86),
            Rgb::new(0x68, 0x9d, 0x6a),
            Rgb::new(0xa8, 0x99, 0x84),
            Rgb::new(0x92, 0x83, 0x74),
            Rgb::new(0xfb, 0x49, 0x34),
            Rgb::new(0xb8, 0xbb, 0x26),
            Rgb::new(0xfa, 0xbd, 0x2f),
            Rgb::new(0x83, 0xa5, 0x98),
            Rgb::new(0xd3, 0x86, 0x9b),
            Rgb::new(0x8e, 0xc0, 0x7c),
            Rgb::new(0xeb, 0xdb, 0xb2),
        ];
        Palette {
            ink0: Rgb::new(0x14, 0x16, 0x17),
            ink1: Rgb::new(0x1d, 0x20, 0x21), // bg0_h
            ink3: Rgb::new(0x32, 0x30, 0x2f), // bg1
            ink4: Rgb::new(0x3c, 0x38, 0x36), // bg2
            rule: Rgb::new(0x3c, 0x38, 0x36),
            rule2: Rgb::new(0x50, 0x49, 0x45),
            mute: Rgb::new(0x92, 0x83, 0x74),
            text2: Rgb::new(0xfb, 0xf1, 0xc7),
            paper: Rgb::new(0xfe, 0x80, 0x19), // orange
            fg: Rgb::new(0xeb, 0xdb, 0xb2),
            bg: Rgb::new(0x28, 0x28, 0x28),
            bg2: Rgb::new(0x1f, 0x21, 0x21),
            cursor: Rgb::new(0xeb, 0xdb, 0xb2),
            sel: Rgb::new(0x64, 0x59, 0x53),
            ansi: fill_ansi(base16),
        }
    }

    fn tokyo_night() -> Self {
        // tokyo night (storm-leaning darks for the chrome ladder)
        let base16 = [
            Rgb::new(0x15, 0x16, 0x1e),
            Rgb::new(0xf7, 0x76, 0x8e),
            Rgb::new(0x9e, 0xce, 0x6a),
            Rgb::new(0xe0, 0xaf, 0x68),
            Rgb::new(0x7a, 0xa2, 0xf7),
            Rgb::new(0xbb, 0x9a, 0xf7),
            Rgb::new(0x7d, 0xcf, 0xff),
            Rgb::new(0xa9, 0xb1, 0xd6),
            Rgb::new(0x41, 0x48, 0x68),
            Rgb::new(0xf7, 0x76, 0x8e),
            Rgb::new(0x9e, 0xce, 0x6a),
            Rgb::new(0xe0, 0xaf, 0x68),
            Rgb::new(0x7a, 0xa2, 0xf7),
            Rgb::new(0xbb, 0x9a, 0xf7),
            Rgb::new(0x7d, 0xcf, 0xff),
            Rgb::new(0xc0, 0xca, 0xf5),
        ];
        Palette {
            ink0: Rgb::new(0x0f, 0x0f, 0x17),
            ink1: Rgb::new(0x16, 0x16, 0x1e), // bg_dark
            ink3: Rgb::new(0x1f, 0x23, 0x35),
            ink4: Rgb::new(0x29, 0x2e, 0x42),
            rule: Rgb::new(0x29, 0x2e, 0x42),
            rule2: Rgb::new(0x3b, 0x42, 0x61),
            mute: Rgb::new(0x56, 0x5f, 0x89),
            text2: Rgb::new(0xc0, 0xca, 0xf5),
            paper: Rgb::new(0x7a, 0xa2, 0xf7), // the signature blue
            fg: Rgb::new(0xc0, 0xca, 0xf5),
            bg: Rgb::new(0x1a, 0x1b, 0x26),
            bg2: Rgb::new(0x16, 0x16, 0x1e),
            cursor: Rgb::new(0xc0, 0xca, 0xf5),
            sel: Rgb::new(0x38, 0x4f, 0x8d),
            ansi: fill_ansi(base16),
        }
    }

    fn nord() -> Self {
        // nord: polar night ground, snow storm text, frost accent
        let base16 = [
            Rgb::new(0x3b, 0x42, 0x52),
            Rgb::new(0xbf, 0x61, 0x6a),
            Rgb::new(0xa3, 0xbe, 0x8c),
            Rgb::new(0xeb, 0xcb, 0x8b),
            Rgb::new(0x81, 0xa1, 0xc1),
            Rgb::new(0xb4, 0x8e, 0xad),
            Rgb::new(0x88, 0xc0, 0xd0),
            Rgb::new(0xe5, 0xe9, 0xf0),
            Rgb::new(0x4c, 0x56, 0x6a),
            Rgb::new(0xbf, 0x61, 0x6a),
            Rgb::new(0xa3, 0xbe, 0x8c),
            Rgb::new(0xeb, 0xcb, 0x8b),
            Rgb::new(0x81, 0xa1, 0xc1),
            Rgb::new(0xb4, 0x8e, 0xad),
            Rgb::new(0x8f, 0xbc, 0xbb),
            Rgb::new(0xec, 0xef, 0xf4),
        ];
        Palette {
            ink0: Rgb::new(0x23, 0x28, 0x31),
            ink1: Rgb::new(0x2a, 0x2f, 0x3a),
            ink3: Rgb::new(0x3b, 0x42, 0x52),
            ink4: Rgb::new(0x43, 0x4c, 0x5e),
            rule: Rgb::new(0x3b, 0x42, 0x52),
            rule2: Rgb::new(0x4c, 0x56, 0x6a),
            mute: Rgb::new(0x61, 0x6e, 0x88),
            text2: Rgb::new(0xe5, 0xe9, 0xf0),
            paper: Rgb::new(0x88, 0xc0, 0xd0), // frost
            fg: Rgb::new(0xd8, 0xde, 0xe9),
            bg: Rgb::new(0x2e, 0x34, 0x40),
            bg2: Rgb::new(0x29, 0x2e, 0x39),
            cursor: Rgb::new(0xd8, 0xde, 0xe9),
            sel: Rgb::new(0x5a, 0x66, 0x7e),
            ansi: fill_ansi(base16),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::instrument()
    }
}

fn fill_ansi(base16: [Rgb; 16]) -> [Rgb; 256] {
    let mut ansi = [Rgb::new(0, 0, 0); 256];
    ansi[..16].copy_from_slice(&base16);
    // 216-color cube (16..232)
    let steps = [0u8, 95, 135, 175, 215, 255];
    let mut i = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                ansi[i] = Rgb::new(steps[r], steps[g], steps[b]);
                i += 1;
            }
        }
    }
    // grayscale ramp (232..256)
    for j in 0..24 {
        let v = 8 + j as u8 * 10;
        ansi[232 + j] = Rgb::new(v, v, v);
    }
    ansi
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(a: Rgb, b: Rgb) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn min_contrast_lifts_failing_pairs_to_the_ratio() {
        // dark gray on black: lifted lighter until the ratio holds
        let bg = Rgb::new(10, 10, 12);
        let fg = Rgb::new(60, 60, 60);
        assert!(ratio(fg, bg) < 3.0);
        let fixed = apply_min_contrast(fg, bg, 3.0);
        assert!(ratio(fixed, bg) >= 2.95, "got {}", ratio(fixed, bg));
        // light gray on white: pushed darker instead
        let bg = Rgb::new(250, 250, 250);
        let fg = Rgb::new(220, 220, 225);
        let fixed = apply_min_contrast(fg, bg, 4.5);
        assert!(ratio(fixed, bg) >= 4.4, "got {}", ratio(fixed, bg));
        assert!(luminance(fixed) < luminance(bg));
    }

    #[test]
    fn min_contrast_leaves_passing_pairs_untouched() {
        let bg = Rgb::new(20, 20, 24);
        let fg = Rgb::new(220, 215, 200);
        assert_eq!(apply_min_contrast(fg, bg, 4.5), fg);
    }

    #[test]
    fn min_contrast_flips_direction_when_polarity_cant_reach() {
        // white on white: lightening is a dead end, so it goes dark
        let bg = Rgb::new(255, 255, 255);
        let fg = Rgb::new(255, 255, 255);
        let fixed = apply_min_contrast(fg, bg, 3.0);
        assert!(ratio(fixed, bg) >= 2.95, "got {}", ratio(fixed, bg));
        // an unreachable ratio on a mid bg lands on the stronger extreme
        let bg = Rgb::new(128, 128, 128);
        let fixed = apply_min_contrast(bg, bg, 21.0);
        assert!(fixed == Rgb::new(0, 0, 0) || fixed == Rgb::new(255, 255, 255));
    }

    #[test]
    fn inverse_default_cell_resolves_to_visible_colors() {
        // SGR 7 swaps the cell's fg/bg slots, so a default-colored cell puts
        // Color::Default in the bg slot: it must resolve to the theme fg
        let pal = Palette::from_theme(ThemeId::Instrument);
        assert_eq!(pal.resolve_bg(Color::Default), pal.fg);
        assert_eq!(pal.resolve_fg(Color::DefaultBg), pal.bg);
        assert_ne!(pal.resolve_bg(Color::Default), pal.resolve_fg(Color::DefaultBg));
    }

    #[test]
    fn bold_bright_promotes_indexed_0_7_only_when_enabled() {
        // bold + enabled: the 8 base ansi colors promote to their bright pair (+8)
        assert_eq!(Palette::bold_bright(Color::Indexed(1), true, true), Color::Indexed(9));
        assert_eq!(Palette::bold_bright(Color::Indexed(7), true, true), Color::Indexed(15));
        // already-bright (>=8) is left alone
        assert_eq!(Palette::bold_bright(Color::Indexed(9), true, true), Color::Indexed(9));
        // not bold, disabled, or non-indexed: unchanged
        assert_eq!(Palette::bold_bright(Color::Indexed(1), false, true), Color::Indexed(1));
        assert_eq!(Palette::bold_bright(Color::Indexed(1), true, false), Color::Indexed(1));
        assert_eq!(Palette::bold_bright(Color::Default, true, true), Color::Default);
    }

    #[test]
    fn every_theme_keeps_text_and_selection_readable() {
        for id in ThemeId::ALL {
            let p = Palette::from_theme(id);
            let name = id.name();
            assert!(ratio(p.fg, p.bg) >= 7.0, "{name}: body text");
            // selection is painted at 0.9 over the background
            let sel = p.bg.lerp(p.sel, 0.9);
            assert!(ratio(sel, p.bg) >= 2.0, "{name}: selection reads against the background");
            assert!(ratio(p.fg, sel) >= 4.5, "{name}: text stays readable on the selection");
            // secondary chrome text is lifted to 4.5:1 on each surface it sits on
            for surface in [p.ink0, p.ink1, p.bg] {
                let lifted = apply_min_contrast(p.mute, surface, crate::render::tokens::TEXT_CONTRAST);
                assert!(ratio(lifted, surface) >= 4.5, "{name}: muted text");
            }
        }
    }

    #[test]
    fn house_themes_keep_ansi_colors_legible() {
        // third-party themes keep their published colors; min_contrast covers them
        for id in [ThemeId::Instrument, ThemeId::Koi, ThemeId::Paper] {
            let p = Palette::from_theme(id);
            for n in (1..8).chain(9..16) {
                assert!(ratio(p.ansi_color(n), p.bg) >= 3.0, "{} ansi {n}", id.name());
            }
        }
    }
}
