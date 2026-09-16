//! Maple theme tokens for the gpui frontend.
//!
//! Values come from the Maple brand kit (`docs/brand.md`): the light
//! palette is the kit's own; the dark palette is derived from it, using
//! the Pebble scale for chrome so the dark app keeps the same lavender-grey
//! cast as the light one. Each token is a function that reads the active
//! palette, so a theme switch needs only a window refresh. Values are
//! `u32` hex literals for `gpui::rgb`.
//!
//! Scale steps referenced in comments: `maple-500` is the coral primary,
//! `pebble-*` the secondary grey-lavender, `bark-*` the tertiary brown,
//! `neutral-*` the greys.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use gpui::{Pixels, px};

/// Theme preference from settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    System,
    Dark,
    Light,
}

impl Preference {
    pub fn parse(value: &str) -> Self {
        match value {
            "dark" => Self::Dark,
            "light" => Self::Light,
            _ => Self::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

static PREFERENCE: AtomicU8 = AtomicU8::new(0);
static LIGHT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_preference(preference: Preference) {
    PREFERENCE.store(
        match preference {
            Preference::System => 0,
            Preference::Dark => 1,
            Preference::Light => 2,
        },
        Ordering::Relaxed,
    );
}

/// Set the preference and tell the platform, so a forced theme also
/// restyles the window chrome (traffic lights, native menus, scrollbars).
/// `System` hands control back to the OS. Every view must render again.
pub fn apply_preference(preference: Preference, cx: &mut gpui::App) {
    set_preference(preference);
    cx.set_window_appearance(match preference {
        Preference::System => None,
        Preference::Dark => Some(gpui::WindowAppearance::Dark),
        Preference::Light => Some(gpui::WindowAppearance::Light),
    });
    cx.refresh_windows();
}

pub fn preference() -> Preference {
    match PREFERENCE.load(Ordering::Relaxed) {
        1 => Preference::Dark,
        2 => Preference::Light,
        _ => Preference::System,
    }
}

/// Pick the palette for the preference and the window appearance. Returns
/// whether the active palette changed, in which case every view must
/// render again.
pub fn resolve(appearance: gpui::WindowAppearance) -> bool {
    let light = match preference() {
        Preference::Dark => false,
        Preference::Light => true,
        Preference::System => matches!(
            appearance,
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
        ),
    };
    LIGHT_ACTIVE.swap(light, Ordering::Relaxed) != light
}

pub fn is_light() -> bool {
    LIGHT_ACTIVE.load(Ordering::Relaxed)
}

macro_rules! tokens {
    ($($(#[$doc:meta])* $name:ident: $dark:literal, $light:literal;)*) => {
        struct Palette {
            $($name: u32,)*
        }

        const DARK: Palette = Palette { $($name: $dark,)* };
        const LIGHT: Palette = Palette { $($name: $light,)* };

        $(
            $(#[$doc])*
            pub fn $name() -> u32 {
                palette().$name
            }
        )*
    };
}

fn palette() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
}

tokens! {
    // Surfaces. Light: white page, neutral-50 cards, pebble-50 sidebar.
    // Dark: lifted well (not near-black) with pebble-tinted chrome so
    // body type does not bloom. Depth is app < sidebar < elevated.
    bg_app: 0x18181c, 0xffffff;
    bg_sidebar: 0x1e1e24, 0xf7f7f9;
    bg_elevated: 0x27272e, 0xffffff;
    bg_sidebar_card: 0x27272e, 0xfafafa;
    bg_sidebar_pill: 0x32323a, 0xe8e8ed;
    bg_input: 0x1c1c22, 0xffffff;
    bg_user_bubble: 0x27272e, 0xe8e8ed;
    bg_code_block: 0x141416, 0xf7f7f9;
    bg_tool_card: 0x1e1e24, 0xfafafa;

    // Hairlines. pebble-100 on light; a step above each dark surface.
    border: 0x3a3a44, 0xe8e8ed;
    border_subtle: 0x2c2c34, 0xf1f1f4;

    // Text. Light: neutral-900 / 600 / 400 / 300.
    // Dark body is Pebble 200 (~12:1 on the well), not Pebble 50, so
    // Regular and SemiBold stay distinct. Headings sit one step brighter.
    text_primary: 0xd1d2dc, 0x171717;
    text_heading: 0xe8e8ed, 0x171717;
    text_secondary: 0xbabccb, 0x525252;
    text_muted: 0x9c9dab, 0xa3a3a3;
    text_faint: 0x5e5f6e, 0xd4d4d4;

    /// Maple coral (maple-500): send button, focus, caret, permission prompts.
    accent: 0xff9771, 0xff9771;
    /// Hover for accent fills: maple-400 on dark, maple-600 on light.
    accent_hover: 0xffa88a, 0xf67d57;
    /// Text and icons placed on an accent fill (pebble-50).
    on_accent: 0xf7f7f9, 0xf7f7f9;
    /// Soft coral container (maple-100 / a dark coral tint).
    accent_container: 0x3a2118, 0xffe8e0;

    status_running: 0xbabccb, 0xa3a3a3;
    status_success: 0x8fa35a, 0x7b8f4a;
    status_error: 0xe07052, 0xd05e41;
    /// Wavy underline under a misspelled word in the composer.
    spell_error: 0xe07052, 0xd05e41;
    status_warning: 0xd4a35a, 0xd4a35a;

    code_text: 0xe8e8ed, 0x171717;
    /// Links take the tertiary Bark scale (bark-300 / bark-500).
    link: 0xc29a8d, 0x9e7469;

    /// Permission card fill/border: the coral container and maple-300.
    permission_fill: 0x2a1a14, 0xffe8e0;
    permission_border: 0x784a38, 0xffbaa2;

    user_bubble_border: 0x3a3a44, 0xe8e8ed;

    /// Sidebar chrome: segmented toggle track, row hover, selected row
    /// (pebble-100 / pebble-200 on light).
    bg_sidebar_chrome: 0x1e1e24, 0xffffff;
    bg_sidebar_row_hover: 0x32323a, 0xe8e8ed;
    bg_sidebar_row_selected: 0x3a3a44, 0xd1d2dc;

    /// Display headings (Array face): pebble-300 on dark, pebble-800 on
    /// light, as the brand kit sets its section titles.
    display_text: 0xbabccb, 0x474854;

    /// Send button gradient stops (maple-500 to maple-700).
    send_top: 0xff9771, 0xff9771;
    send_bottom: 0xe8633d, 0xe8633d;

    /// Title bar control buttons.
    bg_title_control: 0x3a3a44, 0xe8e8ed;
    bg_title_control_hover: 0x474854, 0xd1d2dc;

    /// Text input caret; near the primary text color of each palette.
    text_cursor: 0xf7f7f9, 0x171717;
}

/// Translucent selection highlight in text inputs: the coral primary at a
/// low alpha, a little stronger on light where the white field washes it
/// out.
pub fn text_selection() -> gpui::Rgba {
    if is_light() {
        gpui::rgba(0xff977159)
    } else {
        gpui::rgba(0xff977147)
    }
}

/// Light veil over a pane whose content is being replaced.
pub fn loading_veil() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 1., 0.55)
    } else {
        gpui::hsla(0., 0., 0., 0.45)
    }
}

/// Dimming layer behind a modal or the image lightbox. One opacity for
/// every overlay, so a dialog and the lightbox darken the app equally.
pub fn scrim() -> gpui::Rgba {
    gpui::rgba(0x000000a0)
}

/// Translucent fill for hover states and inline code over any surface.
pub fn overlay_hover() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 0., 0.06)
    } else {
        gpui::hsla(0., 0., 1., 0.08)
    }
}

/// Scrollbar thumb over the transcript.
pub fn scrollbar_thumb() -> gpui::Rgba {
    if is_light() {
        gpui::rgba(0x00000033)
    } else {
        gpui::rgba(0xffffff26)
    }
}

/// Scrollbar thumb while hovered or dragged.
pub fn scrollbar_thumb_active() -> gpui::Rgba {
    if is_light() {
        gpui::rgba(0x00000059)
    } else {
        gpui::rgba(0xffffff4d)
    }
}

/// Placeholder text in inputs.
pub fn placeholder() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 0., 0.55)
    } else {
        gpui::hsla(0., 0., 1., 0.5)
    }
}

/// Corner radii from the brand kit. `SM` is for small controls (icon
/// buttons, menu rows, chips), `MD` for popups and inputs, `LG` for cards
/// and message bubbles, `XL` for the composer and dialogs. Pills use
/// `rounded_full`.
pub const RADIUS_SM: Pixels = px(8.);
pub const RADIUS_MD: Pixels = px(12.);
pub const RADIUS_LG: Pixels = px(16.);
pub const RADIUS_XL: Pixels = px(24.);

/// The brand kit's medium spacing step (`--space-md`): horizontal padding
/// of every pill button.
pub const SPACE_MD: Pixels = px(20.);
