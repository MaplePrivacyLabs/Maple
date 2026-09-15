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
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
];

/// Body font for the whole app (brand kit: `--font-body`).
pub const FONT_BODY: &str = "Manrope";
/// Display font for headings (brand kit: `--font-display`).
pub const FONT_DISPLAY: &str = "Array";
/// Code font (brand kit: `--font-mono`). Bundled, so every platform
/// renders code the same way.
pub const FONT_MONO: &str = "Geist Mono";

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
