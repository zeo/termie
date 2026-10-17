//! the instrument theme's chrome ladder as plain rgb triples, shared by the
//! app palette and the installer (which includes this file by path) so the two
//! can never drift apart

pub const INK0: [u8; 3] = [0x05, 0x05, 0x05];
pub const INK1: [u8; 3] = [0x0d, 0x0d, 0x0d];
pub const INK3: [u8; 3] = [0x1c, 0x1c, 0x1c];
pub const INK4: [u8; 3] = [0x26, 0x26, 0x26];
pub const RULE: [u8; 3] = [0x2a, 0x2a, 0x2a];
pub const RULE2: [u8; 3] = [0x3a, 0x3a, 0x3a];
pub const MUTE: [u8; 3] = [0x6f, 0x6f, 0x6f];
pub const TEXT2: [u8; 3] = [0xed, 0xed, 0xed];
pub const PAPER: [u8; 3] = [0xf5, 0xf5, 0xf5];
pub const FG: [u8; 3] = [0xc8, 0xc8, 0xc8];
pub const BG: [u8; 3] = [0x14, 0x14, 0x14];

/// windows' caption close-hover red, kept for every theme so the destructive
/// control reads the way the platform trained it to
pub const CLOSE_RED: [u8; 3] = [0xc4, 0x2b, 0x1c];
