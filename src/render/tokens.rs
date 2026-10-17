//! chrome design tokens in points; every chrome measurement snaps to these and
//! is converted to device pixels once through `px`

/// optical nudges: carets, inset highlights, the tight shadow layer
pub const SPACE_3XS: f32 = 1.0;
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
pub const ICON_SMALL: f32 = 8.0;
pub const ICON_STATUS: f32 = 9.0;
pub const BADGE: f32 = 20.0;
pub const WELCOME_BADGE: f32 = 48.0;
pub const CONTROL_W: f32 = 46.0;

pub const TAB_MIN: f32 = 120.0;
pub const TAB_MAX: f32 = 220.0;
pub const TAB_INSET: f32 = 12.0;
pub const TAB_RAIL: f32 = 2.0;
pub const TAB_DIVIDER: f32 = 16.0;

pub const CONTROL_H: f32 = 28.0;
pub const SWITCH_W: f32 = 32.0;
pub const SWITCH_H: f32 = 18.0;
pub const FIND_W: f32 = 560.0;
pub const DIALOG_MIN_W: f32 = 300.0;
pub const DIALOG_MAX_W: f32 = 520.0;
pub const MENU_MAX_W: f32 = 360.0;
pub const STEPPER_VALUE_W: f32 = 60.0;
pub const SETTINGS_LABEL_W: f32 = 152.0;
pub const SETTINGS_MIN_W: f32 = 460.0;
pub const SETTINGS_MAX_W: f32 = 760.0;
/// the settings drawer always leaves this much terminal visible beside it
pub const SETTINGS_GUTTER: f32 = 160.0;
pub const SETTINGS_FLOOR_W: f32 = 360.0;
pub const MARKET_W: f32 = 940.0;
pub const DOCK_W: f32 = 224.0;
/// the invisible band along the window edge that grabs a resize
pub const RESIZE_EDGE: f32 = 6.0;

pub const DOT: f32 = 6.0;
pub const PALETTE_W: f32 = 640.0;
pub const MENU_W: f32 = 196.0;

/// floor for chrome text against its surface: wcag aa 4.5:1, asked a touch
/// high so 8-bit rounding never lands under it
pub const TEXT_CONTRAST: f32 = 4.6;

/// unfocused panes fade toward their background by this much
pub const PANE_DIM: f32 = 0.28;
/// inactive tab labels and other secondary text
pub const SECONDARY: f32 = 0.55;
pub const KEY_TRACK_EM: f32 = 0.06;

pub fn px(pt: f32, scale: f32) -> f32 {
    (pt * scale).round()
}
