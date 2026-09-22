//! Embedded assets: brand fonts and SVG icons. Files live in `app/assets`
//! and are compiled into the binary so the app has no runtime asset path.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        const ASSETS: &[(&str, &[u8])] = &[
            $(($path, include_bytes!(concat!("../assets/", $path))),)*
        ];
    };
}

assets!(
    "icons/archive-restore.svg",
    "icons/archive.svg",
    "icons/arrow-up.svg",
    "icons/check.svg",
    "icons/chevron-down.svg",
    "icons/chevron-right.svg",
    // Claude mark (simple-icons, CC0) for the Claude Code integration card.
    "icons/claude-mark.svg",
    "icons/copy.svg",
    // Contrast-safe partner marks published at https://cua.ai/branding.
    "icons/cua-mark-black.svg",
    "icons/cua-mark-white.svg",
    "icons/ellipsis.svg",
    "icons/folder-open.svg",
    "icons/folder-plus.svg",
    "icons/folder.svg",
    "icons/globe.svg",
    "icons/image.svg",
    "icons/loader-circle.svg",
    "icons/lock.svg",
    "icons/mic.svg",
    "icons/maple-wordmark.svg",
    "icons/maximize-2.svg",
    "icons/minimize-2.svg",
    // OpenAI mark (simple-icons, CC0) for the Codex integration card.
    "icons/openai-mark.svg",
    "icons/paperclip.svg",
    "icons/pin.svg",
    "icons/plus.svg",
    "icons/puzzle.svg",
    "icons/search.svg",
    "icons/panel-left.svg",
    "icons/pencil.svg",
    "icons/plug.svg",
    "icons/settings.svg",
    "icons/shield-check.svg",
    "icons/square-pen.svg",
    "icons/square.svg",
    "icons/trash-2.svg",
    "icons/undo-2.svg",
    "icons/users.svg",
    "icons/volume-2.svg",
    "icons/x.svg",
    "icons/zap.svg",
);

/// Font files registered with the text system at startup.
pub const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Manrope-Regular.ttf"),
    include_bytes!("../assets/fonts/Manrope-Medium.ttf"),
    include_bytes!("../assets/fonts/Manrope-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Manrope-Bold.ttf"),
    include_bytes!("../assets/fonts/Array-Regular.otf"),
    include_bytes!("../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-Bold.ttf"),
    include_bytes!("../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
];

/// Body font for chrome (brand kit: `--font-body`).
pub const FONT_BODY: &str = "Manrope";
/// Display font for headings (brand kit: `--font-display`).
pub const FONT_DISPLAY: &str = "Array";
/// Code font (brand kit: `--font-mono`). Bundled static Regular / Medium
/// / SemiBold, so markdown strong and inline code keep their weight.
pub const FONT_MONO: &str = "Geist Mono";
/// Optional chat reading face. Bundled static Regular through Bold;
/// gpui/font-kit does not apply variable `wght` axes.
pub const FONT_GEIST: &str = "Geist";
/// Platform UI font. GPUI maps this to SF Pro on macOS, Segoe UI on
/// Windows, and the desktop default elsewhere.
pub const FONT_SYSTEM: &str = ".SystemUIFont";
/// Chat serif option. Georgia is installed on stock macOS; New York is
/// not, and gpui fallbacks cannot rescue a missing primary family.
pub const FONT_SERIF: &str = "Georgia";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
