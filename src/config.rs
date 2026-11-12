//! config.conf: the key=value settings file. parse never fails (a bad line
//! keeps its default and warns), and render writes the whole file back

#[cfg(windows)]
use crate::parse_quake_key;
#[cfg(target_os = "linux")]
use crate::quake_portal_trigger;
use crate::{
    CONTENT_PT, CloseAction, MAX_BOOKMARKS, MAX_PROFILE_ENV, MAX_PROFILE_RAW_LINES, MAX_SHELL_THEMES,
    MAX_SNIPPETS, MAX_STARTUP_TEXT_BYTES, RightClick, ShellKind, color, config_path, grid, profile_mut, pty,
    read_text, render, same_location, snippet_entry, split_cmdline,
};

/// all persisted settings (App-owned + renderer-owned), read at startup
#[derive(Clone)]
pub struct Persisted {
    pub scrollback: usize,
    pub copy_on_select: bool,
    pub shell: ShellKind,
    pub load_profile: bool,
    pub close_action: CloseAction,
    pub right_click: RightClick,
    pub backend: render::BackendChoice,
    pub restore_on_launch: bool,
    /// bring each pane's last lines back with the restored layout
    pub restore_contents: bool,
    pub font_size: f32,
    pub padding: f32,
    pub cursor: grid::CursorShape,
    pub cursor_blink: bool,
    pub bold_as_bright: bool,
    pub line_height: f32,
    pub theme: color::ThemeId,
    /// `theme=auto`: follow the OS light/dark setting, resolving to
    /// theme_dark / theme_light (which only matter while auto is on)
    pub theme_auto: bool,
    pub theme_dark: color::ThemeId,
    pub theme_light: color::ThemeId,
    pub font: Option<String>,
    /// base weight regular text renders at (`font_weight=`, named or 100-900)
    pub font_weight: u16,
    /// minimum WCAG contrast ratio text is lifted to against its cell bg
    /// (`min_contrast=`, 1-21; 1 = off). wt's minimumContrastRatio
    pub min_contrast: f32,
    /// window background image png (`background_image=` path) drawn behind
    /// panes at `background_image_opacity=` (0..1)
    pub background_image: Option<String>,
    pub background_image_opacity: f32,
    /// shape punctuation runs through the font's ligature rules (`ligatures=`,
    /// default on; fonts without liga/calt render identically)
    pub ligatures: bool,
    /// opentype features the terminal font shapes with (`font_features=zero,
    /// ss01, -calt`); empty leaves the font's defaults
    pub font_features: String,
    pub opacity: i32,
    #[cfg(windows)]
    pub quake_key: Option<(u32, u32)>,
    #[cfg(target_os = "linux")]
    pub quake_key: Option<String>,
    /// the quake_key line as written, kept so save_config preserves its spelling
    pub quake_key_raw: Option<String>,
    /// the WSL distribution `new tab: wsl` launches (None = wsl.exe default)
    pub wsl_distro: Option<String>,
    /// run plugins inside the OS sandbox for privilege isolation: a windows
    /// appcontainer (`plugin_sandbox=appcontainer`) or a linux bwrap jail
    /// (`plugin_sandbox=bwrap`); on by default
    pub plugin_sandbox: bool,
    /// let terminal programs replace the OS clipboard through OSC 52
    pub osc52: bool,
    /// answer `termie @ list|send-text|new-tab|focus` from other processes of
    /// this user (`remote_control=true`); off by default
    pub remote_control: bool,
    /// system backdrop behind the window (`acrylic=true`, alias `mica`): mica
    /// on windows 11 and compositor blur on linux
    pub acrylic: bool,
    /// custom shell profiles, kept both raw as (`profile.` sub-key, value) so
    /// save_config re-emits every line the user wrote, and parsed into Profiles
    /// (argv plus optional .cwd / .env.<VAR> sub-keys)
    pub profiles_raw: Vec<(String, String)>,
    pub profiles: Vec<pty::Profile>,
    /// per-profile themes (`theme.<shell-or-profile>=<theme name>`): panes
    /// spawned as that shell/profile paint with that theme's palette while
    /// the window chrome keeps the global theme
    pub shell_themes: Vec<(String, color::ThemeId)>,
    /// paint pty output inline instead of via the request_redraw hop, shaving up
    /// to a frame of input-to-photon latency and staying tear-free under Fifo
    /// vsync; on by default; set `inline_paint=false` to use the redraw hop
    pub inline_paint: bool,
    /// draw the input-to-photon latency hud (`latency_hud=true`); off by default
    pub latency_hud: bool,
    /// daily update check against GitHub releases; `update_check=false` opts out
    pub update_check: bool,
    /// a BEL flashes the pane border, dots a background tab and flashes the
    /// taskbar; `bell=off` drops all three
    pub bell: bool,
    /// chrome text and controls size, multiplied by the os text size;
    /// 0.75 to 2.0, the terminal grid keeps font_size
    pub ui_scale: f32,
    /// value of $TERM_PROGRAM for child processes. default "termie". set to a
    /// known host name (e.g. ghostty) only for apps that refuse the kitty
    /// keyboard protocol unless the name is on their allowlist
    pub term_program: String,
    /// working directories saved from the command palette or tab menu
    pub bookmarks: Vec<String>,
    /// `snippet.<name>=<text>` lines, typed into the prompt from the palette
    pub snippets: Vec<(String, String)>,
    /// the file exists but could not be read (oversized, permission denied);
    /// saving would replace it with defaults, so it is left alone
    pub unreadable: bool,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            scrollback: 10_000,
            copy_on_select: false,
            shell: ShellKind::Auto,
            load_profile: false,
            close_action: CloseAction::Quit,
            right_click: RightClick::Menu,
            backend: render::BackendChoice::Auto,
            restore_on_launch: true,
            restore_contents: true,
            font_size: CONTENT_PT,
            padding: 6.0,
            cursor: grid::CursorShape::Block,
            cursor_blink: true,
            bold_as_bright: true,
            line_height: 1.32,
            theme: color::ThemeId::Instrument,
            theme_auto: false,
            theme_dark: color::ThemeId::Instrument,
            theme_light: color::ThemeId::Paper,
            font: None,
            font_weight: 400,
            min_contrast: 1.0,
            background_image: None,
            background_image_opacity: 0.3,
            ligatures: true,
            font_features: String::new(),
            opacity: 85,
            #[cfg(any(windows, target_os = "linux"))]
            quake_key: None,
            quake_key_raw: None,
            wsl_distro: None,
            plugin_sandbox: true,
            osc52: false,
            remote_control: false,
            acrylic: false,
            profiles_raw: Vec::new(),
            profiles: Vec::new(),
            shell_themes: Vec::new(),
            inline_paint: true,
            latency_hud: false,
            update_check: true,
            bell: true,
            ui_scale: 1.0,
            term_program: String::from("termie"),
            bookmarks: Vec::new(),
            snippets: Vec::new(),
            unreadable: false,
        }
    }
}

pub fn load() -> Persisted {
    let Some(path) = config_path() else {
        return Persisted::default();
    };
    match std::fs::File::open(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
        file => match file.ok().and_then(|file| read_text(file, MAX_STARTUP_TEXT_BYTES)) {
            Some(text) => parse(&text),
            None => {
                log::warn!("config: {} could not be read; using defaults and leaving the file untouched", path.display());
                Persisted { unreadable: true, ..Persisted::default() }
            }
        },
    }
}

/// the whole file as the settings panel writes it; opt-ins at their default
/// are left out so the file stays short
pub fn render(p: &Persisted) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "scrollback={}", p.scrollback);
    let _ = writeln!(s, "copy_on_select={}", p.copy_on_select);
    let _ = writeln!(s, "shell={}", p.shell.label());
    let _ = writeln!(s, "load_profile={}", p.load_profile);
    let _ = writeln!(s, "close_action={}", p.close_action.label());
    let _ = writeln!(s, "right_click={}", p.right_click.label());
    let _ = writeln!(s, "backend={}", p.backend.label());
    let _ = writeln!(s, "restore_on_launch={}", p.restore_on_launch);
    let _ = writeln!(s, "restore_contents={}", p.restore_contents);
    let _ = writeln!(s, "font_size={}", p.font_size);
    let _ = writeln!(s, "padding={}", p.padding);
    let _ = writeln!(s, "opacity={}", p.opacity);
    let _ = writeln!(s, "cursor={}", cursor_name(p.cursor));
    let _ = writeln!(s, "cursor_blink={}", p.cursor_blink);
    let _ = writeln!(s, "bold_as_bright={}", p.bold_as_bright);
    let _ = writeln!(s, "line_height={}", p.line_height);
    if p.theme_auto {
        let _ = writeln!(s, "theme=auto");
    } else {
        let _ = writeln!(s, "theme={}", p.theme.name());
    }
    if p.theme_dark != color::ThemeId::Instrument {
        let _ = writeln!(s, "theme_dark={}", p.theme_dark.name());
    }
    if p.theme_light != color::ThemeId::Paper {
        let _ = writeln!(s, "theme_light={}", p.theme_light.name());
    }
    if let Some(font) = &p.font {
        let _ = writeln!(s, "font={font}");
    }
    if p.font_weight != 400 {
        let _ = writeln!(s, "font_weight={}", p.font_weight);
    }
    if p.min_contrast > 1.0 {
        let _ = writeln!(s, "min_contrast={}", p.min_contrast);
    }
    if let Some(bi) = &p.background_image {
        let _ = writeln!(s, "background_image={bi}");
        let _ = writeln!(s, "background_image_opacity={}", p.background_image_opacity);
    }
    if !p.ligatures {
        let _ = writeln!(s, "ligatures=false");
    }
    if !p.font_features.is_empty() {
        let _ = writeln!(s, "font_features={}", p.font_features);
    }
    if let Some(q) = &p.quake_key_raw {
        // an opt-in the panel can't edit; dropping it here silently killed
        // the user's drop-down hotkey on every settings change
        let _ = writeln!(s, "quake_key={q}");
    }
    if let Some(d) = &p.wsl_distro {
        let _ = writeln!(s, "wsl_distro={d}");
    }
    if !p.plugin_sandbox {
        let _ = writeln!(s, "plugin_sandbox=off");
    }
    if p.osc52 {
        let _ = writeln!(s, "osc52=true");
    }
    if p.remote_control {
        let _ = writeln!(s, "remote_control=true");
    }
    if p.acrylic {
        let _ = writeln!(s, "acrylic=true");
    }
    for (name, line) in &p.profiles_raw {
        // config-file-only like quake_key: re-emit exactly as written
        let _ = writeln!(s, "profile.{name}={line}");
    }
    for (name, id) in &p.shell_themes {
        let _ = writeln!(s, "theme.{name}={}", id.name());
    }
    for path in &p.bookmarks {
        let _ = writeln!(s, "bookmark={path}");
    }
    for (name, text) in &p.snippets {
        let _ = writeln!(s, "snippet.{name}={text}");
    }
    if p.term_program != "termie" {
        let _ = writeln!(s, "term_program={}", p.term_program);
    }
    if !p.inline_paint {
        let _ = writeln!(s, "inline_paint=false");
    }
    if p.latency_hud {
        let _ = writeln!(s, "latency_hud=true");
    }
    if !p.update_check {
        let _ = writeln!(s, "update_check=false");
    }
    if !p.bell {
        let _ = writeln!(s, "bell=off");
    }
    if p.ui_scale != 1.0 {
        let _ = writeln!(s, "ui_scale={}", p.ui_scale);
    }
    s
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn set_bool(slot: &mut bool, key: &str, v: &str) {
    match parse_bool(v) {
        Some(b) => *slot = b,
        None => log::warn!("config: `{key}={v}` is neither on nor off; kept {slot}"),
    }
}

fn record_profile_line(persisted: &mut Persisted, key: &str, value: &str) {
    if persisted.profiles_raw.len() < MAX_PROFILE_RAW_LINES {
        persisted.profiles_raw.push((key.to_string(), value.to_string()));
    }
}

/// parse the key=value config text into Persisted, leaving defaults for any key
/// absent or malformed; split out from load so the settings parser is
/// unit-testable without touching %APPDATA%
pub fn parse(text: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "scrollback" => {
                if let Ok(n) = v.parse::<usize>() {
                    // same ceiling as the settings control: history is stored
                    // as full-width lines, so an uncapped value is an OOM knob
                    if n > 100_000 {
                        log::warn!("scrollback={n} clamped to 100000");
                    }
                    p.scrollback = n.min(100_000);
                }
            }
            "copy_on_select" => set_bool(&mut p.copy_on_select, k, v),
            "shell" => p.shell = ShellKind::from_label(v),
            "load_profile" => set_bool(&mut p.load_profile, k, v),
            "close_action" => p.close_action = CloseAction::from_label(v),
            "right_click" => p.right_click = RightClick::from_label(v),
            "backend" => p.backend = render::BackendChoice::from_label(v),
            "restore_on_launch" => set_bool(&mut p.restore_on_launch, k, v),
            "restore_contents" => set_bool(&mut p.restore_contents, k, v),
            "font_size" => {
                if let Ok(n) = v.parse::<f32>()
                    && n.is_finite()
                {
                    p.font_size = n;
                }
            }
            "padding" => {
                if let Ok(n) = v.parse::<f32>()
                    && n.is_finite()
                {
                    p.padding = n;
                }
            }
            "opacity" => {
                if let Ok(n) = v.parse() {
                    p.opacity = n;
                }
            }
            "cursor" => p.cursor = cursor_from_name(v),
            "cursor_blink" => set_bool(&mut p.cursor_blink, k, v),
            "bold_as_bright" => set_bool(&mut p.bold_as_bright, k, v),
            "line_height" => {
                if let Ok(x) = v.parse::<f32>()
                    && x.is_finite()
                {
                    p.line_height = x;
                }
            }
            "ui_scale" => {
                if let Ok(x) = v.parse::<f32>()
                    && x.is_finite()
                {
                    p.ui_scale = x.clamp(0.75, 2.0);
                }
            }
            "theme" => {
                if v.eq_ignore_ascii_case("auto") {
                    p.theme_auto = true;
                } else {
                    p.theme = color::ThemeId::from_name(v);
                    p.theme_auto = false;
                }
            }
            "theme_dark" => p.theme_dark = color::ThemeId::from_name(v),
            "theme_light" => p.theme_light = color::ThemeId::from_name(v),
            "font" => {
                if !v.is_empty() {
                    p.font = Some(v.to_string());
                }
            }
            "font_weight" => {
                if let Some(w) = font_weight_from_label(v) {
                    p.font_weight = w;
                }
            }
            "min_contrast" => {
                if let Ok(x) = v.parse::<f32>()
                    && x.is_finite()
                {
                    p.min_contrast = x.clamp(1.0, 21.0);
                }
            }
            "ligatures" => set_bool(&mut p.ligatures, k, v),
            "font_features" => p.font_features = v.to_string(),
            "background_image" => p.background_image = (!v.is_empty()).then(|| v.to_string()),
            "background_image_opacity" => {
                if let Ok(x) = v.parse::<f32>()
                    && x.is_finite()
                {
                    p.background_image_opacity = x.clamp(0.0, 1.0);
                }
            }
            "quake_key" => {
                p.quake_key_raw = (!v.is_empty()).then(|| v.to_string());
                #[cfg(windows)]
                {
                    p.quake_key = parse_quake_key(v);
                    if p.quake_key.is_none() {
                        p.quake_key_raw = None;
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    p.quake_key = quake_portal_trigger(v);
                    if p.quake_key.is_none() {
                        p.quake_key_raw = None;
                    }
                }
            }
            "wsl_distro" => {
                if !v.is_empty() {
                    p.wsl_distro = Some(v.to_string());
                }
            }
            // a misspelled value keeps the sandbox on rather than dropping it
            "plugin_sandbox" => {
                p.plugin_sandbox = matches!(v, "appcontainer" | "bwrap") || parse_bool(v).unwrap_or_else(|| {
                    log::warn!("config: `plugin_sandbox={v}` is not a known value; the sandbox stays on");
                    true
                })
            }
            "osc52" => set_bool(&mut p.osc52, k, v),
            "remote_control" => set_bool(&mut p.remote_control, k, v),
            "acrylic" | "mica" => set_bool(&mut p.acrylic, k, v),
            "inline_paint" => set_bool(&mut p.inline_paint, k, v),
            "latency_hud" => set_bool(&mut p.latency_hud, k, v),
            "update_check" => set_bool(&mut p.update_check, k, v),
            "bell" => set_bool(&mut p.bell, k, v),
            "term_program" => {
                // empty falls back to the default at spawn time
                if !v.is_empty() {
                    p.term_program = v.to_string();
                }
            }
            "bookmark" => {
                if !v.is_empty()
                    && p.bookmarks.len() < MAX_BOOKMARKS
                    && !p.bookmarks.iter().any(|path| same_location(path, v))
                {
                    p.bookmarks.push(v.to_string());
                }
            }
            other => {
                if let Some(name) = other.strip_prefix("snippet.") {
                    match snippet_entry(name, v) {
                        Some(entry) if p.snippets.len() < MAX_SNIPPETS => p.snippets.push(entry),
                        Some(_) => {}
                        None => log::warn!("config: snippet line `{other}` needs a name and plain text"),
                    }
                } else if let Some(name) = other.strip_prefix("theme.") {
                    if !name.is_empty() && !v.is_empty() {
                        if p.shell_themes.len() < MAX_SHELL_THEMES {
                            p.shell_themes.push((name.to_string(), color::ThemeId::from_name(v)));
                        }
                    } else {
                        log::warn!("config: per-profile theme line `{other}` needs a name and a theme");
                    }
                } else if let Some(rest) = other.strip_prefix("profile.") {
                    // profile.<name>=<argv>, profile.<name>.cwd=<dir>,
                    // profile.<name>.env.<VAR>=<value>; sub-keys attach to the
                    // profile of that name whatever order the lines appear in
                    if let Some(name) = rest.strip_suffix(".cwd") {
                        if !name.is_empty() && !v.is_empty() {
                            if let Some(profile) = profile_mut(&mut p.profiles, name) {
                                profile.cwd = Some(v.to_string());
                                record_profile_line(&mut p, rest, v);
                            }
                        } else {
                            log::warn!("config: profile line `{other}` needs a name and a directory");
                        }
                    } else if let Some((name, var)) = rest.split_once(".env.") {
                        if !name.is_empty() && !var.is_empty() {
                            if let Some(profile) = profile_mut(&mut p.profiles, name)
                                && profile.env.len() < MAX_PROFILE_ENV
                            {
                                profile.env.push((var.to_string(), v.to_string()));
                                record_profile_line(&mut p, rest, v);
                            }
                        } else {
                            log::warn!("config: profile env line `{other}` needs a name and a variable");
                        }
                    } else {
                        let argv = split_cmdline(v);
                        if !rest.is_empty() && !argv.is_empty() {
                            if let Some(profile) = profile_mut(&mut p.profiles, rest) {
                                profile.argv = argv;
                                record_profile_line(&mut p, rest, v);
                            }
                        } else {
                            log::warn!("config: profile line `{other}` needs a name and a command");
                        }
                    }
                } else {
                    // a typo used to be discarded with zero feedback
                    log::warn!("config: unknown key `{other}` ignored");
                }
            }
        }
    }
    // a .cwd / .env line for a profile with no command line leaves an argv-less
    // entry: drop it (it must never reach the spawn path) and say so, like the
    // other malformed-config warns
    p.profiles.retain(|pr| {
        if pr.argv.is_empty() {
            log::warn!("config: profile `{}` has .cwd/.env lines but no profile.{}=<command>; ignored", pr.name, pr.name);
        }
        !pr.argv.is_empty()
    });
    p
}

/// css/opentype weight, named or numeric; None leaves the setting untouched
pub fn font_weight_from_label(v: &str) -> Option<u16> {
    let named = match v.to_ascii_lowercase().as_str() {
        "thin" => 100,
        "extralight" => 200,
        "light" => 300,
        "normal" | "regular" => 400,
        "medium" => 500,
        "semibold" => 600,
        "bold" => 700,
        "extrabold" => 800,
        "black" => 900,
        _ => return v.parse::<u16>().ok().map(|n| n.clamp(100, 900)),
    };
    Some(named)
}

pub fn cursor_from_name(s: &str) -> grid::CursorShape {
    match s {
        "block" => grid::CursorShape::Block,
        "underline" => grid::CursorShape::Underline,
        _ => grid::CursorShape::Bar,
    }
}


pub fn cursor_name(shape: grid::CursorShape) -> &'static str {
    match shape {
        grid::CursorShape::Bar => "beam",
        grid::CursorShape::Block => "block",
        grid::CursorShape::Underline => "underline",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_CUSTOM_PROFILES;

    #[test]
    fn config_parses_feature_flags_and_aliases() {
        // the sandbox is on unless explicitly disabled
        assert!(parse("plugin_sandbox=appcontainer").plugin_sandbox);
        assert!(parse("plugin_sandbox=on").plugin_sandbox);
        assert!(parse("plugin_sandbox=true").plugin_sandbox);
        assert!(!parse("plugin_sandbox=off").plugin_sandbox);
        assert!(Persisted::default().plugin_sandbox);
        assert!(!Persisted::default().osc52);
        assert!(!Persisted::default().remote_control);
        assert!(parse("remote_control=true").remote_control);
        assert!(parse("osc52=true").osc52);

        assert!(parse("inline_paint=true").inline_paint);
        assert!(parse("inline_paint=on").inline_paint);
        assert!(!parse("inline_paint=false").inline_paint);

        assert!(parse("latency_hud=true").latency_hud);
        assert!(parse("latency_hud=on").latency_hud);
        assert!(!parse("latency_hud=nope").latency_hud);

        assert_eq!(parse("wsl_distro=Ubuntu").wsl_distro.as_deref(), Some("Ubuntu"));
        // an empty value leaves the default (no distro pinned)
        assert_eq!(parse("wsl_distro=").wsl_distro, None);

        // the system backdrop accepts both spellings, off by default
        assert!(parse("acrylic=true").acrylic);
        assert!(parse("mica=on").acrylic);
        assert!(!parse("acrylic=false").acrylic);
        assert!(!Persisted::default().acrylic);
    }

    #[test]
    fn config_parses_right_click() {
        assert_eq!(parse("right_click=paste").right_click, RightClick::Paste);
        assert_eq!(parse("right_click=menu").right_click, RightClick::Menu);
        // an unknown value warns and keeps the default (menu)
        assert_eq!(parse("right_click=bogus").right_click, RightClick::Menu);
        assert_eq!(Persisted::default().right_click, RightClick::Menu);
    }

    #[test]
    fn config_parses_custom_profiles() {
        let p = parse(
            "profile.git-bash=\"C:\\Git\\bin\\bash.exe\" -i -l\nprofile.nu=nu.exe\nprofile.=broken\nprofile.empty=\n",
        );
        assert_eq!(p.profiles.len(), 2);
        assert_eq!(p.profiles[0].name, "git-bash");
        assert_eq!(p.profiles[0].argv, ["C:\\Git\\bin\\bash.exe", "-i", "-l"]);
        assert_eq!(p.profiles[1].name, "nu");
        assert_eq!(p.profiles[1].argv, ["nu.exe"]);
        // the raw lines round-trip for save_config
        assert_eq!(p.profiles_raw[1], ("nu".to_string(), "nu.exe".to_string()));
    }

    #[test]
    fn config_caps_profile_and_theme_collections() {
        let profiles = (0..=MAX_CUSTOM_PROFILES)
            .map(|i| format!("profile.p{i}=shell{i}\n"))
            .collect::<String>();
        let p = parse(&profiles);
        assert_eq!(p.profiles.len(), MAX_CUSTOM_PROFILES);
        assert_eq!(p.profiles_raw.len(), MAX_CUSTOM_PROFILES);

        let env = (0..=MAX_PROFILE_ENV)
            .map(|i| format!("profile.dev.env.V{i}=value{i}\n"))
            .collect::<String>();
        let p = parse(&format!("profile.dev=shell\n{env}"));
        assert_eq!(p.profiles[0].env.len(), MAX_PROFILE_ENV);

        let raw = std::iter::repeat_n("profile.dev=shell\n", MAX_PROFILE_RAW_LINES + 1).collect::<String>();
        assert_eq!(parse(&raw).profiles_raw.len(), MAX_PROFILE_RAW_LINES);

        let themes = (0..=MAX_SHELL_THEMES)
            .map(|i| format!("theme.shell{i}=nord\n"))
            .collect::<String>();
        assert_eq!(parse(&themes).shell_themes.len(), MAX_SHELL_THEMES);
    }

    #[test]
    fn config_parses_profile_cwd_and_env() {
        let p = parse(
            "profile.dev=pwsh.exe\nprofile.dev.cwd=C:\\repo\nprofile.dev.env.RUST_LOG=debug\nprofile.dev.env.API=1\n",
        );
        assert_eq!(p.profiles.len(), 1);
        let dev = &p.profiles[0];
        assert_eq!(dev.argv, ["pwsh.exe"]);
        assert_eq!(dev.cwd.as_deref(), Some("C:\\repo"));
        assert_eq!(
            dev.env,
            [("RUST_LOG".to_string(), "debug".to_string()), ("API".to_string(), "1".to_string())]
        );
        // sub-keys attach in any line order; an orphaned sub-key (no command
        // line) leaves no profile but the raw line still round-trips
        let q = parse("profile.x.cwd=C:\\a\nprofile.x=cmd.exe\nprofile.orphan.env.FOO=bar\n");
        assert_eq!(q.profiles.len(), 1);
        assert_eq!(q.profiles[0].name, "x");
        assert_eq!(q.profiles[0].cwd.as_deref(), Some("C:\\a"));
        assert!(q.profiles_raw.iter().any(|(k, v)| k == "x.cwd" && v == "C:\\a"));
        assert!(q.profiles_raw.iter().any(|(k, v)| k == "orphan.env.FOO" && v == "bar"));
    }

    #[test]
    fn config_parses_per_profile_themes() {
        let p = parse("theme=paper\ntheme.cmd=nord\ntheme.git-bash=gruvbox\ntheme.=broken\ntheme.wsl=\n");
        // the global theme key is untouched by the per-profile lines
        assert_eq!(p.theme, color::ThemeId::Paper);
        assert_eq!(
            p.shell_themes,
            [
                ("cmd".to_string(), color::ThemeId::Nord),
                ("git-bash".to_string(), color::ThemeId::Gruvbox)
            ]
        );
    }

    #[test]
    fn config_parses_theme_auto() {
        let p = parse("theme=auto\ntheme_dark=nord\ntheme_light=paper\n");
        assert!(p.theme_auto);
        assert_eq!(p.theme_dark, color::ThemeId::Nord);
        assert_eq!(p.theme_light, color::ThemeId::Paper);
        // a concrete theme after auto wins and turns auto off (last line rules)
        let p = parse("theme=auto\ntheme=koi\n");
        assert!(!p.theme_auto);
        assert_eq!(p.theme, color::ThemeId::Koi);
    }

    #[test]
    fn config_round_trips_the_serialized_flag_lines() {
        // exactly what the settings writer emits for non-default feature values
        let text = "plugin_sandbox=off\nosc52=true\ninline_paint=true\nlatency_hud=true\nwsl_distro=Arch\n";
        let p = parse(text);
        assert!(!p.plugin_sandbox && p.osc52 && p.inline_paint && p.latency_hud);
        assert_eq!(p.wsl_distro.as_deref(), Some("Arch"));
    }

    #[test]
    fn config_ignores_unknown_keys_and_malformed_lines() {
        let text = "\nfrobnicate=yes\nlatency_hud=true\n   \nplugin_sandbox\n";
        let p = parse(text);
        // the one real key still takes effect
        assert!(p.latency_hud);
        // a bare key with no '=' is skipped, leaving the secure default
        assert!(p.plugin_sandbox);
        // unknown keys leave everything else at its default
        assert_eq!(p.scrollback, Persisted::default().scrollback);
    }

    #[test]
    fn config_ignores_non_finite_renderer_values() {
        let p = parse(
            "font_size=NaN\npadding=inf\nline_height=-inf\nmin_contrast=NaN\nbackground_image_opacity=NaN\nui_scale=inf\n",
        );
        let defaults = Persisted::default();
        assert_eq!(p.font_size, defaults.font_size);
        assert_eq!(p.padding, defaults.padding);
        assert_eq!(p.line_height, defaults.line_height);
        assert_eq!(p.min_contrast, defaults.min_contrast);
        assert_eq!(p.background_image_opacity, defaults.background_image_opacity);
        assert_eq!(p.ui_scale, defaults.ui_scale);
        assert_eq!(parse("ui_scale=9").ui_scale, 2.0);
    }

    #[test]
    fn bookmarks_parse_once_and_keep_roots_distinct() {
        let parsed = parse(
            "bookmark=C:\\work\\termie\nbookmark=c:/work/termie/\nbookmark=/\nbookmark=\n",
        );
        #[cfg(windows)]
        assert_eq!(parsed.bookmarks, ["C:\\work\\termie", "/"]);
        #[cfg(not(windows))]
        assert_eq!(
            parsed.bookmarks,
            ["C:\\work\\termie", "c:/work/termie/", "/"]
        );
        assert!(!same_location("/", ""));
        #[cfg(windows)]
        assert_eq!(crate::bookmark_name("C:\\work\\termie"), "termie");
    }

    #[test]
    fn font_weight_labels_and_numbers_parse() {
        assert_eq!(font_weight_from_label("semibold"), Some(600));
        assert_eq!(font_weight_from_label("Regular"), Some(400));
        assert_eq!(font_weight_from_label("350"), Some(350));
        // out-of-range numbers clamp, garbage leaves the setting untouched
        assert_eq!(font_weight_from_label("1000"), Some(900));
        assert_eq!(font_weight_from_label("55"), Some(100));
        assert_eq!(font_weight_from_label("heavyish"), None);
    }

    #[test]
    fn rendered_config_parses_back_to_the_same_settings() {
        let text = "scrollback=2500\ncopy_on_select=true\nrestore_on_launch=false\nfont_size=10.5\npadding=3.5\n\
            opacity=90\ncursor=underline\ncursor_blink=false\nline_height=1.2\ntheme=auto\ntheme_dark=nord\n\
            font=Cascadia Code\nfont_weight=600\nmin_contrast=4.5\nbackground_image=C:\\bg.png\n\
            background_image_opacity=0.25\nligatures=false\nfont_features=zero, -calt\nwsl_distro=Arch\nplugin_sandbox=off\nosc52=true\n\
            acrylic=true\nprofile.dev=pwsh.exe -NoLogo\nprofile.dev.cwd=C:\\repo\ntheme.cmd=koi\n\
            bookmark=C:\\work\nterm_program=ghostty\ninline_paint=false\nlatency_hud=true\nupdate_check=false\nbell=off\nui_scale=1.25\n\
            snippet.logs=git log --oneline -20\nsnippet.eq=a=b\nsnippet.=x\nsnippet.tab=a\tb\n";
        let first = render(&parse(text));
        assert_eq!(render(&parse(&first)), first);
        let p = parse(&first);
        assert_eq!((p.font_size, p.padding, p.ui_scale), (10.5, 3.5, 1.25));
        assert_eq!(p.cursor, grid::CursorShape::Underline);
        assert!(p.theme_auto && !p.restore_on_launch && !p.plugin_sandbox && !p.update_check && !p.bell);
        assert!(parse("").bell);
        assert_eq!(p.profiles[0].cwd.as_deref(), Some("C:\\repo"));
        assert_eq!(p.font.as_deref(), Some("Cascadia Code"));
        assert_eq!(p.font_features, "zero, -calt");
        let snippets: Vec<(&str, &str)> = p.snippets.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
        assert_eq!(snippets, [("logs", "git log --oneline -20"), ("eq", "a=b")]);
    }

    #[test]
    fn folder_snippets_come_from_the_nearest_file_above() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let root = std::env::temp_dir().join(format!("termie-snippets-{}-{nonce}", std::process::id()));
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).expect("temp dirs");
        assert!(crate::folder_snippets(&deep).is_empty());
        std::fs::write(root.join(".termie-snippets"), "# build\nbuild = cargo build --release\nbad = x\u{1b}[2J\nnope\n").expect("write");
        assert_eq!(crate::folder_snippets(&deep), [("build".to_string(), "cargo build --release".to_string())]);
        std::fs::write(deep.join(".termie-snippets"), "near=echo near\n").expect("write");
        assert_eq!(crate::folder_snippets(&deep), [("near".to_string(), "echo near".to_string())]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_switch_takes_the_same_spellings() {
        let p = parse("restore_on_launch=off\ncopy_on_select=On\ncursor_blink=no\nupdate_check=0\nosc52=YES\nrestore_contents=no\n");
        assert!(!p.restore_contents);
        assert!(!p.restore_on_launch && p.copy_on_select && !p.cursor_blink && !p.update_check && p.osc52);
        // an unreadable value keeps the default instead of flipping it
        let p = parse("restore_on_launch=maybe\ninline_paint=sure\nbold_as_bright=\n");
        assert!(p.restore_on_launch && p.inline_paint && p.bold_as_bright);
    }

    #[test]
    fn a_misspelled_sandbox_value_keeps_the_sandbox_on() {
        assert!(parse("plugin_sandbox=ture").plugin_sandbox);
        assert!(parse("plugin_sandbox=off\nplugin_sandbox=of").plugin_sandbox);
        assert!(parse("plugin_sandbox=bwrap").plugin_sandbox);
        assert!(!parse("plugin_sandbox=no").plugin_sandbox);
    }
}
