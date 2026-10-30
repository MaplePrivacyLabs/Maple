//! Lucide-style line icons from the embedded asset set. The SVG is drawn
//! as a mask, so the color comes from `text_color` and the geometry from
//! the file.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use gpui::{AnyElement, ElementId, IntoElement, Pixels, SharedString, Svg, prelude::*, svg};

use super::motion;

/// Asset paths built once per icon name. Render functions ask for icons
/// by name on every frame; without this each call formatted and
/// allocated a new path string.
fn icon_path(name: &'static str) -> SharedString {
    static PATHS: OnceLock<RwLock<HashMap<&'static str, SharedString>>> = OnceLock::new();
    let paths = PATHS.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(cache) = paths.read()
        && let Some(path) = cache.get(name)
    {
        return path.clone();
    }
    let path = SharedString::from(format!("icons/{name}.svg"));
    if let Ok(mut cache) = paths.write() {
        cache.insert(name, path.clone());
    }
    path
}

/// An icon element sized to a square of `size` and filled with `color`.
pub fn icon(name: &'static str, size: Pixels, color: u32) -> Svg {
    svg()
        .path(icon_path(name))
        .size(size)
        .flex_none()
        .text_color(gpui::rgb(color))
}

/// The Maple wordmark at the given height; the width follows the 248:50
/// aspect ratio of the artwork.
pub fn wordmark(height: Pixels, color: u32) -> Svg {
    svg()
        .path("icons/maple-wordmark.svg")
        .h(height)
        .w(wordmark_width(height))
        .flex_none()
        .text_color(gpui::rgb(color))
}

/// The wordmark's width at `height`.
pub fn wordmark_width(height: Pixels) -> Pixels {
    height * (248. / 50.)
}

/// A `loader-circle` icon that turns once per second while it is shown.
/// `id` must be unique among the animations on screen.
pub fn spinner(id: &str, size: Pixels, color: u32) -> AnyElement {
    spinner_with_id(SharedString::from(format!("spinner-{id}")), size, color)
}

/// One turn of the spinner.
const SPINNER_TURN: Duration = Duration::from_secs(1);

/// `spinner` with a prebuilt element id, for render paths that must not
/// format strings per frame. Runs on the shared low-rate clock in
/// `motion`, so any number of spinners cost one repaint per interval.
pub fn spinner_with_id(id: SharedString, size: Pixels, color: u32) -> AnyElement {
    motion::ticker(ElementId::Name(id), SPINNER_TURN, move |phase| {
        icon("loader-circle", size, color)
            .with_transformation(gpui::Transformation::rotate(gpui::radians(
                phase * std::f32::consts::TAU,
            )))
            .into_any_element()
    })
    .into_any_element()
}
