//! chrome design tokens in points; every chrome measurement snaps to these and
//! is converted to device pixels once through `px`

pub const SPACE_2XS: f32 = 2.0;
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;
pub const SPACE_2XL: f32 = 32.0;

pub const RADIUS_OVERLAY: f32 = 12.0;
pub const RADIUS_ROW: f32 = 6.0;
pub const RADIUS_CHIP: f32 = 4.0;

pub const TITLE_BAR: f32 = 38.0;
pub const STATUS_BAR: f32 = 24.0;
pub const WINDOW_PAD: f32 = 12.0;

pub const CHROME_PT: f32 = 12.0;
pub const ICON: f32 = 10.0;
pub const CONTROL_W: f32 = 46.0;

pub const TAB_MIN: f32 = 120.0;
pub const TAB_MAX: f32 = 220.0;
pub const TAB_INSET: f32 = 12.0;
pub const TAB_RAIL: f32 = 2.0;
pub const TAB_DIVIDER: f32 = 16.0;

pub const DOT: f32 = 6.0;
pub const PALETTE_W: f32 = 640.0;
pub const MENU_W: f32 = 196.0;

/// unfocused panes fade toward their background by this much
pub const PANE_DIM: f32 = 0.28;
/// inactive tab labels and other secondary text
pub const SECONDARY: f32 = 0.55;
pub const KEY_LABEL: f32 = 0.5;
pub const KEY_TRACK_EM: f32 = 0.06;

/// windows' caption close-hover red, kept for every theme so the destructive
/// control reads the way the platform trained it to
pub const CLOSE_RED: (u8, u8, u8) = (0xC4, 0x2B, 0x1C);

pub fn px(pt: f32, scale: f32) -> f32 {
    (pt * scale).round()
}
