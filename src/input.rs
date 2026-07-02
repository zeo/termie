use winit::event::ElementState;
use winit::keyboard::{Key, ModifiersState, NamedKey};

// kitty keyboard protocol progressive-enhancement flags termie honors
const FLAG_DISAMBIGUATE: u8 = 0b1;
const FLAG_REPORT_EVENTS: u8 = 0b10;

/// translate a key event into the bytes a terminal would send to the pty.
/// `kbd_flags` are the active kitty keyboard protocol flags for the focused
/// terminal (0 = legacy xterm encoding, which is byte-identical to the old
/// behavior). returns None for events that produce no output.
pub fn key_to_bytes(
    logical: &Key,
    text: Option<&str>,
    state: ElementState,
    repeat: bool,
    mods: ModifiersState,
    app_cursor: bool,
    kbd_flags: u8,
) -> Option<Vec<u8>> {
    let disambiguate = kbd_flags & FLAG_DISAMBIGUATE != 0;
    let report_events = kbd_flags & FLAG_REPORT_EVENTS != 0;
    let pressed = state == ElementState::Pressed;

    // releases only matter when an app asked for event types; this keeps the
    // legacy and flag-1-only paths identical to the old press-only behavior
    if !pressed && !report_events {
        return None;
    }

    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // AltGr arrives as ctrl+alt on windows. when the layout translated the
    // chord into printable text (a European layout's [, ], {, }, @, €), that
    // text is what the user typed — send it bare instead of ESC-prefixing it
    // (which would emit "ESC [", the start of a CSI sequence) or CSI-u
    // encoding it. a bare ctrl+alt chord produces no translated text, so it
    // keeps its escape encoding
    let altgr = ctrl && alt && text.is_some_and(|t| !t.is_empty() && !t.chars().any(char::is_control));

    // modifier code per the xterm/kitty spec (1 + bitfield: shift 1, alt 2,
    // ctrl 4, super 8)
    let mod_code = 1
        + (shift as u8)
        + ((alt as u8) << 1)
        + ((ctrl as u8) << 2)
        + ((mods.super_key() as u8) << 3);

    // in legacy mode alt prefixes ESC (metaSendsEscape), the same convention the
    // ordinary-text path uses; under the kitty protocol alt is folded into the
    // modifier field instead, so the prefix is suppressed there
    let legacy = |bytes: &[u8]| -> Option<Vec<u8>> {
        pressed.then(|| {
            let mut v = Vec::with_capacity(bytes.len() + 1);
            if alt && !disambiguate {
                v.push(0x1b);
            }
            v.extend_from_slice(bytes);
            v
        })
    };

    // event type subparameter; forced to press (omitted) unless event reporting
    // is on, so legacy / flag-1-only output never carries a :evt field
    let evt = if report_events {
        if pressed {
            if repeat { 2 } else { 1 }
        } else {
            3
        }
    } else {
        1
    };

    if let Key::Named(named) = logical {
        match named {
            // Enter/Tab/Backspace keep their legacy bytes when unmodified (so a
            // shell stays usable); a modifier makes them unambiguous CSI u
            NamedKey::Enter => {
                if disambiguate && mod_code > 1 {
                    return Some(csi_u(13, mod_code, evt));
                }
                return legacy(b"\r");
            }
            NamedKey::Tab => {
                if disambiguate && mod_code > 1 {
                    return Some(csi_u(9, mod_code, evt));
                }
                if !disambiguate && shift {
                    return pressed.then(|| b"\x1b[Z".to_vec());
                }
                return legacy(b"\t");
            }
            NamedKey::Backspace => {
                if disambiguate && mod_code > 1 {
                    return Some(csi_u(127, mod_code, evt));
                }
                return legacy(b"\x7f");
            }
            // Escape is disambiguated even unmodified (so apps can tell a real
            // Esc keypress from the start of an escape sequence)
            NamedKey::Escape => {
                if disambiguate {
                    return Some(csi_u(27, mod_code, evt));
                }
                return legacy(b"\x1b");
            }
            NamedKey::Space => {
                if disambiguate && (ctrl || alt) {
                    return Some(csi_u(32, mod_code, evt));
                }
                if ctrl && !disambiguate {
                    return pressed.then(|| vec![0u8]);
                }
                return legacy(b" ");
            }
            NamedKey::ArrowUp => return Some(cursor_seq(b'A', mod_code, app_cursor, evt)),
            NamedKey::ArrowDown => return Some(cursor_seq(b'B', mod_code, app_cursor, evt)),
            NamedKey::ArrowRight => return Some(cursor_seq(b'C', mod_code, app_cursor, evt)),
            NamedKey::ArrowLeft => return Some(cursor_seq(b'D', mod_code, app_cursor, evt)),
            NamedKey::Home => return Some(cursor_seq(b'H', mod_code, app_cursor, evt)),
            NamedKey::End => return Some(cursor_seq(b'F', mod_code, app_cursor, evt)),
            NamedKey::PageUp => return Some(tilde_seq(5, mod_code, evt)),
            NamedKey::PageDown => return Some(tilde_seq(6, mod_code, evt)),
            NamedKey::Insert => return Some(tilde_seq(2, mod_code, evt)),
            NamedKey::Delete => return Some(tilde_seq(3, mod_code, evt)),
            NamedKey::F1 => return Some(fkey_seq(b'P', mod_code, evt)),
            NamedKey::F2 => return Some(fkey_seq(b'Q', mod_code, evt)),
            NamedKey::F3 => return Some(fkey_seq(b'R', mod_code, evt)),
            NamedKey::F4 => return Some(fkey_seq(b'S', mod_code, evt)),
            NamedKey::F5 => return Some(tilde_seq(15, mod_code, evt)),
            NamedKey::F6 => return Some(tilde_seq(17, mod_code, evt)),
            NamedKey::F7 => return Some(tilde_seq(18, mod_code, evt)),
            NamedKey::F8 => return Some(tilde_seq(19, mod_code, evt)),
            NamedKey::F9 => return Some(tilde_seq(20, mod_code, evt)),
            NamedKey::F10 => return Some(tilde_seq(21, mod_code, evt)),
            NamedKey::F11 => return Some(tilde_seq(23, mod_code, evt)),
            NamedKey::F12 => return Some(tilde_seq(24, mod_code, evt)),
            _ => return None,
        }
    }

    // under the protocol a modified printable is reported as CSI u with the
    // base (unshifted) codepoint; this also disambiguates ctrl+i from Tab etc
    if disambiguate
        && (ctrl || alt)
        && !altgr
        && let Key::Character(s) = logical
        && let Some(c) = s.chars().next()
    {
        let base = c.to_lowercase().next().unwrap_or(c);
        return Some(csi_u(base as u32, mod_code, evt));
    }

    // legacy control combinations on character keys (protocol off)
    if ctrl && !alt && !disambiguate
        && let Key::Character(s) = logical
        && let Some(c) = s.chars().next()
        && let Some(code) = control_code(c)
    {
        return pressed.then(|| vec![code]);
    }

    // ordinary text; printables have no release event without flag 8
    if !pressed {
        return None;
    }
    let text = text
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .or_else(|| match logical {
            Key::Character(s) => Some(s.to_string()),
            Key::Named(NamedKey::Space) => Some(" ".to_string()),
            _ => None,
        })?;

    let mut out = Vec::new();
    // legacy alt sends an ESC prefix; under the protocol alt is folded into the
    // CSI u modifier field above, so don't double-encode here. AltGr text is
    // never prefixed — it is ordinary typing, not a meta chord
    if alt && !disambiguate && !altgr {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

/// CSI u form: ESC [ code ; mods : evt u, omitting the modifier field when it
/// is 1 and there is no event-type subparameter
fn csi_u(code: u32, mod_code: u8, evt: u8) -> Vec<u8> {
    if mod_code == 1 && evt == 1 {
        format!("\x1b[{}u", code).into_bytes()
    } else if evt == 1 {
        format!("\x1b[{};{}u", code, mod_code).into_bytes()
    } else {
        format!("\x1b[{};{}:{}u", code, mod_code, evt).into_bytes()
    }
}

fn cursor_seq(letter: u8, mod_code: u8, app_cursor: bool, evt: u8) -> Vec<u8> {
    if mod_code == 1 && evt == 1 {
        if app_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    } else if evt == 1 {
        format!("\x1b[1;{}{}", mod_code, letter as char).into_bytes()
    } else {
        format!("\x1b[1;{}:{}{}", mod_code, evt, letter as char).into_bytes()
    }
}

fn tilde_seq(num: u8, mod_code: u8, evt: u8) -> Vec<u8> {
    if mod_code == 1 && evt == 1 {
        format!("\x1b[{}~", num).into_bytes()
    } else if evt == 1 {
        format!("\x1b[{};{}~", num, mod_code).into_bytes()
    } else {
        format!("\x1b[{};{}:{}~", num, mod_code, evt).into_bytes()
    }
}

/// F1-F4 use SS3 (ESC O P..S) when unmodified, the parameterized CSI form when
/// modified or reporting an event type
fn fkey_seq(letter: u8, mod_code: u8, evt: u8) -> Vec<u8> {
    if mod_code == 1 && evt == 1 {
        vec![0x1b, b'O', letter]
    } else if evt == 1 {
        format!("\x1b[1;{}{}", mod_code, letter as char).into_bytes()
    } else {
        format!("\x1b[1;{}:{}{}", mod_code, evt, letter as char).into_bytes()
    }
}

fn control_code(c: char) -> Option<u8> {
    let b = match c {
        'a'..='z' => (c as u8) - b'a' + 1,
        'A'..='Z' => (c as u8) - b'A' + 1,
        '@' | ' ' => 0,
        '[' => 27,
        '\\' => 28,
        ']' => 29,
        '^' => 30,
        '_' => 31,
        '?' => 127,
        _ => return None,
    };
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::ModifiersState as M;

    fn press(logical: Key, mods: M, flags: u8) -> Option<Vec<u8>> {
        key_to_bytes(&logical, None, ElementState::Pressed, false, mods, false, flags)
    }
    fn press_app(logical: Key, mods: M, app: bool, flags: u8) -> Option<Vec<u8>> {
        key_to_bytes(&logical, None, ElementState::Pressed, false, mods, app, flags)
    }
    fn release(logical: Key, mods: M, flags: u8) -> Option<Vec<u8>> {
        key_to_bytes(&logical, None, ElementState::Released, false, mods, false, flags)
    }
    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn enter_plain_is_cr_at_all_flag_levels() {
        for f in [0u8, 1, 3] {
            assert_eq!(
                press(Key::Named(NamedKey::Enter), M::empty(), f),
                Some(b"\r".to_vec())
            );
        }
    }

    #[test]
    fn shift_enter_distinct_under_disambiguate() {
        assert_eq!(
            press(Key::Named(NamedKey::Enter), M::SHIFT, 1),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Enter), M::CONTROL, 1),
            Some(b"\x1b[13;5u".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Enter), M::ALT, 1),
            Some(b"\x1b[13;3u".to_vec())
        );
        // legacy: shift+enter is still a bare CR
        assert_eq!(
            press(Key::Named(NamedKey::Enter), M::SHIFT, 0),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn tab_backspace_escape() {
        assert_eq!(press(Key::Named(NamedKey::Tab), M::empty(), 1), Some(b"\t".to_vec()));
        assert_eq!(
            press(Key::Named(NamedKey::Tab), M::SHIFT, 1),
            Some(b"\x1b[9;2u".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Tab), M::SHIFT, 0),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Backspace), M::empty(), 1),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Backspace), M::CONTROL, 1),
            Some(b"\x1b[127;5u".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Escape), M::empty(), 0),
            Some(b"\x1b".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Escape), M::empty(), 1),
            Some(b"\x1b[27u".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::Escape), M::SHIFT, 1),
            Some(b"\x1b[27;2u".to_vec())
        );
    }

    #[test]
    fn alt_legacy_named_keys_get_esc_prefix() {
        // metaSendsEscape: in legacy mode alt prefixes ESC on the C0 keys, the
        // same as it does on ordinary characters
        assert_eq!(
            press(Key::Named(NamedKey::Backspace), M::ALT, 0),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(press(Key::Named(NamedKey::Enter), M::ALT, 0), Some(b"\x1b\r".to_vec()));
        assert_eq!(press(Key::Named(NamedKey::Escape), M::ALT, 0), Some(b"\x1b\x1b".to_vec()));
        // under the kitty protocol alt is folded into the modifier field, so the
        // C0 keys disambiguate to CSI u instead of getting an ESC prefix
        assert_eq!(
            press(Key::Named(NamedKey::Backspace), M::ALT, 1),
            Some(b"\x1b[127;3u".to_vec())
        );
    }

    #[test]
    fn super_modifier_reported_under_protocol() {
        // super is kitty modifier bit 8 -> mod_code 9
        assert_eq!(
            press(Key::Named(NamedKey::ArrowLeft), M::SUPER, 1),
            Some(b"\x1b[1;9D".to_vec())
        );
    }

    #[test]
    fn altgr_text_is_sent_bare() {
        // AltGr is ctrl+alt on windows; a German layout's AltGr+8 produces "["
        // and it must arrive as "[" — an ESC prefix would start a CSI sequence
        let altgr = M::CONTROL | M::ALT;
        assert_eq!(
            key_to_bytes(&ch("["), Some("["), ElementState::Pressed, false, altgr, false, 0),
            Some(b"[".to_vec())
        );
        // the same under the kitty protocol: text, not a CSI u report
        assert_eq!(
            key_to_bytes(&ch("{"), Some("{"), ElementState::Pressed, false, altgr, false, 1),
            Some(b"{".to_vec())
        );
        // a bare ctrl+alt chord (no layout translation -> no text) keeps its
        // escape encoding: legacy ESC prefix, kitty CSI u
        assert_eq!(press(ch("a"), altgr, 0), Some(b"\x1ba".to_vec()));
        assert_eq!(press(ch("a"), altgr, 1), Some(b"\x1b[97;7u".to_vec()));
    }

    #[test]
    fn ctrl_letter_disambiguates_from_tab() {
        // ctrl+i legacy is 0x09 (collides with Tab); disambiguate makes it distinct
        assert_eq!(press(ch("i"), M::CONTROL, 0), Some(vec![9]));
        assert_eq!(press(ch("i"), M::CONTROL, 1), Some(b"\x1b[105;5u".to_vec()));
        // plain and shifted printables stay text under disambiguate
        assert_eq!(
            key_to_bytes(&ch("a"), Some("a"), ElementState::Pressed, false, M::empty(), false, 1),
            Some(b"a".to_vec())
        );
        assert_eq!(
            key_to_bytes(&ch("A"), Some("A"), ElementState::Pressed, false, M::SHIFT, false, 1),
            Some(b"A".to_vec())
        );
    }

    #[test]
    fn arrows_modifiers_and_app_mode() {
        assert_eq!(
            press(Key::Named(NamedKey::ArrowUp), M::empty(), 0),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            press_app(Key::Named(NamedKey::ArrowUp), M::empty(), true, 0),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            press(Key::Named(NamedKey::ArrowUp), M::SHIFT, 0),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn releases_only_with_event_types() {
        // flag 1 only: no release output at all
        assert_eq!(release(Key::Named(NamedKey::Enter), M::SHIFT, 1), None);
        // flags 1+2: modified enter release reported, unmodified enter not
        assert_eq!(
            release(Key::Named(NamedKey::Enter), M::SHIFT, 3),
            Some(b"\x1b[13;2:3u".to_vec())
        );
        assert_eq!(release(Key::Named(NamedKey::Enter), M::empty(), 3), None);
        // a press under event types still omits the default :1
        assert_eq!(
            press(Key::Named(NamedKey::Enter), M::SHIFT, 3),
            Some(b"\x1b[13;2u".to_vec())
        );
    }
}
