use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// translate a winit key press into the bytes a terminal would send to the pty.
/// returns None for keys that produce no output (modifiers, releases, etc).
pub fn key_to_bytes(
    event: &KeyEvent,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // modifier code per the xterm spec (1 + bitfield)
    let mod_code = 1 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2);

    if let Key::Named(named) = &event.logical_key {
        let bytes = match named {
            NamedKey::Enter => return Some(b"\r".to_vec()),
            NamedKey::Backspace => return Some(b"\x7f".to_vec()),
            NamedKey::Escape => return Some(b"\x1b".to_vec()),
            NamedKey::Tab => {
                return Some(if shift {
                    b"\x1b[Z".to_vec()
                } else {
                    b"\t".to_vec()
                })
            }
            NamedKey::Space if ctrl => return Some(vec![0]),
            NamedKey::ArrowUp => return Some(cursor_seq(b'A', mod_code, app_cursor)),
            NamedKey::ArrowDown => return Some(cursor_seq(b'B', mod_code, app_cursor)),
            NamedKey::ArrowRight => return Some(cursor_seq(b'C', mod_code, app_cursor)),
            NamedKey::ArrowLeft => return Some(cursor_seq(b'D', mod_code, app_cursor)),
            NamedKey::Home => return Some(cursor_seq(b'H', mod_code, app_cursor)),
            NamedKey::End => return Some(cursor_seq(b'F', mod_code, app_cursor)),
            NamedKey::PageUp => return Some(tilde_seq(5, mod_code)),
            NamedKey::PageDown => return Some(tilde_seq(6, mod_code)),
            NamedKey::Insert => return Some(tilde_seq(2, mod_code)),
            NamedKey::Delete => return Some(tilde_seq(3, mod_code)),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => return Some(tilde_seq(15, mod_code)),
            NamedKey::F6 => return Some(tilde_seq(17, mod_code)),
            NamedKey::F7 => return Some(tilde_seq(18, mod_code)),
            NamedKey::F8 => return Some(tilde_seq(19, mod_code)),
            NamedKey::F9 => return Some(tilde_seq(20, mod_code)),
            NamedKey::F10 => return Some(tilde_seq(21, mod_code)),
            NamedKey::F11 => return Some(tilde_seq(23, mod_code)),
            NamedKey::F12 => return Some(tilde_seq(24, mod_code)),
            NamedKey::Space => {
                // fall through to text handling below
                Vec::new()
            }
            _ => return None,
        };
        if !bytes.is_empty() {
            return Some(bytes);
        }
    }

    // control combinations on character keys
    if ctrl && !alt
        && let Key::Character(s) = &event.logical_key
            && let Some(c) = s.chars().next()
                && let Some(code) = control_code(c) {
                    return Some(vec![code]);
                }

    // ordinary text (prefer the layout-resolved text winit provides)
    let text = event
        .text
        .as_ref()
        .map(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .or_else(|| match &event.logical_key {
            Key::Character(s) => Some(s.to_string()),
            Key::Named(NamedKey::Space) => Some(" ".to_string()),
            _ => None,
        })?;

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

fn cursor_seq(final_byte: u8, mod_code: u8, app_cursor: bool) -> Vec<u8> {
    if mod_code > 1 {
        format!("\x1b[1;{}{}", mod_code, final_byte as char).into_bytes()
    } else if app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn tilde_seq(num: u8, mod_code: u8) -> Vec<u8> {
    if mod_code > 1 {
        format!("\x1b[{};{}~", num, mod_code).into_bytes()
    } else {
        format!("\x1b[{}~", num).into_bytes()
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
