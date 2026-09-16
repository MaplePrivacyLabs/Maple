//! Chat reading typography: family, size, and line-height that apply to
//! the transcript and composer. Chrome stays Manrope 14; this module is
//! the user-facing reading face.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{Div, FontFallbacks, prelude::*, px, relative};

use crate::assets;

/// Chat faces the user can pick in Settings → Chat appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFontFamily {
    /// SF Pro / Segoe UI / the platform UI face. Default.
    System,
    /// Named SF Pro Text. Same design as System on macOS when the
    /// `.SystemUIFont` mapping works; useful when that mapping does not.
    SfPro,
    /// Brand kit body face.
    Manrope,
    /// Geist Sans, paired with the bundled Geist Mono.
    Geist,
    /// New York / Georgia / Times.
    Serif,
}

pub const CHAT_FONT_SIZE_MIN: u8 = 13;
pub const CHAT_FONT_SIZE_MAX: u8 = 18;
pub const DEFAULT_CHAT_FONT_SIZE: u8 = 14;
/// Matching the Research web app's chat line-height.
const CHAT_LINE_HEIGHT: f32 = 1.65;

const FAMILY_SYSTEM: u8 = 0;
const FAMILY_SF_PRO: u8 = 1;
const FAMILY_MANROPE: u8 = 2;
const FAMILY_GEIST: u8 = 3;
const FAMILY_SERIF: u8 = 4;

static FAMILY: AtomicU8 = AtomicU8::new(FAMILY_SYSTEM);
static SIZE: AtomicU8 = AtomicU8::new(DEFAULT_CHAT_FONT_SIZE);

impl ChatFontFamily {
    pub const ALL: [Self; 5] = [
        Self::System,
        Self::SfPro,
        Self::Manrope,
        Self::Geist,
        Self::Serif,
    ];

    /// Unknown values fall back to System, the reading default.
    pub fn parse(value: &str) -> Self {
        match value {
            "sf-pro" => Self::SfPro,
            "manrope" => Self::Manrope,
            "geist" => Self::Geist,
            "serif" => Self::Serif,
            _ => Self::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::SfPro => "sf-pro",
            Self::Manrope => "manrope",
            Self::Geist => "geist",
            Self::Serif => "serif",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::SfPro => "SF Pro",
            Self::Manrope => "Manrope",
            Self::Geist => "Geist",
            Self::Serif => "Serif",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            Self::System => "The familiar interface font for your device.",
            Self::SfPro => "Apple's named SF Pro Text. Try this if System looks like Manrope.",
            Self::Manrope => "Maple's brand face, with a compact modern feel.",
            Self::Geist => "A lighter grotesque that pairs with Geist Mono.",
            Self::Serif => "A traditional reading style with distinct letterforms.",
        }
    }

    fn family_name(self) -> &'static str {
        match self {
            Self::System => assets::FONT_SYSTEM,
            Self::SfPro => assets::FONT_SF_PRO,
            Self::Manrope => assets::FONT_BODY,
            Self::Geist => assets::FONT_GEIST,
            Self::Serif => assets::FONT_SERIF,
        }
    }

    fn fallbacks(self) -> Option<FontFallbacks> {
        match self {
            Self::System => Some(FontFallbacks::from_fonts(vec![
                assets::FONT_SF_PRO.into(),
                "SF Pro".into(),
                "Helvetica Neue".into(),
            ])),
            Self::SfPro => Some(FontFallbacks::from_fonts(vec![
                "SF Pro".into(),
                assets::FONT_SYSTEM.into(),
                "Helvetica Neue".into(),
            ])),
            Self::Serif => Some(FontFallbacks::from_fonts(vec![
                "Georgia".into(),
                "Times New Roman".into(),
                "Times".into(),
            ])),
            _ => None,
        }
    }
}

pub fn clamp_chat_font_size(size: u8) -> u8 {
    size.clamp(CHAT_FONT_SIZE_MIN, CHAT_FONT_SIZE_MAX)
}

/// Install the live reading face. Chat render reads this every frame, so
/// a settings change does not need to rebuild cached markdown.
pub fn apply(family: ChatFontFamily, size: u8) {
    FAMILY.store(
        match family {
            ChatFontFamily::System => FAMILY_SYSTEM,
            ChatFontFamily::SfPro => FAMILY_SF_PRO,
            ChatFontFamily::Manrope => FAMILY_MANROPE,
            ChatFontFamily::Geist => FAMILY_GEIST,
            ChatFontFamily::Serif => FAMILY_SERIF,
        },
        Ordering::Relaxed,
    );
    SIZE.store(clamp_chat_font_size(size), Ordering::Relaxed);
}

pub fn chat_font_family() -> ChatFontFamily {
    match FAMILY.load(Ordering::Relaxed) {
        FAMILY_SF_PRO => ChatFontFamily::SfPro,
        FAMILY_MANROPE => ChatFontFamily::Manrope,
        FAMILY_GEIST => ChatFontFamily::Geist,
        FAMILY_SERIF => ChatFontFamily::Serif,
        _ => ChatFontFamily::System,
    }
}

pub fn chat_font_size() -> u8 {
    clamp_chat_font_size(SIZE.load(Ordering::Relaxed))
}

/// Markdown heading size relative to the live chat body size, matching
/// the Research web app's em scale.
pub fn heading_size(level: pulldown_cmark::HeadingLevel) -> gpui::Pixels {
    let body = f32::from(chat_font_size());
    let em = match level {
        pulldown_cmark::HeadingLevel::H1 => 2.0,
        pulldown_cmark::HeadingLevel::H2 => 1.5,
        pulldown_cmark::HeadingLevel::H3 => 1.25,
        pulldown_cmark::HeadingLevel::H4 => 1.0,
        pulldown_cmark::HeadingLevel::H5 | pulldown_cmark::HeadingLevel::H6 => 0.875,
    };
    px((body * em).round())
}

/// Emphasis in chat markdown: SemiBold, not Bold. Manrope 700 at body
/// size fills in counters; the brand kit reserves 700 for micro captions.
pub fn emphasis_weight() -> gpui::FontWeight {
    gpui::FontWeight::SEMIBOLD
}

/// Apply only the face, leaving size to the caller. Used by the font
/// picker so each option renders in itself.
pub fn with_family<T: Styled>(el: T, family: ChatFontFamily) -> T {
    let named = el.font_family(family.family_name());
    match family.fallbacks() {
        Some(fallbacks) => {
            let mut font = gpui::font(family.family_name());
            font.fallbacks = Some(fallbacks);
            named.font(font)
        }
        None => named,
    }
}

/// Apply the live chat face, size, and 1.65 line-height to a container.
/// Children inherit, so wrap transcript messages and the composer input.
pub fn chat_reading(el: Div) -> Div {
    with_family(el, chat_font_family())
        .text_size(px(f32::from(chat_font_size())))
        .line_height(relative(CHAT_LINE_HEIGHT))
}

#[cfg(test)]
mod tests {
    use super::*;

    static APPLY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            apply(ChatFontFamily::System, DEFAULT_CHAT_FONT_SIZE);
        }
    }

    #[test]
    fn unknown_family_is_system() {
        assert_eq!(ChatFontFamily::parse("system"), ChatFontFamily::System);
        assert_eq!(ChatFontFamily::parse(""), ChatFontFamily::System);
        assert_eq!(ChatFontFamily::parse("comic-sans"), ChatFontFamily::System);
        assert_eq!(ChatFontFamily::parse("sf-pro"), ChatFontFamily::SfPro);
        assert_eq!(ChatFontFamily::parse("manrope"), ChatFontFamily::Manrope);
        assert_eq!(ChatFontFamily::parse("geist"), ChatFontFamily::Geist);
        assert_eq!(ChatFontFamily::parse("serif"), ChatFontFamily::Serif);
    }

    #[test]
    fn size_clamps_to_the_web_range() {
        assert_eq!(clamp_chat_font_size(12), 13);
        assert_eq!(clamp_chat_font_size(15), 15);
        assert_eq!(clamp_chat_font_size(19), 18);
    }

    #[test]
    fn apply_is_visible_to_readers() {
        let _guard = APPLY_LOCK.lock();
        let _restore = Restore;
        apply(ChatFontFamily::Geist, 17);
        assert_eq!(chat_font_family(), ChatFontFamily::Geist);
        assert_eq!(chat_font_size(), 17);
    }

    #[test]
    fn heading_scale_follows_body_size() {
        let _guard = APPLY_LOCK.lock();
        let _restore = Restore;
        apply(ChatFontFamily::System, 15);
        assert_eq!(heading_size(pulldown_cmark::HeadingLevel::H1), px(30.));
        assert_eq!(heading_size(pulldown_cmark::HeadingLevel::H2), px(23.));
        assert_eq!(heading_size(pulldown_cmark::HeadingLevel::H3), px(19.));
    }
}
