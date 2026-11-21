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

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{Pixels, px};

/// Theme choice from settings. The saved value is the stable ID from
/// [`Preference::as_str`]; `system`, `dark` and `light` predate the named
/// themes, so earlier settings keep their meaning without a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    /// Standard Maple, following the OS appearance.
    System,
    /// Maple Light, always.
    Light,
    /// Maple Dark, always.
    Dark,
    Dracula,
    Americana,
    NeonDusk,
    CherryBlossom,
}

impl Preference {
    /// Every choice, in the order Settings offers them.
    pub const ALL: [Self; 7] = [
        Self::System,
        Self::Light,
        Self::Dark,
        Self::Dracula,
        Self::Americana,
        Self::NeonDusk,
        Self::CherryBlossom,
    ];

    /// An unknown ID (a newer app's theme, or a damaged value) falls back
    /// to Standard Maple. The caller keeps the saved string, so a later
    /// app that knows the ID still finds it.
    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|preference| preference.as_str() == value)
            .unwrap_or(Self::System)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Dracula => "dracula",
            Self::Americana => "americana",
            Self::NeonDusk => "neon-dusk",
            Self::CherryBlossom => "cherry-blossom",
        }
    }

    /// The fixed palette, or `None` when the choice follows the OS.
    fn fixed_palette(self) -> Option<PaletteId> {
        match self {
            Self::System => None,
            Self::Light => Some(PaletteId::Light),
            Self::Dark => Some(PaletteId::Dark),
            Self::Dracula => Some(PaletteId::Dracula),
            Self::Americana => Some(PaletteId::Americana),
            Self::NeonDusk => Some(PaletteId::NeonDusk),
            Self::CherryBlossom => Some(PaletteId::CherryBlossom),
        }
    }

    /// The two dots a Settings choice previews: the canvas and accent of
    /// the palette it draws with, or for Standard Maple, which follows the
    /// OS, the Maple Dark and Maple Light canvases.
    pub fn swatch(self) -> [u32; 2] {
        match self.fixed_palette() {
            Some(palette) => [palette.palette().bg_app, palette.palette().accent],
            None => [DARK.bg_app, LIGHT.bg_app],
        }
    }
}

/// A complete palette. Two dark themes are different palettes, so the
/// resolver tracks identity rather than a light/dark flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PaletteId {
    Dark = 0,
    Light = 1,
    Dracula = 2,
    Americana = 3,
    NeonDusk = 4,
    CherryBlossom = 5,
}

impl PaletteId {
    const ALL: [Self; 6] = [
        Self::Dark,
        Self::Light,
        Self::Dracula,
        Self::Americana,
        Self::NeonDusk,
        Self::CherryBlossom,
    ];

    fn from_u8(value: u8) -> Self {
        Self::ALL
            .into_iter()
            .find(|id| *id as u8 == value)
            .unwrap_or(Self::Dark)
    }

    fn is_light(self) -> bool {
        matches!(self, Self::Light | Self::CherryBlossom)
    }

    fn palette(self) -> &'static Palette {
        match self {
            Self::Dark => &DARK,
            Self::Light => &LIGHT,
            Self::Dracula => &DRACULA,
            Self::Americana => &AMERICANA,
            Self::NeonDusk => &NEON_DUSK,
            Self::CherryBlossom => &CHERRY_BLOSSOM,
        }
    }
}

static PREFERENCE: AtomicU8 = AtomicU8::new(0);
static ACTIVE_PALETTE: AtomicU8 = AtomicU8::new(PaletteId::Dark as u8);
#[cfg(target_os = "linux")]
static SYSTEM_APPEARANCE: AtomicU8 = AtomicU8::new(0);

/// Linux's protected GUI cannot query the portal itself. The credential-free
/// helper supplies this value; native appearance remains the fallback.
#[cfg(target_os = "linux")]
pub fn set_system_appearance(appearance: gpui::WindowAppearance) {
    SYSTEM_APPEARANCE.store(
        if appearance_is_light(appearance) {
            2
        } else {
            1
        },
        Ordering::Relaxed,
    );
}

fn appearance_is_light(appearance: gpui::WindowAppearance) -> bool {
    matches!(
        appearance,
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
    )
}

pub fn set_preference(preference: Preference) {
    let index = Preference::ALL
        .iter()
        .position(|candidate| *candidate == preference)
        .unwrap_or(0);
    PREFERENCE.store(index as u8, Ordering::Relaxed);
}

/// Set the preference and tell the platform, so a fixed theme also
/// restyles the window chrome (traffic lights, native menus, scrollbars)
/// to match its lightness. `System` hands control back to the OS. Every
/// open window renders again, including a switch between two dark themes.
pub fn apply_preference(preference: Preference, cx: &mut gpui::App) {
    set_preference(preference);
    cx.set_window_appearance(preference.fixed_palette().map(|palette| {
        if palette.is_light() {
            gpui::WindowAppearance::Light
        } else {
            gpui::WindowAppearance::Dark
        }
    }));
    // Resolve here rather than waiting for the main window's render, which
    // may not exist (only a dialog open, or the main window in the tray).
    resolve(cx.window_appearance());
    cx.refresh_windows();
}

/// Resolve against the app's appearance and repaint every window if the
/// palette changed. For changes that arrive outside any render: the Linux
/// portal helper reporting a new OS appearance. Other platforms deliver
/// appearance changes to each window's `observe_window_appearance`.
#[cfg(target_os = "linux")]
pub fn sync(cx: &mut gpui::App) {
    if resolve(cx.window_appearance()) {
        cx.refresh_windows();
    }
}

/// Resolve against one top-level window's appearance and repaint every
/// window if the palette changed. Every top-level view calls this first in
/// its render and on `observe_window_appearance`, so whichever windows are
/// open (the main window, a quit prompt, a native dialog) keep the palette
/// current; a change between two palettes of the same lightness repaints
/// them all.
pub fn sync_window(window: &gpui::Window, cx: &mut gpui::App) {
    if resolve(window.appearance()) {
        cx.refresh_windows();
    }
}

pub fn preference() -> Preference {
    Preference::ALL
        .get(usize::from(PREFERENCE.load(Ordering::Relaxed)))
        .copied()
        .unwrap_or(Preference::System)
}

/// Pick the palette for the preference and the window appearance. Returns
/// whether the active palette changed, in which case every view must
/// render again.
pub fn resolve(appearance: gpui::WindowAppearance) -> bool {
    #[cfg(target_os = "linux")]
    let system = match SYSTEM_APPEARANCE.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let system = None;
    let palette = resolved_palette(preference(), appearance, system) as u8;
    ACTIVE_PALETTE.swap(palette, Ordering::Relaxed) != palette
}

fn resolved_palette(
    preference: Preference,
    appearance: gpui::WindowAppearance,
    system: Option<bool>,
) -> PaletteId {
    preference.fixed_palette().unwrap_or_else(|| {
        if system.unwrap_or_else(|| appearance_is_light(appearance)) {
            PaletteId::Light
        } else {
            PaletteId::Dark
        }
    })
}

/// Serializes tests that change the process-global theme and restores
/// Standard Maple on a dark OS when the test ends, panics included, so a
/// flip never leaks into another test that reads the palette.
///
/// Take it after any other test lock (`SETTINGS_IO_LOCK`, the localization
/// guard) so the locks are always acquired in one order.
#[cfg(test)]
pub(crate) struct TestThemeGuard(#[allow(dead_code)] parking_lot::MutexGuard<'static, ()>);

#[cfg(test)]
impl TestThemeGuard {
    pub(crate) fn acquire() -> Self {
        static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        Self(LOCK.lock())
    }
}

#[cfg(test)]
impl Drop for TestThemeGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        SYSTEM_APPEARANCE.store(0, Ordering::Relaxed);
        set_preference(Preference::System);
        resolve(gpui::WindowAppearance::Dark);
    }
}

#[cfg(test)]
mod repaint_tests {
    use super::*;
    use gpui::{Context, IntoElement, Render, Styled as _, TestAppContext, Window, div};
    use std::{cell::Cell, rc::Rc};

    /// A top-level view that records the canvas color it last painted.
    /// `syncs` mirrors the app's top-level views, which call `sync_window`
    /// first in render; a view without it stands for a window that only
    /// reads the palette.
    struct Probe {
        painted: Rc<Cell<u32>>,
        syncs: bool,
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.syncs {
                sync_window(window, cx);
            }
            self.painted.set(bg_app());
            div().size_full().bg(gpui::rgb(bg_app()))
        }
    }

    fn open(cx: &mut TestAppContext, syncs: bool) -> (gpui::WindowHandle<Probe>, Rc<Cell<u32>>) {
        let painted = Rc::new(Cell::new(0));
        let probe = painted.clone();
        let window = cx.add_window(move |_, _| Probe {
            painted: probe,
            syncs,
        });
        (window, painted)
    }

    #[gpui::test]
    fn a_switch_between_dark_themes_repaints_every_window(cx: &mut TestAppContext) {
        let _theme = TestThemeGuard::acquire();
        cx.update(|cx| apply_preference(Preference::Dark, cx));
        let (first, first_painted) = open(cx, true);
        let (_second, second_painted) = open(cx, true);
        cx.run_until_parked();
        assert_eq!(first_painted.get(), DARK.bg_app);
        assert_eq!(second_painted.get(), DARK.bg_app);

        // Settings: Maple Dark to Dracula, both dark.
        cx.update(|cx| apply_preference(Preference::Dracula, cx));
        cx.run_until_parked();
        assert_eq!(first_painted.get(), DRACULA.bg_app);
        assert_eq!(second_painted.get(), DRACULA.bg_app);

        // A change only one window's render notices still repaints the
        // other: resolution compares palettes, not lightness.
        set_preference(Preference::NeonDusk);
        first
            .update(cx, |_, _, cx| cx.notify())
            .expect("first window is open");
        cx.run_until_parked();
        assert_eq!(first_painted.get(), NEON_DUSK.bg_app);
        assert_eq!(second_painted.get(), NEON_DUSK.bg_app);
    }

    /// With the main window gone (closed to the tray), an OS appearance
    /// change still reaches a remaining window that never resolves the
    /// palette itself: the portal update resolves at app level.
    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn an_os_change_repaints_a_window_that_does_not_resolve(cx: &mut TestAppContext) {
        let _theme = TestThemeGuard::acquire();
        set_system_appearance(gpui::WindowAppearance::Dark);
        cx.update(|cx| apply_preference(Preference::System, cx));
        let (_dialog, painted) = open(cx, false);
        cx.run_until_parked();
        assert_eq!(painted.get(), DARK.bg_app);

        // What the portal monitor does on a new color scheme.
        set_system_appearance(gpui::WindowAppearance::Light);
        cx.update(sync);
        cx.run_until_parked();
        assert_eq!(painted.get(), LIGHT.bg_app);
    }
}

#[cfg(test)]
mod system_appearance_tests {
    use super::*;
    use gpui::WindowAppearance::{Dark, Light};

    #[test]
    fn helper_overrides_stale_native_appearance_only_in_system_mode() {
        assert_eq!(
            resolved_palette(Preference::System, Light, Some(false)),
            PaletteId::Dark
        );
        assert_eq!(
            resolved_palette(Preference::System, Dark, Some(true)),
            PaletteId::Light
        );
        assert_eq!(
            resolved_palette(Preference::Dark, Light, Some(true)),
            PaletteId::Dark
        );
        assert_eq!(
            resolved_palette(Preference::Light, Dark, Some(false)),
            PaletteId::Light
        );
    }

    #[test]
    fn missing_helper_preserves_native_fallback() {
        assert_eq!(
            resolved_palette(Preference::System, Light, None),
            PaletteId::Light
        );
        assert_eq!(
            resolved_palette(Preference::System, Dark, None),
            PaletteId::Dark
        );
    }

    #[test]
    fn named_themes_ignore_the_os_appearance() {
        for (preference, palette) in [
            (Preference::Dracula, PaletteId::Dracula),
            (Preference::Americana, PaletteId::Americana),
            (Preference::NeonDusk, PaletteId::NeonDusk),
            (Preference::CherryBlossom, PaletteId::CherryBlossom),
        ] {
            for appearance in [Light, Dark] {
                for system in [None, Some(true), Some(false)] {
                    assert_eq!(resolved_palette(preference, appearance, system), palette);
                }
            }
        }
    }

    #[test]
    fn saved_ids_round_trip_and_keep_their_earlier_meaning() {
        for preference in Preference::ALL {
            assert_eq!(Preference::parse(preference.as_str()), preference);
        }
        // Values written before named themes existed.
        assert_eq!(Preference::parse("system"), Preference::System);
        assert_eq!(Preference::parse("light"), Preference::Light);
        assert_eq!(Preference::parse("dark"), Preference::Dark);
        for unknown in ["", "solarized", "Dracula", "neon_dusk"] {
            assert_eq!(Preference::parse(unknown), Preference::System);
        }
    }
}

/// Whether the active palette is a light one. Translucent overlays,
/// shadows and monochrome marks pick black or white by this; colors pick
/// from the palette itself.
pub fn is_light() -> bool {
    active_palette().is_light()
}

fn active_palette() -> PaletteId {
    PaletteId::from_u8(ACTIVE_PALETTE.load(Ordering::Relaxed))
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
    active_palette().palette()
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
    /// Sidebar text stays softer than body text in both themes.
    text_sidebar: 0xbcbdc8, 0x45454b;
    text_heading: 0xe8e8ed, 0x171717;
    text_secondary: 0xbabccb, 0x525252;
    /// Captions. Light sits between neutral-500 and 600: neutral-400 was
    /// 2.5:1 on white, under the 4.5:1 the contrast test holds text to.
    text_muted: 0x9c9dab, 0x6b6b6b;
    text_faint: 0x5e5f6e, 0xd4d4d4;

    /// Maple coral (maple-500): send button, focus, caret, question cards.
    accent: 0xff9771, 0xff9771;
    /// Hover for accent fills: maple-400 on dark, maple-600 on light.
    accent_hover: 0xffa88a, 0xf67d57;
    /// Text and icons placed on an accent fill (pebble-50).
    on_accent: 0xf7f7f9, 0xf7f7f9;
    /// Soft coral container (maple-100 / a dark coral tint).
    accent_container: 0x3a2118, 0xffe8e0;
    /// Coral for an icon on a coral container: maple-500 on dark,
    /// maple-800 on light, where lighter steps wash out (3:1 or better).
    accent_ink: 0xff9771, 0xd04926;

    status_running: 0xbabccb, 0xa3a3a3;
    status_success: 0x8fa35a, 0x7b8f4a;
    status_error: 0xe07052, 0xd05e41;
    /// Wavy underline under a misspelled word in the composer.
    spell_error: 0xe07052, 0xd05e41;
    /// Amber-700 on light: the dark amber was 2.3:1 on white.
    status_warning: 0xd4a35a, 0xb45309;

    code_text: 0xe8e8ed, 0x171717;
    /// Links take the tertiary Bark scale (bark-300 / bark-500).
    link: 0xc29a8d, 0x9e7469;
    /// A short label's pill, such as the Popular badge: coral on the dark
    /// coral container on dark; on light, where coral read like an alert,
    /// secondary grey on pebble-200, a step darker than a highlighted
    /// row so the pill stays visible on one. 4.5:1 or better.
    badge_fill: 0x3a2118, 0xd1d2dc;
    badge_text: 0xff9771, 0x525252;

    /// Elicitation row fill/border: the coral container and maple-300.
    permission_fill: 0x2a1a14, 0xffe8e0;
    permission_border: 0x784a38, 0xffbaa2;

    /// Panel docked inside the composer (the out-of-usage notice): half a
    /// step above the composer's well.
    bg_composer_tray: 0x222228, 0xf7f7f9;

    user_bubble_border: 0x3a3a44, 0xe8e8ed;

    /// Sidebar chrome: segmented toggle track, row hover, selected row
    /// (pebble-100 / pebble-200 on light).
    bg_sidebar_chrome: 0x1e1e24, 0xffffff;
    bg_sidebar_row_hover: 0x32323a, 0xe8e8ed;
    bg_sidebar_row_selected: 0x3a3a44, 0xd1d2dc;
    bg_sidebar_row_selected_hover: 0x43434e, 0xc5c6d1;
    /// A running task's breathing dot: Pebble 300 on dark, Pebble 500 on
    /// light, where a lighter grey vanished on the selected row.
    sidebar_running: 0xbabccb, 0x8c8d96;
    /// The band that crosses a running task's title on dark (Pebble 50).
    /// Light does not shine: it has no ink brighter than its text.
    sidebar_running_shine: 0xf7f7f9, 0x171717;
    /// The dot of a task that finished while unseen: a clearer success
    /// green than `status_success`'s olive, which read as dull at 8 px.
    sidebar_finished: 0x4fc298, 0x18a44e;

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

    /// High-contrast default/primary action pill (WCAG AAA in both themes,
    /// matching `maple-theme-spec.md`'s inverted button foreground/fill).
    bg_default_button: 0xe8e8ed, 0x171717;
    bg_default_button_hover: 0xf7f7f9, 0x2c2c34;
    bg_default_button_active: 0xd1d2dc, 0x404040;
    text_on_default_button: 0x18181c, 0xf7f7f9;

    /// Usage chart gridlines and axis rule.
    chart_grid: 0x262626, 0xededed;
    chart_axis: 0x3a3a3a, 0xd4d4d4;

    /// Text and icons on a `status_error` fill (destructive buttons, the
    /// recording stop square) and on a `status_success` fill (a completed
    /// plan step). Stock palettes keep the accent foreground they used.
    on_error: 0xf7f7f9, 0xf7f7f9;
    on_success: 0xf7f7f9, 0xf7f7f9;
    /// A switch's knob while it is off, on the off track. When on, the knob
    /// takes `on_accent` on the accent track.
    switch_knob_off: 0xf7f7f9, 0xf7f7f9;
    /// The effort slider's thumb, drawn against the send-gradient fill.
    slider_thumb: 0xf7f7f9, 0xf7f7f9;
    /// Fill of the legacy accent action button behind an `on_accent` label.
    /// It equals the accent except where a theme's accent must stay bright
    /// for focus rings and marks, too bright for a readable label.
    accent_fill: 0xff9771, 0xff9771;
    accent_fill_hover: 0xffa88a, 0xf67d57;
    /// The accent button's fill while pressed, behind its `on_accent` label.
    accent_fill_active: 0xe8633d, 0xe8633d;
    /// The accent hue as body-size text (slash commands, the Manage
    /// integrations row, an Updating badge) on panels, hovered, selected
    /// and pill fills. `accent` itself is for marks, borders and focus
    /// rings. Stock palettes keep the accent they used.
    accent_text: 0xff9771, 0xff9771;
}

// Named themes. Each lists every role, so a new role cannot silently fall
// back to a stock color. Dracula uses the published Dracula palette
// (draculatheme.com) and its darker editor-chrome steps. Americana, Neon
// Dusk and Cherry Blossom follow the first-pass mock-ups, tuned in the
// running app.

const DRACULA: Palette = Palette {
    bg_app: 0x282a36,
    bg_sidebar: 0x21222c,
    bg_elevated: 0x343746,
    bg_sidebar_card: 0x2c2e3b,
    bg_sidebar_pill: 0x3b3e4f,
    bg_input: 0x21222c,
    bg_user_bubble: 0x44475a,
    bg_code_block: 0x191a21,
    bg_tool_card: 0x2c2e3b,
    border: 0x4d5066,
    border_subtle: 0x363949,
    text_primary: 0xeaeae4,
    text_sidebar: 0xd6d7e0,
    text_heading: 0xf8f8f2,
    text_secondary: 0xcfd1e3,
    text_muted: 0xaab0d4,
    text_faint: 0x6272a4,
    accent: 0xbd93f9,
    accent_hover: 0xcaa7fa,
    on_accent: 0x21222c,
    accent_container: 0x3e3558,
    accent_ink: 0xc9a6ff,
    status_running: 0xcfd1e3,
    status_success: 0x50fa7b,
    status_error: 0xff5555,
    spell_error: 0xff5555,
    status_warning: 0xffb86c,
    code_text: 0xf8f8f2,
    link: 0x8be9fd,
    badge_fill: 0x3e3558,
    badge_text: 0xd6bcfc,
    permission_fill: 0x2f2b41,
    permission_border: 0x6e5c9e,
    bg_composer_tray: 0x2c2e3b,
    user_bubble_border: 0x55586f,
    bg_sidebar_chrome: 0x21222c,
    bg_sidebar_row_hover: 0x343746,
    bg_sidebar_row_selected: 0x44475a,
    bg_sidebar_row_selected_hover: 0x4d5066,
    display_text: 0xd6bcfc,
    send_top: 0xff79c6,
    send_bottom: 0xbd93f9,
    bg_title_control: 0x44475a,
    bg_title_control_hover: 0x55586f,
    text_cursor: 0xf8f8f2,
    bg_default_button: 0xf8f8f2,
    bg_default_button_hover: 0xffffff,
    bg_default_button_active: 0xe2e2dc,
    text_on_default_button: 0x21222c,
    chart_grid: 0x44475a,
    chart_axis: 0x6272a4,
    on_error: 0x21222c,
    on_success: 0x21222c,
    switch_knob_off: 0xf8f8f2,
    slider_thumb: 0xf8f8f2,
    accent_fill: 0xbd93f9,
    accent_fill_hover: 0xcaa7fa,
    accent_fill_active: 0xbd93f9,
    accent_text: 0xd6bcfc,
};

/// Navy canvas, lighter blue tool rows, ivory type and red actions. Errors
/// take a light salmon so they read apart from the crimson action fills.
const AMERICANA: Palette = Palette {
    bg_app: 0x14213d,
    bg_sidebar: 0x0f1a31,
    bg_elevated: 0x1d2d4f,
    bg_sidebar_card: 0x182746,
    bg_sidebar_pill: 0x253a63,
    bg_input: 0x111c34,
    bg_user_bubble: 0x233760,
    bg_code_block: 0x0c152a,
    bg_tool_card: 0x213a67,
    border: 0x3a5688,
    border_subtle: 0x263c63,
    text_primary: 0xf3ecdc,
    text_sidebar: 0xe2d9c5,
    text_heading: 0xfbf6ea,
    text_secondary: 0xdcd3bf,
    text_muted: 0xbfb9aa,
    text_faint: 0x5f7090,
    accent: 0xea4d61,
    accent_hover: 0xf06276,
    on_accent: 0xfbf6ea,
    accent_container: 0x3e1d2e,
    accent_ink: 0xff8090,
    status_running: 0xdcd3bf,
    status_success: 0x93d08c,
    status_error: 0xffa58a,
    spell_error: 0xffa58a,
    status_warning: 0xf2c14e,
    code_text: 0xf3ecdc,
    link: 0x9cc4ff,
    badge_fill: 0x3e1d2e,
    badge_text: 0xff9aa6,
    permission_fill: 0x2a2038,
    permission_border: 0x8f3c50,
    bg_composer_tray: 0x1a2a4a,
    user_bubble_border: 0x3a5688,
    bg_sidebar_chrome: 0x0f1a31,
    bg_sidebar_row_hover: 0x1d2d4f,
    bg_sidebar_row_selected: 0x2a4170,
    bg_sidebar_row_selected_hover: 0x334c80,
    display_text: 0xe2d9c5,
    send_top: 0xea4d61,
    send_bottom: 0xb3172f,
    bg_title_control: 0x253a63,
    bg_title_control_hover: 0x2f4777,
    text_cursor: 0xfbf6ea,
    bg_default_button: 0xf3ecdc,
    bg_default_button_hover: 0xfbf6ea,
    bg_default_button_active: 0xe3dac6,
    text_on_default_button: 0x14213d,
    chart_grid: 0x2c4373,
    chart_axis: 0x4d6aa0,
    on_error: 0x14213d,
    on_success: 0x14213d,
    switch_knob_off: 0xfbf6ea,
    slider_thumb: 0xfbf6ea,
    accent_fill: 0xc8243b,
    accent_fill_hover: 0xae1c31,
    accent_fill_active: 0xb3172f,
    accent_text: 0xff9aa6,
};

/// Ink-blue canvas, plum cards, fuchsia actions and focus rings. The
/// fuchsia is bright enough that a focus ring stands out on plum.
const NEON_DUSK: Palette = Palette {
    bg_app: 0x12122a,
    bg_sidebar: 0x0e0e22,
    bg_elevated: 0x2b1b40,
    bg_sidebar_card: 0x24173a,
    bg_sidebar_pill: 0x362352,
    bg_input: 0x161634,
    bg_user_bubble: 0x2f1d47,
    bg_code_block: 0x0b0b1c,
    bg_tool_card: 0x26193e,
    border: 0x4a3366,
    border_subtle: 0x2e2147,
    text_primary: 0xe8e2f4,
    text_sidebar: 0xd2cae4,
    text_heading: 0xf6f1ff,
    text_secondary: 0xcfc5e4,
    text_muted: 0xaea4c9,
    text_faint: 0x5c5378,
    accent: 0xff4fbf,
    accent_hover: 0xff73cc,
    on_accent: 0x12122a,
    accent_container: 0x441a43,
    accent_ink: 0xff7dcf,
    status_running: 0xcfc5e4,
    status_success: 0x5ee6a8,
    status_error: 0xff7070,
    spell_error: 0xff7070,
    status_warning: 0xffc857,
    code_text: 0xf0eaff,
    link: 0x7fcaff,
    badge_fill: 0x441a43,
    badge_text: 0xff96d8,
    permission_fill: 0x2b1532,
    permission_border: 0x93327a,
    bg_composer_tray: 0x1d1a3c,
    user_bubble_border: 0x4a3366,
    bg_sidebar_chrome: 0x0e0e22,
    bg_sidebar_row_hover: 0x231a3d,
    bg_sidebar_row_selected: 0x3b2558,
    bg_sidebar_row_selected_hover: 0x462d68,
    display_text: 0xdcc8f7,
    send_top: 0xff5cc6,
    send_bottom: 0xc42a93,
    bg_title_control: 0x2e2147,
    bg_title_control_hover: 0x3b2b58,
    text_cursor: 0xf6f1ff,
    bg_default_button: 0xf6f1ff,
    bg_default_button_hover: 0xffffff,
    bg_default_button_active: 0xe2d9f2,
    text_on_default_button: 0x12122a,
    chart_grid: 0x3d2a59,
    chart_axis: 0x5c4380,
    on_error: 0x12122a,
    on_success: 0x12122a,
    switch_knob_off: 0xf6f1ff,
    slider_thumb: 0xf6f1ff,
    accent_fill: 0xff4fbf,
    accent_fill_hover: 0xff73cc,
    accent_fill_active: 0xff73cc,
    accent_text: 0xff96d8,
};

/// Blush sidebar, pale pink canvas, plum type and raspberry actions.
/// Secondary text stays dark enough to read on the blush selected row.
const CHERRY_BLOSSOM: Palette = Palette {
    bg_app: 0xfff6f8,
    bg_sidebar: 0xfbe5eb,
    bg_elevated: 0xffffff,
    bg_sidebar_card: 0xfff0f4,
    bg_sidebar_pill: 0xf5d2dc,
    bg_input: 0xffffff,
    bg_user_bubble: 0xfbe1e9,
    bg_code_block: 0xfdeff3,
    bg_tool_card: 0xfdeff3,
    border: 0xedc6d2,
    border_subtle: 0xf6dfe6,
    text_primary: 0x3d1f33,
    text_sidebar: 0x5a2f4b,
    text_heading: 0x2e1426,
    text_secondary: 0x65395a,
    text_muted: 0x7c4d6b,
    text_faint: 0xd9b3c3,
    accent: 0xc2185b,
    accent_hover: 0xa3134c,
    on_accent: 0xfff6f8,
    accent_container: 0xfcd9e5,
    accent_ink: 0xa3134c,
    status_running: 0x7c4d6b,
    status_success: 0x4a7a36,
    status_error: 0xc0392b,
    spell_error: 0xc0392b,
    status_warning: 0xa15c07,
    code_text: 0x3d1f33,
    link: 0x7b3fa0,
    badge_fill: 0xf5d2dc,
    badge_text: 0x65395a,
    permission_fill: 0xfde5ed,
    permission_border: 0xf0a8c0,
    bg_composer_tray: 0xfdeff3,
    user_bubble_border: 0xf0cad6,
    bg_sidebar_chrome: 0xfff6f8,
    bg_sidebar_row_hover: 0xf7d7e0,
    bg_sidebar_row_selected: 0xf0c4d1,
    bg_sidebar_row_selected_hover: 0xeab5c5,
    display_text: 0x65395a,
    send_top: 0xd81b60,
    send_bottom: 0xa8134f,
    bg_title_control: 0xf5d2dc,
    bg_title_control_hover: 0xefc1cf,
    text_cursor: 0x3d1f33,
    bg_default_button: 0x3d1f33,
    bg_default_button_hover: 0x522a45,
    bg_default_button_active: 0x63385a,
    text_on_default_button: 0xfff6f8,
    chart_grid: 0xf0cfda,
    chart_axis: 0xdba7b9,
    on_error: 0xfff6f8,
    on_success: 0xfff6f8,
    switch_knob_off: 0x65395a,
    slider_thumb: 0xffffff,
    accent_fill: 0xc2185b,
    accent_fill_hover: 0xa3134c,
    accent_fill_active: 0xa8134f,
    accent_text: 0x8e1043,
};

/// Translucent selection highlight in text inputs: the palette's accent at
/// a low alpha, a little stronger on light where the pale field washes it
/// out.
pub fn text_selection() -> gpui::Rgba {
    accent_alpha(if is_light() { 0x59 } else { 0x47 })
}

/// Background drawn under selected transcript text: the accent at ~40%.
pub fn transcript_selection() -> gpui::Rgba {
    accent_alpha(0x66)
}

fn accent_alpha(alpha: u8) -> gpui::Rgba {
    gpui::rgba((accent() << 8) | u32::from(alpha))
}

/// Dimming layer behind a modal or the image lightbox. One opacity for
/// every overlay, so a dialog and the lightbox darken the app equally.
pub fn scrim() -> gpui::Rgba {
    gpui::rgba(0x000000a0)
}

/// Opacity of a sign-in form while its request runs.
pub const BUSY_FORM_OPACITY: f32 = 0.7;

/// The busy submit button of a sign-in form: a neutral pill whose label
/// reads at 4.5:1 even with the form at [`BUSY_FORM_OPACITY`].
pub fn busy_button_fill() -> u32 {
    busy_pair(palette()).0
}

pub fn busy_button_text() -> u32 {
    busy_pair(palette()).1
}

/// Fill and label of a busy submit, shared with the contrast test.
fn busy_pair(palette: &Palette) -> (u32, u32) {
    (palette.bg_sidebar_pill, palette.text_primary)
}

/// Icons drawn over the dimming [`scrim`], such as the lightbox close
/// button: the scrim is black in every palette, so this stays light.
pub fn on_scrim() -> u32 {
    0xf7f7f9
}

/// Translucent fill for hover states and inline code over any surface.
pub fn overlay_hover() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 0., 0.06)
    } else {
        gpui::hsla(0., 0., 1., 0.08)
    }
}

/// Translucent fill for a pressed control over any surface: one step past
/// [`overlay_hover`], so a press always reads darker (lighter on dark)
/// than the hover and the row beneath, whatever that row's own state.
pub fn overlay_press() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 0., 0.11)
    } else {
        gpui::hsla(0., 0., 1., 0.14)
    }
}

/// Alternate-row tint in markdown tables: a whisper of the hover overlay,
/// so a long table reads row by row without stripes competing with the
/// text.
pub fn table_stripe() -> gpui::Hsla {
    if is_light() {
        gpui::hsla(0., 0., 0., 0.025)
    } else {
        gpui::hsla(0., 0., 1., 0.03)
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

#[cfg(test)]
mod contrast_tests {
    use super::*;

    /// WCAG relative luminance of a 24-bit color.
    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let value = f64::from((color >> shift) & 0xff) / 255.;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Every text role must read on every resting surface in every shipped
    /// palette: 4.5:1 for body roles (WCAG AA), 3:1 for status and link
    /// colors, which carry short labels. `text_faint` is decorative and not
    /// held to a ratio. Hover fills are transient and not checked; the
    /// selected sidebar row persists, so its text is.
    #[test]
    fn text_roles_read_on_every_surface() {
        let mut failures = Vec::new();
        for id in PaletteId::ALL {
            let name = format!("{id:?}");
            let palette = id.palette();
            let surfaces = [
                ("bg_app", palette.bg_app),
                ("bg_sidebar", palette.bg_sidebar),
                ("bg_elevated", palette.bg_elevated),
                ("bg_input", palette.bg_input),
                ("bg_sidebar_card", palette.bg_sidebar_card),
                ("bg_tool_card", palette.bg_tool_card),
                ("bg_composer_tray", palette.bg_composer_tray),
            ];
            let body = [
                ("text_primary", palette.text_primary),
                ("text_heading", palette.text_heading),
                ("text_secondary", palette.text_secondary),
                ("text_muted", palette.text_muted),
                ("code_text", palette.code_text),
            ];
            let labels = [
                ("status_success", palette.status_success),
                ("status_error", palette.status_error),
                ("status_warning", palette.status_warning),
                ("link", palette.link),
            ];
            let ratio = contrast(palette.accent_ink, palette.accent_container);
            if ratio < 3. {
                failures.push(format!(
                    "{name}: accent_ink on accent_container is {ratio:.2}:1"
                ));
            }
            for (surface, fill) in surfaces {
                for (role, color) in body {
                    let ratio = contrast(color, fill);
                    if ratio < 4.5 {
                        failures.push(format!("{name}: {role} on {surface} is {ratio:.2}:1"));
                    }
                }
                for (role, color) in labels {
                    let ratio = contrast(color, fill);
                    if ratio < 3. {
                        failures.push(format!("{name}: {role} on {surface} is {ratio:.2}:1"));
                    }
                }
            }
            let ratio = contrast(palette.badge_text, palette.badge_fill);
            if ratio < 4.5 {
                failures.push(format!("{name}: badge_text on badge_fill is {ratio:.2}:1"));
            }
            for (role, color) in [
                ("text_sidebar", palette.text_sidebar),
                ("text_primary", palette.text_primary),
            ] {
                let ratio = contrast(color, palette.bg_sidebar_row_selected);
                if ratio < 4.5 {
                    failures.push(format!(
                        "{name}: {role} on bg_sidebar_row_selected is {ratio:.2}:1"
                    ));
                }
            }
            for (label, fg, bg) in [
                (
                    "default_button",
                    palette.text_on_default_button,
                    palette.bg_default_button,
                ),
                (
                    "default_button_hover",
                    palette.text_on_default_button,
                    palette.bg_default_button_hover,
                ),
                (
                    "default_button_active",
                    palette.text_on_default_button,
                    palette.bg_default_button_active,
                ),
            ] {
                let ratio = contrast(fg, bg);
                if ratio < 7. {
                    failures.push(format!("{name}: {label} is {ratio:.2}:1 (< 7.0:1)"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// `color` drawn at `alpha` over `under`, per channel, as gpui paints
    /// each primitive of an element with reduced opacity.
    fn over(color: u32, under: u32, alpha: f32) -> u32 {
        let channel = |shift: u32| {
            let top = f64::from((color >> shift) & 0xff);
            let bottom = f64::from((under >> shift) & 0xff);
            ((top * f64::from(alpha) + bottom * (1. - f64::from(alpha))).round() as u32) << shift
        };
        channel(16) | channel(8) | channel(0)
    }

    /// The busy submit of a sign-in form is drawn inside a card at
    /// `BUSY_FORM_OPACITY`: the card over the canvas, the pill over the card
    /// and the label over the pill, each faded. Its label still reads at
    /// 4.5:1 in every palette.
    #[test]
    fn busy_sign_in_label_reads_inside_the_faded_form() {
        let mut failures = Vec::new();
        for id in PaletteId::ALL {
            let p = id.palette();
            let (fill, text) = busy_pair(p);
            let card = over(p.bg_elevated, p.bg_app, BUSY_FORM_OPACITY);
            let pill = over(fill, card, BUSY_FORM_OPACITY);
            let label = over(text, pill, BUSY_FORM_OPACITY);
            let ratio = contrast(label, pill);
            if ratio < 4.5 {
                failures.push(format!("{id:?}: busy label is {ratio:.2}:1"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Foreground and fill pairs as the app draws them. Labels hold 4.5:1;
    /// icons, marks, knobs and focus rings hold 3:1 (WCAG non-text
    /// contrast); chart rules only need to separate from their card.
    ///
    /// The stock palettes predate this check. Their known shortfalls (the
    /// coral fill under a light label, coral text on light surfaces, the pale
    /// light-theme track and running label, the near-invisible dark chart
    /// grid) are listed in
    /// `STOCK_EXEMPT` rather than changed here; every named theme meets
    /// every floor.
    #[test]
    fn rendered_pairs_meet_their_floors() {
        const TEXT: f64 = 4.5;
        const MARK: f64 = 3.;
        const STOCK_EXEMPT: &[(PaletteId, &str)] = &[
            (PaletteId::Dark, "destructive button label"),
            (PaletteId::Dark, "accent button label"),
            (PaletteId::Dark, "accent button label (hover)"),
            (PaletteId::Dark, "accent button label (pressed)"),
            (PaletteId::Dark, "completed plan check"),
            (PaletteId::Dark, "recording stop square"),
            (PaletteId::Dark, "send arrow (top)"),
            (PaletteId::Dark, "checkbox mark"),
            (PaletteId::Dark, "switch knob (on)"),
            (PaletteId::Dark, "chart grid"),
            (PaletteId::Dark, "chart axis"),
            (PaletteId::Light, "destructive button label"),
            (PaletteId::Light, "accent button label"),
            (PaletteId::Light, "accent button label (hover)"),
            (PaletteId::Light, "accent button label (pressed)"),
            (PaletteId::Light, "accent text on bg_elevated"),
            (PaletteId::Light, "accent text on bg_sidebar_row_hover"),
            (PaletteId::Light, "accent text on bg_sidebar_row_selected"),
            (PaletteId::Light, "accent text on bg_sidebar_pill"),
            (PaletteId::Light, "running label on bg_tool_card"),
            (PaletteId::Light, "running label on bg_app"),
            (PaletteId::Light, "running label on bg_elevated"),
            (PaletteId::Light, "send arrow (top)"),
            (PaletteId::Light, "checkbox mark"),
            (PaletteId::Light, "switch knob (off)"),
            (PaletteId::Light, "switch knob (on)"),
            (PaletteId::Light, "focus ring on bg_app"),
            (PaletteId::Light, "focus ring on bg_elevated"),
            (PaletteId::Light, "focus ring on bg_tool_card"),
            (PaletteId::Light, "focus ring on bg_user_bubble"),
            (PaletteId::Light, "focus ring on bg_input"),
            (PaletteId::Light, "focus ring on bg_sidebar"),
            (PaletteId::Light, "chart grid"),
            (PaletteId::Light, "chart axis"),
        ];
        let mut failures = Vec::new();
        let mut exempt_used = Vec::new();
        for id in PaletteId::ALL {
            let p = id.palette();
            let mut pairs: Vec<(String, u32, u32, f64)> = vec![
                // widgets::danger_button
                (
                    "destructive button label".into(),
                    p.on_error,
                    p.status_error,
                    TEXT,
                ),
                // login submit while signing in, opaque; the faded form is
                // busy_sign_in_label_reads_inside_the_faded_form
                (
                    "busy sign-in label".into(),
                    busy_pair(p).1,
                    busy_pair(p).0,
                    TEXT,
                ),
                // widgets::primary_button
                (
                    "accent button label".into(),
                    p.on_accent,
                    p.accent_fill,
                    TEXT,
                ),
                (
                    "accent button label (hover)".into(),
                    p.on_accent,
                    p.accent_fill_hover,
                    TEXT,
                ),
                // transcript plan step, composer dictation, send button
                (
                    "completed plan check".into(),
                    p.on_success,
                    p.status_success,
                    MARK,
                ),
                (
                    "recording stop square".into(),
                    p.on_error,
                    p.status_error,
                    MARK,
                ),
                ("send arrow (top)".into(), p.on_accent, p.send_top, MARK),
                (
                    "send arrow (bottom)".into(),
                    p.on_accent,
                    p.send_bottom,
                    MARK,
                ),
                // widgets::checkbox and widgets::switch_track
                ("checkbox mark".into(), p.on_accent, p.accent, MARK),
                (
                    "switch knob (off)".into(),
                    p.switch_knob_off,
                    p.bg_sidebar_row_selected,
                    MARK,
                ),
                ("switch knob (on)".into(), p.on_accent, p.accent, MARK),
                // The effort slider's thumb sits where the fill meets the
                // track; it must stand out from at least one of them. The
                // pair listed is whichever it contrasts with more.
                if contrast(p.slider_thumb, p.send_bottom)
                    >= contrast(p.slider_thumb, p.bg_sidebar_pill)
                {
                    ("slider thumb".into(), p.slider_thumb, p.send_bottom, MARK)
                } else {
                    (
                        "slider thumb".into(),
                        p.slider_thumb,
                        p.bg_sidebar_pill,
                        MARK,
                    )
                },
                // usage chart rules on the chart card
                ("chart grid".into(), p.chart_grid, p.bg_elevated, 1.25),
                ("chart axis".into(), p.chart_axis, p.bg_elevated, 1.5),
            ];
            // widgets::primary_button while pressed
            pairs.push((
                "accent button label (pressed)".into(),
                p.on_accent,
                p.accent_fill_active,
                TEXT,
            ));
            for (surface, fill) in [
                // slash palette and popup menu panels
                ("bg_elevated", p.bg_elevated),
                // hovered or keyboard-highlighted row
                ("bg_sidebar_row_hover", p.bg_sidebar_row_hover),
                // selected slash command, pressed menu row
                ("bg_sidebar_row_selected", p.bg_sidebar_row_selected),
                // widgets::status_badge (Remote computers' Updating)
                ("bg_sidebar_pill", p.bg_sidebar_pill),
            ] {
                pairs.push((
                    format!("accent text on {surface}"),
                    p.accent_text,
                    fill,
                    TEXT,
                ));
            }
            for (surface, fill) in [
                ("bg_tool_card", p.bg_tool_card),
                ("bg_app", p.bg_app),
                ("bg_elevated", p.bg_elevated),
            ] {
                // tool status label
                pairs.push((
                    format!("running label on {surface}"),
                    p.status_running,
                    fill,
                    TEXT,
                ));
            }
            for (surface, fill) in [
                ("bg_app", p.bg_app),
                ("bg_elevated", p.bg_elevated),
                ("bg_tool_card", p.bg_tool_card),
                ("bg_user_bubble", p.bg_user_bubble),
                ("bg_input", p.bg_input),
                ("bg_sidebar", p.bg_sidebar),
            ] {
                // widgets::focus_ring and the composer's focus border
                pairs.push((format!("focus ring on {surface}"), p.accent, fill, MARK));
            }
            for (label, fg, bg, floor) in pairs {
                let ratio = contrast(fg, bg);
                if ratio >= floor {
                    continue;
                }
                if STOCK_EXEMPT.contains(&(id, label.as_str())) {
                    exempt_used.push((id, label));
                } else {
                    failures.push(format!("{id:?}: {label} is {ratio:.2}:1 (< {floor}:1)"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        // An exemption that no longer fails should be removed from the list.
        for (id, label) in STOCK_EXEMPT {
            assert!(
                exempt_used
                    .iter()
                    .any(|(used, used_label)| used == id && used_label == label),
                "{id:?}: {label} now passes; drop its exemption"
            );
        }
    }
}
