//! Chart colors, from the web Usage page's `.usage-viz` variables, which
//! were validated against light and dark cards (adjacent color-vision ΔE of
//! at least 8). Coral stays with the plan meter and Chat; models never use
//! it.

use super::model::{Origin, SeriesKey, Split, TokenKind};
use crate::ui::theme;

struct Palette {
    series: [u32; 7],
    more_models: u32,
    chat: u32,
    work: u32,
    proxy: u32,
    api: u32,
    plan: u32,
    extra: u32,
    input: u32,
    cached: u32,
    output: u32,
    grid: u32,
    axis: u32,
}

const LIGHT: Palette = Palette {
    series: [
        0x2f7ed8, 0x1a9e7a, 0xe09b12, 0x7a5cc8, 0xd2578f, 0x8d0da0, 0x237915,
    ],
    more_models: 0xa3a3a3,
    chat: 0xe5683f,
    work: 0x0e9fb3,
    proxy: 0x7f9a1a,
    api: 0x3f3f46,
    plan: 0x8a8b9a,
    extra: 0xe5683f,
    input: 0x64748b,
    cached: 0x94a3b8,
    output: 0x334155,
    grid: 0xededed,
    axis: 0xd4d4d4,
};

const DARK: Palette = Palette {
    series: [
        0x3987e5, 0x199e70, 0xc98500, 0x9085e9, 0xd55181, 0xe198e1, 0x5ae25a,
    ],
    more_models: 0x6b6b6b,
    chat: 0xdd6236,
    work: 0x17a2b8,
    proxy: 0x899d1b,
    api: 0xd4d4d8,
    plan: 0x8a8b9a,
    extra: 0xdd6236,
    input: 0x7c8ba1,
    cached: 0x475569,
    output: 0xcbd5e1,
    grid: 0x262626,
    axis: 0x3a3a3a,
};

fn palette() -> &'static Palette {
    if theme::is_light() { &LIGHT } else { &DARK }
}

pub(super) fn series(key: SeriesKey) -> u32 {
    let palette = palette();
    match key {
        SeriesKey::Model(slot) => palette.series[slot % palette.series.len()],
        SeriesKey::MoreModels => palette.more_models,
        SeriesKey::Origin(origin) => self::origin(origin),
        SeriesKey::Plan => palette.plan,
        SeriesKey::Extra => palette.extra,
        SeriesKey::Token(kind) => token(kind),
    }
}

pub(super) fn origin(origin: Origin) -> u32 {
    let palette = palette();
    match origin {
        Origin::Chat => palette.chat,
        Origin::Work => palette.work,
        Origin::Proxy => palette.proxy,
        Origin::Api => palette.api,
    }
}

pub(super) fn token(kind: TokenKind) -> u32 {
    let palette = palette();
    match kind {
        TokenKind::Input => palette.input,
        TokenKind::Cached => palette.cached,
        TokenKind::Output => palette.output,
    }
}

pub(super) fn model(model: &str) -> u32 {
    series(
        super::model::model_slot(model)
            .map(SeriesKey::Model)
            .unwrap_or(SeriesKey::MoreModels),
    )
}

/// Same color the chart uses when `peers` are the models drawn beside this one.
pub(super) fn model_among(model: &str, peers: &[&str]) -> u32 {
    match super::model::color_slot(model, peers) {
        Some(slot) => series(SeriesKey::Model(slot)),
        None => palette().more_models,
    }
}

/// Follow the model chart even when the selected metric leaves it empty.
/// With another split, retain the table's stable model highlights.
pub(super) fn model_table_color(model: &str, peers: &[&str], split: Split) -> u32 {
    if split == Split::Model {
        model_among(model, peers)
    } else {
        self::model(model)
    }
}

pub(super) fn grid() -> u32 {
    palette().grid
}

pub(super) fn axis() -> u32 {
    palette().axis
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_colors_follow_the_selected_model_chart() {
        let peers = ["glm-5-3", "llama"];
        assert_eq!(
            model_table_color("llama", &peers, Split::Model),
            series(SeriesKey::Model(0))
        );
        assert_eq!(
            model_table_color("glm-5-3", &[], Split::Model),
            series(SeriesKey::MoreModels),
            "zero usage for the selected metric stays gray"
        );
        for split in [Split::Origin, Split::Pool, Split::Token] {
            assert_eq!(
                model_table_color("glm-5-3", &peers, split),
                series(SeriesKey::Model(2)),
                "other charts leave the table's highlights intact"
            );
        }
    }
}
