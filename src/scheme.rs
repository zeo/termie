//! color schemes from other terminals, as colors.conf keys: a windows
//! terminal scheme object, an iterm2 .itermcolors plist, or a ghostty theme

use crate::color::Rgb;
use crate::plugin::json::Json;

const WT_ANSI: [&str; 8] = ["black", "red", "green", "yellow", "blue", "purple", "cyan", "white"];

/// the colors.conf keys a scheme sets, in conf order; None when the text is
/// none of the three formats or names no color
pub fn import(text: &str) -> Option<Vec<(String, Rgb)>> {
    let text = text.trim().trim_start_matches('\u{feff}');
    let colors = if text.starts_with('{') {
        windows_terminal(text)?
    } else if text.contains("<plist") {
        iterm(text)
    } else {
        ghostty(text)
    };
    (!colors.is_empty()).then_some(colors)
}

fn windows_terminal(text: &str) -> Option<Vec<(String, Rgb)>> {
    let json = Json::parse(text)?;
    let bright = WT_ANSI.map(|n| format!("bright{}{}", n[..1].to_ascii_uppercase(), &n[1..]));
    let keys = [("background", "bg"), ("foreground", "fg"), ("cursorColor", "cursor"), ("selectionBackground", "sel")]
        .map(|(wt, key)| (wt.to_string(), key.to_string()))
        .into_iter()
        .chain(WT_ANSI.iter().map(|n| n.to_string()).chain(bright).enumerate().map(|(i, n)| (n, format!("ansi{i}"))));
    Some(keys.filter_map(|(wt, key)| Some((key, crate::parse_color(json.get_str(&wt)?)?))).collect())
}

fn iterm(text: &str) -> Vec<(String, Rgb)> {
    let names = [("Background", "bg"), ("Foreground", "fg"), ("Cursor", "cursor"), ("Selection", "sel")]
        .map(|(it, key)| (it.to_string(), key.to_string()))
        .into_iter()
        .chain((0..16).map(|i| (format!("Ansi {i}"), format!("ansi{i}"))));
    names
        .filter_map(|(name, key)| {
            let start = text.find(&format!("<key>{name} Color</key>"))?;
            let dict = &text[start..];
            let dict = &dict[..dict.find("</dict>")?];
            let component = |c: &str| -> Option<u8> {
                let after = &dict[dict.find(&format!("<key>{c} Component</key>"))?..];
                let value = &after[after.find("<real>")? + 6..];
                let v: f64 = value[..value.find("</real>")?].trim().parse().ok()?;
                Some((v.clamp(0.0, 1.0) * 255.0).round() as u8)
            };
            Some((key, Rgb::new(component("Red")?, component("Green")?, component("Blue")?)))
        })
        .collect()
}

fn ghostty(text: &str) -> Vec<(String, Rgb)> {
    // ghostty takes hex with or without the leading #
    let hex = |v: &str| {
        let v = v.trim();
        crate::parse_color(&format!("#{}", v.strip_prefix('#').unwrap_or(v)))
    };
    let mut out: Vec<(String, Rgb)> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let parsed = match k.trim() {
            "background" => hex(v).map(|c| ("bg".to_string(), c)),
            "foreground" => hex(v).map(|c| ("fg".to_string(), c)),
            "cursor-color" => hex(v).map(|c| ("cursor".to_string(), c)),
            "selection-background" => hex(v).map(|c| ("sel".to_string(), c)),
            "palette" => v.split_once('=').and_then(|(n, c)| {
                let n: usize = n.trim().parse().ok().filter(|n| *n < 256)?;
                Some((format!("ansi{n}"), hex(c)?))
            }),
            _ => None,
        };
        if let Some((key, c)) = parsed {
            out.retain(|(k, _)| *k != key);
            out.push((key, c));
        }
    }
    out
}

/// colors.conf with `colors` written in: a key already set gets its first
/// line replaced and later duplicates dropped, new keys go at the end, and
/// comments and other keys stay as they were
pub fn merge_conf(existing: &str, colors: &[(String, Rgb)]) -> String {
    let line_for = |c: &Rgb| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
    let mut written: Vec<&str> = Vec::new();
    let mut out = String::new();
    for line in existing.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim()).filter(|_| !line.trim_start().starts_with('#'));
        match key.and_then(|k| colors.iter().find(|(name, _)| name == k)) {
            Some((name, _)) if written.contains(&name.as_str()) => continue,
            Some((name, c)) => {
                written.push(name);
                out.push_str(&format!("{name}={}\n", line_for(c)));
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (name, c) in colors.iter().filter(|(name, _)| !written.contains(&name.as_str())) {
        out.push_str(&format!("{name}={}\n", line_for(c)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(colors: &[(String, Rgb)]) -> Vec<String> {
        colors.iter().map(|(k, c)| format!("{k}={:02x}{:02x}{:02x}", c.r, c.g, c.b)).collect()
    }

    #[test]
    fn windows_terminal_scheme_maps_names_to_ansi_slots() {
        let wt = r##"{ "name": "Campbell", "background": "#0C0C0C", "foreground": "#CCC",
            "cursorColor": "#FFFFFF", "black": "#0C0C0C", "purple": "#881798",
            "brightPurple": "#B4009E", "brightWhite": "#F2F2F2" }"##;
        assert_eq!(
            keys(&import(wt).unwrap()),
            ["bg=0c0c0c", "fg=cccccc", "cursor=ffffff", "ansi0=0c0c0c", "ansi5=881798", "ansi13=b4009e", "ansi15=f2f2f2"]
        );
        assert!(import("{ \"name\": \"empty\" }").is_none());
        assert!(import("{ not json").is_none());
    }

    #[test]
    fn iterm_plist_components_become_hex() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
            <key>Ansi 0 Color</key><dict><key>Alpha Component</key><real>1</real>
            <key>Blue Component</key><real>0.1725</real><key>Color Space</key><string>sRGB</string>
            <key>Green Component</key><real>0.1333</real><key>Red Component</key><real>0.1294</real></dict>
            <key>Background Color</key><dict><key>Blue Component</key><real>1</real>
            <key>Green Component</key><real>0.5</real><key>Red Component</key><real>0</real></dict>
            <key>Ansi 1 Color</key><dict><key>Red Component</key><real>1</real></dict>
            </dict></plist>"#;
        assert_eq!(keys(&import(plist).unwrap()), ["bg=0080ff", "ansi0=21222c"]);
    }

    #[test]
    fn ghostty_theme_reads_palette_and_bare_hex() {
        let theme = "# dracula\npalette = 0=#21222c\npalette = 9=ff6e6e\nbackground = 282a36\nforeground = #f8f8f2\n\
                     cursor-color = cell-foreground\nselection-background = 44475a\npalette = 300=#ffffff\nfont-size = 12\n";
        assert_eq!(keys(&import(theme).unwrap()), ["ansi0=21222c", "ansi9=ff6e6e", "bg=282a36", "fg=f8f8f2", "sel=44475a"]);
        assert!(import("font-size = 12\nsome prose here").is_none());
    }

    #[test]
    fn merge_replaces_set_keys_and_keeps_the_rest() {
        let colors = [("bg".to_string(), Rgb::new(1, 2, 3)), ("ansi1".to_string(), Rgb::new(255, 0, 0))];
        let merged = merge_conf("# mine\nbg = #ffffff\nrule=#333333\n#bg=#000000\nbg=#eeeeee\n", &colors);
        assert_eq!(merged, "# mine\nbg=#010203\nrule=#333333\n#bg=#000000\nansi1=#ff0000\n");
    }
}
