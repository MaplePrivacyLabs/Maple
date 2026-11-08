//! Localized text for the Usage page: dates, request times, series and
//! metric names, and the chart tooltips, which are built when they show.

use super::*;

impl SettingsScreen {
    // ---- Dates ---------------------------------------------------------------

    /// `Oct 14`.
    pub(super) fn short_date(&self, date: NaiveDate) -> SharedString {
        self.messages.get(
            "settings-usage-short-date",
            [
                ("month", month_short(date.month()).to_string().into()),
                ("day", date.day().to_string().into()),
            ],
        )
    }

    /// `October 2026`.
    pub(super) fn month_year(&self, year: i32, month: u32) -> SharedString {
        self.messages.get(
            "settings-usage-month-year",
            [
                ("month", month_long(month).to_string().into()),
                ("year", year.to_string().into()),
            ],
        )
    }

    /// `Oct 14 – Nov 13`, with years when they differ.
    pub(super) fn date_span(&self, start: NaiveDate, end: NaiveDate) -> SharedString {
        if start.year() == end.year() {
            return format!("{} – {}", self.short_date(start), self.short_date(end)).into();
        }
        format!(
            "{} – {}",
            super::super::presentation::date(start, &self.messages),
            super::super::presentation::date(end, &self.messages)
        )
        .into()
    }

    /// The range under "Usage in this range", as on the web: `Oct 10 –
    /// Nov 8`, `Nov 1 – Nov 8, month to date` or `September 2026`.
    pub(super) fn usage_range_label(&self, range: UsageRange, today: NaiveDate) -> SharedString {
        let dates = range.dates(today);
        let span = match (dates.first(), dates.last()) {
            (Some(first), Some(last)) => self.date_span(*first, *last),
            _ => SharedString::default(),
        };
        match range {
            UsageRange::Last30Days => span,
            UsageRange::Month { .. } if range.is_current_month(today) => self.messages.get(
                "settings-usage-range-month-to-date",
                [("dates", span.to_string().into())],
            ),
            UsageRange::Month { year, month } => self.month_year(year, month),
        }
    }

    /// `Today 2:41 PM`, `Yesterday 9:05 AM`, `Oct 20, 3:05 PM`.
    pub(super) fn request_time(
        &self,
        at: Option<chrono::DateTime<chrono::Local>>,
        now: chrono::DateTime<chrono::Local>,
    ) -> SharedString {
        let Some(at) = at else {
            return "—".into();
        };
        let clock = if crate::ui::i18n::current_locale().language.as_str() == "en" {
            at.format("%-I:%M %p").to_string()
        } else {
            at.format("%H:%M").to_string()
        };
        let days = (now.date_naive() - at.date_naive()).num_days();
        match days {
            0 => self
                .messages
                .get("settings-usage-time-today", [("time", clock.into())]),
            1 => self
                .messages
                .get("settings-usage-time-yesterday", [("time", clock.into())]),
            _ if at.year() == now.year() => self.messages.get(
                "settings-usage-time-date",
                [
                    ("date", self.short_date(at.date_naive()).to_string().into()),
                    ("time", clock.into()),
                ],
            ),
            _ => self.messages.get(
                "settings-usage-time-date",
                [
                    (
                        "date",
                        super::super::presentation::date(at.date_naive(), &self.messages)
                            .to_string()
                            .into(),
                    ),
                    ("time", clock.into()),
                ],
            ),
        }
    }
}
// ---- Tooltips, built only when they show -------------------------------------

pub(super) fn day_tip(data: Option<&UsageData>, index: usize, metric: Metric) -> Tip {
    let empty = Tip {
        title: SharedString::default(),
        headline: None,
        rows: Vec::new(),
        notes: Vec::new(),
    };
    let Some(range) = data.and_then(|data| data.range.as_ref()) else {
        return empty;
    };
    let Some(day) = range.chart.days.get(index) else {
        return empty;
    };
    let messages = crate::ui::i18n::MessageCache::default();
    let format = fmt_count;
    let mut rows: Vec<(u32, SharedString, SharedString)> = range
        .chart
        .series
        .iter()
        .zip(&day.values)
        .filter(|(_, value)| **value > 0.0)
        .map(|(series, value)| {
            (
                palette::series(series.key),
                series_name(series.key, series.label.as_deref()),
                SharedString::from(format(*value)),
            )
        })
        .collect();
    rows.reverse();
    let mut notes = Vec::new();
    if !day.folded.is_empty() {
        let folded: Vec<String> = day
            .folded
            .iter()
            .map(|(model, value)| format!("{model} {}", fmt_compact(*value)))
            .collect();
        notes.push(messages.get(
            "settings-usage-tip-more-models",
            [("models", folded.join(" · ").into())],
        ));
    }
    let mut footer = messages
        .get(
            "settings-usage-tip-requests",
            [
                ("count", (day.requests as i64).into()),
                ("requests", fmt_count(day.requests as f64).into()),
            ],
        )
        .to_string();
    if metric == Metric::Credits && day.extra_credits > 0.0 {
        footer.push_str(&format!(
            " · {}",
            messages.get(
                "settings-usage-tip-extra",
                [("credits", fmt_count(day.extra_credits).into())],
            )
        ));
    }
    notes.push(footer.into());
    Tip {
        title: weekday_date_plain(&messages, day.date),
        headline: Some(tip_total(&messages, metric, day.total)),
        rows,
        notes,
    }
}

pub(super) fn month_tip(data: Option<&UsageData>, index: usize) -> Tip {
    let messages = crate::ui::i18n::MessageCache::default();
    let Some(month) = data
        .and_then(|data| data.months.as_ref())
        .and_then(|months| months.get(index))
    else {
        return Tip {
            title: SharedString::default(),
            headline: None,
            rows: Vec::new(),
            notes: Vec::new(),
        };
    };
    let title = messages.get(
        "settings-usage-month-year",
        [
            ("month", month_long(month.month).to_string().into()),
            ("year", month.year.to_string().into()),
        ],
    );
    let title = if month.partial {
        messages.get(
            "settings-usage-tip-so-far",
            [("label", title.to_string().into())],
        )
    } else {
        title
    };
    let mut rows: Vec<(u32, SharedString, SharedString)> = Origin::ALL
        .into_iter()
        .filter(|origin| month.origins[origin.index()] > 0.0)
        .map(|origin| {
            (
                palette::origin(origin),
                origin_name(origin),
                SharedString::from(fmt_count(month.origins[origin.index()])),
            )
        })
        .collect();
    rows.reverse();
    let mut footer = messages
        .get(
            "settings-usage-tip-requests",
            [
                ("count", (month.requests as i64).into()),
                ("requests", fmt_count(month.requests as f64).into()),
            ],
        )
        .to_string();
    if month.extra > 0.0 {
        footer.push_str(&format!(
            " · {}",
            messages.get(
                "settings-usage-tip-extra",
                [("credits", fmt_count(month.extra).into())],
            )
        ));
    }
    Tip {
        title,
        headline: Some(tip_total(&messages, Metric::Credits, month.credits())),
        rows,
        notes: vec![footer.into()],
    }
}

pub(super) fn weekday_date_plain(
    messages: &crate::ui::i18n::MessageCache,
    date: NaiveDate,
) -> SharedString {
    let short = messages.get(
        "settings-usage-short-date",
        [
            ("month", month_short(date.month()).to_string().into()),
            ("day", date.day().to_string().into()),
        ],
    );
    messages.get(
        "settings-usage-weekday-date",
        [
            ("weekday", weekday_short(date.weekday()).to_string().into()),
            ("date", short.to_string().into()),
        ],
    )
}

// ---- Labels ------------------------------------------------------------------

pub(super) fn metric_slug(metric: Metric) -> &'static str {
    match metric {
        Metric::Credits => "credits",
        Metric::Requests => "requests",
        Metric::Tokens => "tokens",
    }
}

pub(super) fn split_slug(split: Split) -> &'static str {
    match split {
        Split::Model => "model",
        Split::Origin => "app",
        Split::Pool => "pool",
        Split::Token => "token",
    }
}

pub(super) fn metric_name(metric: Metric) -> SharedString {
    tr(match metric {
        Metric::Credits => "settings-usage-metric-credits",
        Metric::Requests => "settings-usage-metric-requests",
        Metric::Tokens => "settings-usage-metric-tokens",
    })
}

/// The daily chart's heading: `Daily credits`.
pub(super) fn daily_title(metric: Metric) -> SharedString {
    tr(match metric {
        Metric::Credits => "settings-usage-daily-title-credits",
        Metric::Requests => "settings-usage-daily-title-requests",
        Metric::Tokens => "settings-usage-daily-title-tokens",
    })
}

/// A tooltip's total: `1,204 credits`, `36 requests`, `1.2M tokens`.
pub(super) fn tip_total(
    messages: &crate::ui::i18n::MessageCache,
    metric: Metric,
    value: f64,
) -> SharedString {
    let count = value.round() as i64;
    match metric {
        Metric::Credits => messages.get(
            "settings-usage-tip-credits",
            [("value", fmt_count(value).into()), ("count", count.into())],
        ),
        Metric::Requests => messages.get(
            "settings-usage-tip-requests",
            [
                ("requests", fmt_count(value).into()),
                ("count", count.into()),
            ],
        ),
        Metric::Tokens => messages.get(
            "settings-usage-tip-tokens",
            [
                ("value", fmt_compact(value).into()),
                ("count", count.into()),
            ],
        ),
    }
}

pub(super) fn split_name(split: Split) -> SharedString {
    tr(match split {
        Split::Model => "settings-usage-split-model",
        Split::Origin => "settings-usage-split-app",
        Split::Pool => "settings-usage-split-pool",
        Split::Token => "settings-usage-split-token",
    })
}

/// The split in running text, for the chart's label: "Daily credits, by …".
pub(super) fn split_word(split: Split) -> SharedString {
    tr(match split {
        Split::Model => "settings-usage-split-word-model",
        Split::Origin => "settings-usage-split-word-app",
        Split::Pool => "settings-usage-split-word-pool",
        Split::Token => "settings-usage-split-word-token",
    })
}

pub(super) fn origin_name(origin: Origin) -> SharedString {
    tr(match origin {
        Origin::Chat => "settings-usage-origin-chat",
        Origin::Work => "settings-usage-origin-work",
        Origin::Proxy => "settings-usage-origin-proxy",
        Origin::Api => "settings-usage-origin-api",
    })
}

pub(super) fn token_name(kind: TokenKind) -> SharedString {
    tr(match kind {
        TokenKind::Input => "settings-usage-token-input",
        TokenKind::Cached => "settings-usage-token-cached",
        TokenKind::Output => "settings-usage-token-output",
    })
}

pub(super) fn series_name(key: SeriesKey, label: Option<&str>) -> SharedString {
    if let Some(label) = label {
        return label.to_string().into();
    }
    match key {
        SeriesKey::Model(slot) => model::MODEL_SLOTS[slot].into(),
        SeriesKey::MoreModels => tr("settings-usage-more-models"),
        SeriesKey::Origin(origin) => origin_name(origin),
        SeriesKey::Plan => tr("settings-usage-pool-plan"),
        SeriesKey::Extra => tr("settings-usage-pool-extra"),
        SeriesKey::Token(kind) => token_name(kind),
    }
}

pub(super) fn month_short(month: u32) -> SharedString {
    tr(match month {
        1 => "settings-usage-month-jan",
        2 => "settings-usage-month-feb",
        3 => "settings-usage-month-mar",
        4 => "settings-usage-month-apr",
        5 => "settings-usage-month-may",
        6 => "settings-usage-month-jun",
        7 => "settings-usage-month-jul",
        8 => "settings-usage-month-aug",
        9 => "settings-usage-month-sep",
        10 => "settings-usage-month-oct",
        11 => "settings-usage-month-nov",
        _ => "settings-usage-month-dec",
    })
}

pub(super) fn month_long(month: u32) -> SharedString {
    tr(match month {
        1 => "settings-month-january",
        2 => "settings-month-february",
        3 => "settings-month-march",
        4 => "settings-month-april",
        5 => "settings-month-may",
        6 => "settings-month-june",
        7 => "settings-month-july",
        8 => "settings-month-august",
        9 => "settings-month-september",
        10 => "settings-month-october",
        11 => "settings-month-november",
        _ => "settings-month-december",
    })
}

pub(super) fn weekday_short(weekday: Weekday) -> SharedString {
    tr(match weekday {
        Weekday::Mon => "settings-usage-weekday-mon",
        Weekday::Tue => "settings-usage-weekday-tue",
        Weekday::Wed => "settings-usage-weekday-wed",
        Weekday::Thu => "settings-usage-weekday-thu",
        Weekday::Fri => "settings-usage-weekday-fri",
        Weekday::Sat => "settings-usage-weekday-sat",
        Weekday::Sun => "settings-usage-weekday-sun",
    })
}
