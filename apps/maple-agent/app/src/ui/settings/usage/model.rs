//! Usage history arithmetic, ported from the web app's
//! `components/settings/usage/usageData.ts` so Maple Work and the web show
//! the same numbers. Pure: no gpui, no I/O, no localization.

use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Utc};

use crate::billing::{BillingStatus, UsageDayRow, UsageModelLabels, UsageMonthRow};

/// Longest date list built from server-supplied bounds.
const MAX_DAYS: u64 = 400;

/// Compatibility highlights for servers whose catalogs predate groups. The
/// served catalog controls current grouping and order; GPT-OSS stays unpinned.
const HIGHLIGHT_GROUPS: [(&str, &[&str]); 3] = [
    ("capable", &["kimi-k3-1-pro", "kimi-k3", "glm-5-3"]),
    ("uncensored", &["deepseek-v4-1-flash-1017-uncensored"]),
    (
        "fast",
        &["gemma4-31b", "glm-5-3-flash", "deepseek-v4-1-flash"],
    ),
];

/// Named series before the rest fold into "More models"; one per chart color.
pub(crate) const MAX_NAMED_MODELS: usize = 7;

/// The picker's grouped models in chart order, with owned group headings.
pub(crate) struct ModelRanking {
    groups: Vec<maple_sdk::CatalogGroup>,
    models: Vec<(String, Option<usize>)>,
}

impl Default for ModelRanking {
    fn default() -> Self {
        Self {
            groups: HIGHLIGHT_GROUPS
                .iter()
                .map(|(id, _)| maple_sdk::CatalogGroup {
                    id: (*id).to_string(),
                    label: String::new(),
                })
                .collect(),
            models: HIGHLIGHT_GROUPS
                .iter()
                .enumerate()
                .flat_map(|(group, (_, models))| {
                    models
                        .iter()
                        .map(move |model| ((*model).to_string(), Some(group)))
                })
                .collect(),
        }
    }
}

impl ModelRanking {
    pub(crate) fn from_catalog(catalog: Option<&maple_sdk::ModelCatalogResponse>) -> Self {
        let Some(catalog) = catalog.filter(|catalog| !catalog.groups.is_empty()) else {
            return Self::default();
        };
        let mut models = Vec::new();
        for (group_index, group) in catalog.groups.iter().enumerate() {
            for model in catalog.data.iter().filter(|model| {
                model.available_in_agent != Some(false)
                    && model.group.as_deref() == Some(group.id.as_str())
            }) {
                if models
                    .iter()
                    .all(|(candidate, _): &(String, Option<usize>)| candidate != &model.id)
                {
                    models.push((model.id.clone(), Some(group_index)));
                }
            }
        }
        Self {
            groups: catalog.groups.clone(),
            models,
        }
    }

    /// Position in the catalog's quality order, independent of palette size.
    fn catalog_index(&self, model: &str) -> Option<usize> {
        self.models
            .iter()
            .position(|(candidate, _)| candidate == model)
    }

    /// Only palette-sized ranks are pinned. Later catalog entries borrow a
    /// free color when selected; they never wrap around by catalog position.
    pub(crate) fn model_slot(&self, model: &str) -> Option<usize> {
        self.catalog_index(model)
            .filter(|slot| *slot < MAX_NAMED_MODELS)
    }

    fn model_group(&self, model: &str) -> Option<&maple_sdk::CatalogGroup> {
        let (_, group) = self
            .models
            .iter()
            .find(|(candidate, _)| candidate == model)?;
        self.groups.get((*group)?)
    }

    fn highlight_models(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|(model, _)| model.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanKind {
    Free,
    Team,
    /// Pro, Max and other plans with a monthly credit allowance.
    Credits,
}

pub(crate) fn plan_kind(status: &BillingStatus) -> PlanKind {
    let name = status
        .product_name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if name == "free" {
        return PlanKind::Free;
    }
    if name.contains("team") {
        return PlanKind::Team;
    }
    let api_access = name.contains("pro") || name.contains("max");
    if status.total_tokens.unwrap_or(0) > 0 || api_access {
        PlanKind::Credits
    } else {
        PlanKind::Free
    }
}

/// The billing window the plan meter counts, from status.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlanWindow {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    /// The raw values, sent back to billing as the period query.
    pub(crate) start_raw: String,
    pub(crate) end_raw: String,
}

pub(crate) fn status_window(status: &BillingStatus) -> Option<PlanWindow> {
    if plan_kind(status) == PlanKind::Free {
        return None;
    }
    let start_raw = status.usage_period_start.as_deref()?;
    let end_raw = status.usage_reset_date.as_deref()?;
    let start = DateTime::parse_from_rfc3339(start_raw).ok()?.to_utc();
    let end = DateTime::parse_from_rfc3339(end_raw).ok()?.to_utc();
    (end > start).then(|| PlanWindow {
        start,
        end,
        start_raw: start_raw.to_string(),
        end_raw: end_raw.to_string(),
    })
}

pub(crate) fn local_date(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

/// Every date from `start` to `end`, inclusive; empty when `end < start`.
pub(crate) fn date_range(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    if end < start {
        return Vec::new();
    }
    let count = ((end - start).num_days() as u64 + 1).min(MAX_DAYS);
    start.iter_days().take(count as usize).collect()
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// `(year, month)` shifted by `delta` months.
pub(crate) fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let index = year * 12 + month as i32 - 1 + delta;
    (index.div_euclid(12), index.rem_euclid(12) as u32 + 1)
}

fn first_of(year: i32, month: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_default()
}

fn last_of(year: i32, month: u32) -> NaiveDate {
    let (next_year, next_month) = shift_month(year, month, 1);
    first_of(next_year, next_month)
        .pred_opt()
        .unwrap_or_default()
}

/// What the Usage page shows history for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageRange {
    Last30Days,
    Month { year: i32, month: u32 },
}

impl UsageRange {
    pub(crate) fn query(self) -> String {
        match self {
            Self::Last30Days => "last_30_days".into(),
            Self::Month { year, month } => format!("month:{year:04}-{month:02}"),
        }
    }

    /// The dates the range covers, up to today.
    pub(crate) fn dates(self, today: NaiveDate) -> Vec<NaiveDate> {
        match self {
            Self::Last30Days => date_range(today - Days::new(29), today),
            Self::Month { year, month } => {
                date_range(first_of(year, month), last_of(year, month).min(today))
            }
        }
    }

    pub(crate) fn is_current_month(self, today: NaiveDate) -> bool {
        matches!(self, Self::Month { year, month } if year == today.year() && month == today.month())
    }

    /// Last 30 days, this month, then the twelve months before it.
    pub(crate) fn options(today: NaiveDate) -> Vec<Self> {
        std::iter::once(Self::Last30Days)
            .chain((0..=12).map(|back| {
                let (year, month) = shift_month(today.year(), today.month(), -back);
                Self::Month { year, month }
            }))
            .collect()
    }
}

/// Where usage came from: the app that sent it, or API-key usage that named
/// no app. An API key can run inside any app, so `Api` means only that.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Origin {
    Chat,
    Work,
    Proxy,
    Api,
}

impl Origin {
    pub(crate) const ALL: [Self; 4] = [Self::Chat, Self::Work, Self::Proxy, Self::Api];

    /// Requests from before apps reported their name count as Chat unless
    /// they used an API key, as on the web.
    pub(crate) fn of(source: &str, client_app: Option<&str>) -> Self {
        match client_app {
            Some("chat") => Self::Chat,
            Some("work") => Self::Work,
            Some("proxy") => Self::Proxy,
            _ if source == "api" => Self::Api,
            _ => Self::Chat,
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TokenKind {
    Input,
    Cached,
    Output,
}

impl TokenKind {
    pub(crate) const ALL: [Self; 3] = [Self::Input, Self::Cached, Self::Output];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Metric {
    Credits,
    Requests,
    Tokens,
}

impl Metric {
    pub(crate) const ALL: [Self; 3] = [Self::Credits, Self::Requests, Self::Tokens];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Split {
    Model,
    Origin,
    Pool,
    Token,
}

impl Split {
    /// The splits that make sense for `metric` on `kind`'s plan.
    pub(crate) fn options(metric: Metric, kind: PlanKind) -> Vec<Self> {
        let mut options = vec![Self::Model];
        if kind != PlanKind::Free {
            options.push(Self::Origin);
        }
        if kind == PlanKind::Credits && metric == Metric::Credits {
            options.push(Self::Pool);
        }
        if metric == Metric::Tokens {
            options.push(Self::Token);
        }
        options
    }

    /// The split to keep when the metric changes: tokens default to the
    /// token split; a split the new metric lacks falls back to model.
    pub(crate) fn after_metric_change(self, next: Metric, kind: PlanKind) -> Self {
        if next == Metric::Tokens {
            return Self::Token;
        }
        if Self::options(next, kind).contains(&self) {
            self
        } else {
            Self::Model
        }
    }
}

/// Totals over any set of rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Counts {
    pub(crate) credits: f64,
    pub(crate) requests: u64,
    pub(crate) input: u64,
    pub(crate) cached: u64,
    pub(crate) output: u64,
    /// Requests that reported a cached count; 0 means "not reported".
    pub(crate) cached_reported: u64,
}

impl Counts {
    fn add(&mut self, counts: &crate::billing::UsageCounts) {
        self.credits += counts.credits;
        self.requests += counts.requests;
        self.input += counts.uncached_input_tokens;
        self.cached += counts.cached_input_tokens;
        self.output += counts.output_tokens;
        self.cached_reported += counts.cached_reported_requests;
    }

    pub(crate) fn tokens(&self) -> u64 {
        self.input + self.cached + self.output
    }

    pub(crate) fn metric(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Credits => self.credits,
            Metric::Requests => self.requests as f64,
            Metric::Tokens => self.tokens() as f64,
        }
    }
}

pub(crate) fn model_label(model: &str, labels: &UsageModelLabels) -> String {
    labels
        .get(model)
        .and_then(|label| label.display_name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(model)
        .to_string()
}

impl ModelRanking {
    fn series_before(&self, left: &str, right: &str) -> std::cmp::Ordering {
        match (self.catalog_index(left), self.catalog_index(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.cmp(right),
        }
    }

    /// Every model used in the range gets its own series until the chart runs
    /// out of colors. Catalog order chooses the best members; unranked models
    /// use stable ID order so changing the metric cannot change that selection.
    fn choose_named<'a>(&'a self, totals: &[(&'a str, f64)]) -> (Vec<&'a str>, Vec<&'a str>) {
        let used = |id: &str| {
            totals
                .iter()
                .any(|(candidate, total)| *candidate == id && *total > 0.0)
        };
        // Overflow still occupies one of the seven legend slots. Fold the
        // lowest catalog-ranked member rather than displace it with an
        // uncatalogued model, even if the member sits beyond palette capacity.
        let used_models = totals.iter().filter(|(_, total)| *total > 0.0).count();
        let limit = if used_models > MAX_NAMED_MODELS {
            MAX_NAMED_MODELS - 1
        } else {
            MAX_NAMED_MODELS
        };
        let mut named: Vec<&str> = self
            .highlight_models()
            .filter(|id| used(id))
            .take(limit)
            .collect();
        let mut others: Vec<(&str, f64)> = totals
            .iter()
            .filter(|(id, total)| *total > 0.0 && self.catalog_index(id).is_none())
            .map(|(id, total)| (*id, *total))
            .collect();
        others.sort_by(|left, right| left.0.cmp(right.0));
        let room = limit - named.len();
        named.extend(others.iter().take(room).map(|(id, _)| *id));
        named.sort_by(|left, right| self.series_before(left, right));
        let mut folded: Vec<&'a str> = self
            .highlight_models()
            .filter(|id| used(id))
            .skip(limit)
            .collect();
        folded.extend(others.into_iter().skip(room).map(|(id, _)| id));
        (named, folded)
    }

    /// Color slot for a model drawn with `named`. Highlights keep their fixed
    /// slots; each other model takes the next free slot.
    pub(crate) fn color_slot(&self, model: &str, named: &[&str]) -> Option<usize> {
        if !named.contains(&model) {
            return None;
        }
        if let Some(slot) = self.model_slot(model) {
            return Some(slot);
        }
        let rank = named
            .iter()
            .filter(|peer| {
                self.model_slot(peer).is_none() && self.series_before(peer, model).is_lt()
            })
            .count();
        (0..MAX_NAMED_MODELS)
            .filter(|slot| {
                named
                    .iter()
                    .all(|peer| self.model_slot(peer) != Some(*slot))
            })
            .nth(rank)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelSummary {
    pub(crate) model: String,
    pub(crate) label: String,
    pub(crate) counts: Counts,
    pub(crate) origins: [f64; 4],
    /// The top two origins of its credits as `(percent, origin)`, and how
    /// many more there are; see [`origin_mix`].
    pub(crate) mix: (Vec<(u32, Origin)>, usize),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RangeSummary {
    pub(crate) counts: Counts,
    /// Credits by origin.
    pub(crate) origins: [f64; 4],
    /// Credits through API keys in any app; overlaps `origins`.
    pub(crate) api: f64,
    pub(crate) plan: f64,
    pub(crate) extra: f64,
    /// Every model that ran, by credits (requests, then label, break ties).
    pub(crate) by_model: Vec<ModelSummary>,
}

pub(crate) fn summarize_days(rows: &[UsageDayRow], labels: &UsageModelLabels) -> RangeSummary {
    let mut summary = RangeSummary::default();
    let mut models: Vec<ModelSummary> = Vec::new();
    for row in rows {
        let credits = row.counts.credits;
        let origin = Origin::of(&row.source, row.client_app.as_deref());
        summary.counts.add(&row.counts);
        summary.origins[origin.index()] += credits;
        if row.source == "api" {
            summary.api += credits;
        }
        if row.pool == "extra" {
            summary.extra += credits;
        } else {
            summary.plan += credits;
        }
        let model = match models.iter_mut().find(|model| model.model == row.model) {
            Some(model) => model,
            None => {
                models.push(ModelSummary {
                    model: row.model.clone(),
                    label: model_label(&row.model, labels),
                    counts: Counts::default(),
                    origins: [0.0; 4],
                    mix: (Vec::new(), 0),
                });
                models.last_mut().expect("just pushed")
            }
        };
        model.counts.add(&row.counts);
        model.origins[origin.index()] += credits;
    }
    for model in &mut models {
        model.mix = origin_mix(&model.origins, model.counts.credits);
    }
    models.sort_by(|a, b| {
        b.counts
            .credits
            .total_cmp(&a.counts.credits)
            .then(b.counts.requests.cmp(&a.counts.requests))
            .then_with(|| a.label.cmp(&b.label))
    });
    summary.by_model = models;
    summary
}

/// One series in a stacked chart, in fixed identity order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SeriesKey {
    Model(usize),
    MoreModels,
    Origin(Origin),
    Plan,
    Extra,
    Token(TokenKind),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Series {
    pub(crate) key: SeriesKey,
    /// The model's name for model series, and for "More models" when one
    /// model alone folded into it; `None` takes the key's localized label.
    pub(crate) label: Option<String>,
    /// Canonical id for a named model, so matching tables get the same color.
    pub(crate) model: Option<String>,
    /// The model picker's display group, including its served English label.
    pub(crate) group: Option<maple_sdk::CatalogGroup>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChartDay {
    pub(crate) date: NaiveDate,
    /// One value per series, in series order.
    pub(crate) values: Vec<f64>,
    pub(crate) total: f64,
    pub(crate) requests: u64,
    pub(crate) extra_credits: f64,
    /// The models behind "More models" that day, largest first, when more
    /// than one model folded in the range.
    pub(crate) folded: Vec<(String, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DailyChart {
    pub(crate) series: Vec<Series>,
    pub(crate) days: Vec<ChartDay>,
    pub(crate) scale: NiceScale,
}

fn build_model_chart(
    dates: &[NaiveDate],
    rows: &[UsageDayRow],
    metric: Metric,
    labels: &UsageModelLabels,
    ranking: &ModelRanking,
) -> DailyChart {
    let mut model_ids: Vec<&str> = Vec::new();
    let mut per_model: Vec<Vec<f64>> = Vec::new();
    let mut requests = vec![0u64; dates.len()];
    let mut extra_credits = vec![0.0; dates.len()];
    for row in rows {
        let Some(day) =
            parse_date(&row.date).and_then(|date| dates.iter().position(|d| *d == date))
        else {
            continue;
        };
        requests[day] += row.counts.requests;
        if row.pool == "extra" {
            extra_credits[day] += row.counts.credits;
        }
        let mut counts = Counts::default();
        counts.add(&row.counts);
        let value = counts.metric(metric);
        let index = match model_ids.iter().position(|id| *id == row.model.as_str()) {
            Some(index) => index,
            None => {
                model_ids.push(row.model.as_str());
                per_model.push(vec![0.0; dates.len()]);
                model_ids.len() - 1
            }
        };
        per_model[index][day] += value;
    }

    let totals: Vec<(&str, f64)> = model_ids
        .iter()
        .zip(&per_model)
        .map(|(id, days)| (*id, days.iter().sum()))
        .collect();
    let (named, folded_ids) = ranking.choose_named(&totals);
    let mut series: Vec<Series> = named
        .iter()
        .map(|id| Series {
            key: SeriesKey::Model(
                ranking
                    .color_slot(id, &named)
                    .expect("a named model has a free color"),
            ),
            label: Some(model_label(id, labels)),
            model: Some((*id).to_string()),
            group: ranking.model_group(id).cloned(),
        })
        .collect();
    if !folded_ids.is_empty() {
        let label = (folded_ids.len() == 1).then(|| model_label(folded_ids[0], labels));
        series.push(Series {
            key: SeriesKey::MoreModels,
            label,
            model: None,
            group: None,
        });
    }

    let several_folded = folded_ids.len() > 1;
    let days: Vec<ChartDay> = dates
        .iter()
        .enumerate()
        .map(|(day, &date)| {
            let mut values: Vec<f64> = named
                .iter()
                .map(|id| {
                    model_ids
                        .iter()
                        .position(|candidate| *candidate == *id)
                        .map(|index| per_model[index][day])
                        .unwrap_or(0.0)
                })
                .collect();
            let mut folded = Vec::new();
            let mut folded_total = 0.0;
            for id in &folded_ids {
                let value = model_ids
                    .iter()
                    .position(|candidate| *candidate == *id)
                    .map(|index| per_model[index][day])
                    .unwrap_or(0.0);
                if value > 0.0 {
                    folded_total += value;
                    folded.push((model_label(id, labels), value));
                }
            }
            if !folded_ids.is_empty() {
                values.push(folded_total);
            }
            folded.sort_by(|left, right| right.1.total_cmp(&left.1));
            ChartDay {
                date,
                total: values.iter().sum(),
                values,
                requests: requests[day],
                extra_credits: extra_credits[day],
                folded: if several_folded { folded } else { Vec::new() },
            }
        })
        .collect();
    let peak = days.iter().map(|day| day.total).fold(0.0, f64::max);
    DailyChart {
        series,
        days,
        scale: nice_scale((peak.max(1.0)) * 1.04),
    }
}

fn split_keys(split: Split) -> Vec<SeriesKey> {
    match split {
        Split::Model => Vec::new(),
        Split::Origin => Origin::ALL.into_iter().map(SeriesKey::Origin).collect(),
        Split::Pool => vec![SeriesKey::Plan, SeriesKey::Extra],
        Split::Token => TokenKind::ALL.into_iter().map(SeriesKey::Token).collect(),
    }
}

pub(crate) fn build_daily_chart(
    dates: &[NaiveDate],
    rows: &[UsageDayRow],
    metric: Metric,
    split: Split,
    labels: &UsageModelLabels,
    ranking: &ModelRanking,
) -> DailyChart {
    if split == Split::Model {
        return build_model_chart(dates, rows, metric, labels, ranking);
    }
    let keys = split_keys(split);
    let mut values = vec![vec![0.0; keys.len()]; dates.len()];
    let mut requests = vec![0u64; dates.len()];
    let mut extra_credits = vec![0.0; dates.len()];

    for row in rows {
        let Some(day) =
            parse_date(&row.date).and_then(|date| dates.iter().position(|d| *d == date))
        else {
            continue;
        };
        requests[day] += row.counts.requests;
        if row.pool == "extra" {
            extra_credits[day] += row.counts.credits;
        }
        if split == Split::Token {
            for (index, kind) in TokenKind::ALL.into_iter().enumerate() {
                values[day][index] += match kind {
                    TokenKind::Input => row.counts.uncached_input_tokens,
                    TokenKind::Cached => row.counts.cached_input_tokens,
                    TokenKind::Output => row.counts.output_tokens,
                } as f64;
            }
            continue;
        }
        let mut counts = Counts::default();
        counts.add(&row.counts);
        let value = counts.metric(metric);
        let key = match split {
            Split::Model => unreachable!("handled above"),
            Split::Origin => SeriesKey::Origin(Origin::of(&row.source, row.client_app.as_deref())),
            Split::Pool if row.pool == "extra" => SeriesKey::Extra,
            Split::Pool => SeriesKey::Plan,
            Split::Token => unreachable!("handled above"),
        };
        let index = keys
            .iter()
            .position(|k| *k == key)
            .expect("key of this split");
        values[day][index] += value;
    }
    let used: Vec<usize> = (0..keys.len())
        .filter(|&index| values.iter().map(|day| day[index]).sum::<f64>() > 0.0)
        .collect();
    let series = used
        .iter()
        .map(|&index| Series {
            key: keys[index],
            label: None,
            model: None,
            group: None,
        })
        .collect();
    let days: Vec<ChartDay> = dates
        .iter()
        .enumerate()
        .map(|(day, &date)| {
            let day_values: Vec<f64> = used.iter().map(|&index| values[day][index]).collect();
            ChartDay {
                date,
                total: day_values.iter().sum(),
                values: day_values,
                requests: requests[day],
                extra_credits: extra_credits[day],
                folded: Vec::new(),
            }
        })
        .collect();
    let peak = days.iter().map(|day| day.total).fold(0.0, f64::max);
    DailyChart {
        series,
        days,
        scale: nice_scale((peak.max(1.0)) * 1.04),
    }
}

/// First and last day indices that drew on Extra credits.
pub(crate) fn extra_credit_span(days: &[ChartDay]) -> Option<(usize, usize)> {
    let first = days.iter().position(|day| day.extra_credits > 0.0)?;
    let last = days.iter().rposition(|day| day.extra_credits > 0.0)?;
    Some((first, last))
}

/// A "nice" axis, so it reads 0, 5K, 10K… rather than 6.3K. Tries four and
/// five intervals and keeps whichever wastes less headroom.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NiceScale {
    pub(crate) max: f64,
    pub(crate) ticks: Vec<f64>,
}

pub(crate) fn nice_scale(value: f64) -> NiceScale {
    let target = if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    };
    let option = |intervals: f64| {
        let raw = target / intervals;
        let exp = 10f64.powf(raw.log10().floor());
        let n = raw / exp;
        // Quarter steps read cleanly as 25/250 multiples; above that, five
        // intervals of 2.5 would label 1.25K as "1.3K".
        let allow_quarter = exp >= 10.0 && (intervals == 4.0 || target <= 1000.0);
        let unit = if n <= 1.0 {
            1.0
        } else if n <= 2.0 {
            2.0
        } else if allow_quarter && n <= 2.5 {
            2.5
        } else if n <= 5.0 {
            5.0
        } else {
            10.0
        };
        // Usage counts are whole numbers, so steps never go below 1.
        let step = (unit * exp).max(1.0);
        (step, (target / step).ceil() * step)
    };
    let four = option(4.0);
    let five = option(5.0);
    let (step, max) = if five.1 < four.1 { five } else { four };
    let mut ticks = Vec::new();
    let mut index = 0.0;
    while index * step <= max + step / 2.0 {
        ticks.push(index * step);
        index += 1.0;
    }
    NiceScale { max, ticks }
}

/// Where the billing period's plan credits are heading at the current pace.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PeriodProgress {
    /// Plan credits used, from status.
    pub(crate) used: f64,
    pub(crate) allowance: f64,
    /// Plan credits per day so far.
    pub(crate) pace: f64,
    /// Plan credits by the end at the current pace.
    pub(crate) projected: f64,
    pub(crate) exhausted: bool,
    pub(crate) run_out: Option<NaiveDate>,
}

pub(crate) fn build_period_progress(
    window: &PlanWindow,
    used: f64,
    allowance: f64,
    now: DateTime<Utc>,
) -> Option<PeriodProgress> {
    let dates = date_range(
        local_date(window.start),
        local_date(window.end - chrono::Duration::milliseconds(1)),
    );
    let total_days = dates.len();
    let first = *dates.first()?;
    let today_index = ((local_date(now) - first).num_days().max(0) as usize).min(total_days - 1);
    let elapsed = (today_index + 1) as f64;
    let remaining = (total_days - today_index - 1) as f64;
    let pace = used / elapsed;
    let exhausted = used >= allowance;
    let run_out = (!exhausted && pace > 0.0)
        .then(|| today_index + ((allowance - used) / pace).ceil() as usize)
        .and_then(|index| dates.get(index).copied());
    Some(PeriodProgress {
        used,
        allowance,
        pace,
        projected: used + pace * remaining,
        exhausted,
        run_out,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MonthBucket {
    pub(crate) year: i32,
    pub(crate) month: u32,
    /// Credits by origin.
    pub(crate) origins: [f64; 4],
    pub(crate) requests: u64,
    pub(crate) extra: f64,
    pub(crate) partial: bool,
}

impl MonthBucket {
    pub(crate) fn credits(&self) -> f64 {
        self.origins.iter().sum()
    }
}

/// `count` months ending with `current`, oldest first.
pub(crate) fn build_months(
    rows: &[UsageMonthRow],
    current: (i32, u32),
    count: i32,
) -> Vec<MonthBucket> {
    let mut buckets: Vec<MonthBucket> = (0..count)
        .rev()
        .map(|back| {
            let (year, month) = shift_month(current.0, current.1, -back);
            MonthBucket {
                year,
                month,
                origins: [0.0; 4],
                requests: 0,
                extra: 0.0,
                partial: back == 0,
            }
        })
        .collect();
    for row in rows {
        let Some(bucket) = buckets
            .iter_mut()
            .find(|bucket| row.month == format!("{:04}-{:02}", bucket.year, bucket.month))
        else {
            continue;
        };
        let origin = Origin::of(&row.source, row.client_app.as_deref());
        bucket.origins[origin.index()] += row.counts.credits;
        bucket.requests += row.counts.requests;
        if row.pool == "extra" {
            bucket.extra += row.counts.credits;
        }
    }
    buckets
}

/// How many of `dates` had any usage: a request or a credit.
pub(crate) fn active_days(dates: &[NaiveDate], rows: &[UsageDayRow]) -> usize {
    let mut active = vec![false; dates.len()];
    for row in rows {
        if row.counts.requests == 0 && row.counts.credits <= 0.0 {
            continue;
        }
        if let Some(day) =
            parse_date(&row.date).and_then(|date| dates.iter().position(|d| *d == date))
        {
            active[day] = true;
        }
    }
    active.into_iter().filter(|active| *active).count()
}

/// The dates a range's chart spans: the server's resolved range, ending at
/// today or the latest row, whichever is later.
pub(crate) fn response_dates(
    range: UsageRange,
    start: &str,
    end: &str,
    rows: &[UsageDayRow],
    today: NaiveDate,
) -> Vec<NaiveDate> {
    let (Some(start), Some(end)) = (parse_date(start), parse_date(end)) else {
        return range.dates(today);
    };
    let latest = rows
        .iter()
        .filter_map(|row| parse_date(&row.date))
        .fold(today, NaiveDate::max);
    date_range(start, end.min(latest))
}

/// `80% Work · 10% Proxy · 2 more`: the top two origins of a model's
/// credits, as `(percent, origin)`, and how many more there are.
pub(crate) fn origin_mix(origins: &[f64; 4], total: f64) -> (Vec<(u32, Origin)>, usize) {
    let mut used: Vec<(f64, Origin)> = Origin::ALL
        .into_iter()
        .map(|origin| (origins[origin.index()], origin))
        .filter(|(credits, _)| *credits > 0.0)
        .collect();
    used.sort_by(|a, b| b.0.total_cmp(&a.0));
    let more = used.len().saturating_sub(2);
    let top = used
        .into_iter()
        .take(2)
        .map(|(credits, origin)| (percent(credits, total), origin))
        .collect();
    (top, more)
}

pub(crate) fn percent(part: f64, whole: f64) -> u32 {
    if whole <= 0.0 {
        0
    } else {
        (part / whole * 100.0).round().max(0.0) as u32
    }
}

/// `10,500`: whole numbers with thousands separators.
pub(crate) fn fmt_count(value: f64) -> String {
    crate::billing::format_count(value.round() as i64)
}

/// `1.2M`, `950`, `20K`: at most one decimal, trailing `.0` dropped.
pub(crate) fn fmt_compact(value: f64) -> String {
    const UNITS: [(f64, &str); 4] = [(1e3, "K"), (1e6, "M"), (1e9, "B"), (1e12, "T")];
    let one_decimal = |value: f64| {
        let rounded = (value * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            format!("{}", rounded as i64)
        } else {
            format!("{rounded:.1}")
        }
    };
    let magnitude = value.abs();
    if (magnitude * 10.0).round() / 10.0 < 1000.0 {
        return one_decimal(value);
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // A value that rounds up to 1000 of one unit reads as 1 of the next.
    let (scaled, suffix) = UNITS
        .iter()
        .map(|&(unit, suffix)| ((magnitude / unit * 10.0).round() / 10.0, suffix))
        .find(|&(scaled, suffix)| scaled < 1000.0 || suffix == "T")
        .expect("T accepts any magnitude");
    format!("{sign}{}{suffix}", one_decimal(scaled))
}

/// Prices: thousands separators and at most two decimals.
pub(crate) fn fmt_price(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    let whole = crate::billing::format_count(rounded.trunc() as i64);
    let cents = ((rounded.fract().abs()) * 100.0).round() as i64;
    match cents {
        0 => whole,
        c if c % 10 == 0 => format!("{whole}.{}", c / 10),
        c => format!("{whole}.{c:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::UsageCounts;

    fn row(
        date: &str,
        model: &str,
        source: &str,
        app: Option<&str>,
        pool: &str,
        credits: f64,
    ) -> UsageDayRow {
        UsageDayRow {
            date: date.into(),
            model: model.into(),
            source: source.into(),
            client_app: app.map(str::to_string),
            pool: pool.into(),
            counts: UsageCounts {
                requests: 1,
                uncached_input_tokens: 100,
                cached_input_tokens: 0,
                cached_reported_requests: 0,
                output_tokens: 10,
                credits,
            },
        }
    }

    fn day(value: &str) -> NaiveDate {
        parse_date(value).unwrap()
    }

    fn current_catalog() -> maple_sdk::ModelCatalogResponse {
        serde_json::from_value(serde_json::json!({
            "object": "list",
            "groups": [
                {"id": "capable", "label": "Most capable"},
                {"id": "uncensored", "label": "Uncensored"},
                {"id": "fast", "label": "Fast"},
            ],
            "data": [
                {"id": "kimi-k3-1-pro", "group": "capable"},
                {"id": "kimi-k3", "group": "capable"},
                {"id": "glm-5-3", "group": "capable"},
                {"id": "deepseek-v4-1-flash-1017-uncensored", "group": "uncensored"},
                {"id": "gemma4-31b", "group": "fast"},
                {"id": "glm-5-3-flash", "group": "fast"},
                {"id": "deepseek-v4-1-flash", "group": "fast"},
                {"id": "gpt-oss-120b"},
                {"id": "llama3-3-70b"},
            ]
        }))
        .unwrap()
    }

    #[test]
    fn nice_scale_matches_the_web_axis() {
        let cases: [(f64, f64, &[f64]); 4] = [
            (1456.0, 1500.0, &[0.0, 500.0, 1000.0, 1500.0]),
            (950.0, 1000.0, &[0.0, 250.0, 500.0, 750.0, 1000.0]),
            (1.04, 2.0, &[0.0, 1.0, 2.0]),
            (104.0, 125.0, &[0.0, 25.0, 50.0, 75.0, 100.0, 125.0]),
        ];
        for (value, max, ticks) in cases {
            let scale = nice_scale(value);
            assert_eq!((scale.max, scale.ticks.as_slice()), (max, ticks), "{value}");
        }
        assert_eq!(nice_scale(21_000.0).ticks.last(), Some(&25_000.0));
        assert_eq!(nice_scale(0.0).max, 1.0);
    }

    #[test]
    fn origins_attribute_tagged_apps_and_count_untagged_sessions_as_chat() {
        assert_eq!(Origin::of("api", Some("work")), Origin::Work);
        assert_eq!(Origin::of("app", None), Origin::Chat);
        assert_eq!(Origin::of("api", None), Origin::Api);
        assert_eq!(Origin::of("app", Some("ios")), Origin::Chat);
        let summary = summarize_days(
            &[
                row("2026-10-01", "glm-5-3", "api", Some("work"), "plan", 10.0),
                row("2026-10-01", "kimi-k3", "api", None, "extra", 5.0),
                row("2026-10-02", "glm-5-3", "app", None, "plan", 2.0),
            ],
            &UsageModelLabels::default(),
        );
        assert_eq!(summary.origins, [2.0, 10.0, 0.0, 5.0]);
        // API-key credits overlap the per-app totals.
        assert_eq!(summary.api, 15.0);
        assert_eq!((summary.plan, summary.extra), (12.0, 5.0));
        assert_eq!(summary.by_model[0].model, "glm-5-3");
        assert_eq!(summary.by_model[0].counts.credits, 12.0);
    }

    #[test]
    fn daily_chart_names_every_used_model_until_the_legend_is_full() {
        let dates = date_range(day("2026-10-01"), day("2026-10-03"));
        let rows = [
            row("2026-10-01", "glm-5-3", "app", None, "plan", 4.0),
            row("2026-10-02", "llama-x", "app", None, "plan", 3.0),
            row("2026-09-30", "glm-5-3", "app", None, "plan", 99.0),
        ];
        let mut labels = UsageModelLabels::default();
        labels.insert(
            "llama-x".into(),
            maple_billing::UsageModelLabel {
                display_name: Some("Llama X".into()),
            },
        );
        let catalog = current_catalog();
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &labels,
            &ranking,
        );
        assert_eq!(
            chart
                .series
                .iter()
                .map(|s| (s.key, s.label.clone()))
                .collect::<Vec<_>>(),
            [
                (SeriesKey::Model(2), Some("glm-5-3".into())),
                (SeriesKey::Model(0), Some("Llama X".into())),
            ]
        );
        assert_eq!(chart.days[0].values, [4.0, 0.0]);
        assert_eq!(chart.days[1].values, [0.0, 3.0]);
        assert!(chart.days[1].folded.is_empty());
        assert_eq!(chart.days[2].total, 0.0);

        // Two models outside the highlight list are still named. "More models"
        // is an overflow bucket, not a home for everything we did not pin.
        let rows = [
            row("2026-10-02", "llama-x", "app", None, "plan", 3.0),
            row("2026-10-02", "qwen-y", "app", None, "extra", 5.0),
        ];
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &labels,
            &ranking,
        );
        assert_eq!(
            chart
                .series
                .iter()
                .map(|series| series.label.clone())
                .collect::<Vec<_>>(),
            [Some("Llama X".into()), Some("qwen-y".into())]
        );
        assert!(
            chart
                .series
                .iter()
                .all(|series| series.key != SeriesKey::MoreModels)
        );
        assert_eq!(chart.days[1].values, [3.0, 5.0]);
        assert!(chart.days[1].folded.is_empty());
        assert_eq!(chart.days[1].extra_credits, 5.0);
        assert_eq!(extra_credit_span(&chart.days), Some((1, 1)));
    }

    #[test]
    fn group_less_catalog_keeps_the_legacy_purple_highlight() {
        let mut older = current_catalog();
        older.groups.clear();
        for ranking in [
            ModelRanking::default(),
            ModelRanking::from_catalog(None),
            ModelRanking::from_catalog(Some(&older)),
        ] {
            assert_eq!(ranking.model_slot("kimi-k3"), Some(1));
            assert_eq!(
                ranking.model_slot("deepseek-v4-1-flash-1017-uncensored"),
                Some(3)
            );
            assert_eq!(ranking.model_slot("gpt-oss-120b"), None);
            assert_eq!(
                ranking
                    .model_group("deepseek-v4-1-flash-1017-uncensored")
                    .map(|group| group.id.as_str()),
                Some("uncensored")
            );
        }
    }

    #[test]
    fn unranked_overflow_is_independent_of_the_metric() {
        let dates = date_range(day("2026-10-01"), day("2026-10-01"));
        let rows = [
            "model-a", "model-b", "model-c", "model-d", "model-e", "model-f", "model-g", "model-h",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, model)| {
            let mut row = row("2026-10-01", model, "app", None, "plan", (index + 1) as f64);
            row.counts.requests = 8 - index as u64;
            row
        })
        .collect::<Vec<_>>();
        for (metric, folded) in [
            (Metric::Credits, ["model-h", "model-g"]),
            (Metric::Requests, ["model-g", "model-h"]),
        ] {
            let chart = build_daily_chart(
                &dates,
                &rows,
                metric,
                Split::Model,
                &UsageModelLabels::default(),
                &ModelRanking::default(),
            );
            assert_eq!(
                chart
                    .series
                    .iter()
                    .map(|series| series.key)
                    .collect::<Vec<_>>(),
                [
                    SeriesKey::Model(0),
                    SeriesKey::Model(1),
                    SeriesKey::Model(2),
                    SeriesKey::Model(3),
                    SeriesKey::Model(4),
                    SeriesKey::Model(5),
                    SeriesKey::MoreModels,
                ],
                "{metric:?}"
            );
            assert_eq!(
                chart
                    .series
                    .iter()
                    .take(6)
                    .map(|series| series.label.as_deref())
                    .collect::<Vec<_>>(),
                [
                    Some("model-a"),
                    Some("model-b"),
                    Some("model-c"),
                    Some("model-d"),
                    Some("model-e"),
                    Some("model-f"),
                ],
                "{metric:?}"
            );
            assert_eq!(
                chart.days[0]
                    .folded
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>(),
                folded,
                "{metric:?}"
            );
        }
    }

    #[test]
    fn current_catalog_keeps_uncensored_purple_and_oss_unpinned() {
        let catalog = current_catalog();
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        assert_eq!(ranking.color_slot("gemma4-31b", &[]), None);
        assert_eq!(ranking.color_slot("gpt-oss-120b", &[]), None);
        assert_eq!(
            ranking.model_slot("deepseek-v4-1-flash-1017-uncensored"),
            Some(3)
        );
        assert_eq!(ranking.model_slot("gpt-oss-120b"), None);
        assert_eq!(ranking.model_slot("glm-5-3"), Some(2));
        assert!(
            ranking
                .highlight_models()
                .eq(catalog.groups.iter().flat_map(|group| {
                    catalog
                        .data
                        .iter()
                        .filter(move |model| {
                            model.available_in_agent != Some(false)
                                && model.group.as_deref() == Some(group.id.as_str())
                        })
                        .map(|model| model.id.as_str())
                }))
        );
        assert_eq!(
            ranking
                .model_group("deepseek-v4-1-flash-1017-uncensored")
                .map(|group| group.id.as_str()),
            Some("uncensored")
        );

        let dates = date_range(day("2026-10-01"), day("2026-10-01"));
        let rows = [
            row(
                "2026-10-01",
                "deepseek-v4-1-flash-1017-uncensored",
                "app",
                None,
                "plan",
                40.0,
            ),
            row("2026-10-01", "gpt-oss-120b", "app", None, "plan", 2.0),
        ];
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert_eq!(
            chart
                .series
                .iter()
                .map(|series| series.key)
                .collect::<Vec<_>>(),
            [SeriesKey::Model(3), SeriesKey::Model(0)]
        );
        assert!(
            chart
                .series
                .iter()
                .all(|series| series.key != SeriesKey::MoreModels)
        );
    }

    #[test]
    fn served_model_replacement_does_not_resurrect_legacy_k3() {
        let mut catalog = current_catalog();
        catalog.data[1].id = "kimi-k4".into();
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        assert!(ranking.highlight_models().eq([
            "kimi-k3-1-pro",
            "kimi-k4",
            "glm-5-3",
            "deepseek-v4-1-flash-1017-uncensored",
            "gemma4-31b",
            "glm-5-3-flash",
            "deepseek-v4-1-flash",
        ]));
        assert_eq!(ranking.model_slot("kimi-k3"), None);
        assert_eq!(ranking.model_slot("kimi-k4"), Some(1));
        assert_eq!(
            ranking.model_slot("deepseek-v4-1-flash-1017-uncensored"),
            Some(3)
        );
    }

    #[test]
    fn served_order_controls_groups_and_slots_instead_of_usage_totals() {
        let catalog: maple_sdk::ModelCatalogResponse = serde_json::from_value(serde_json::json!({
            "object": "list",
            "groups": [
                {"id": "future", "label": "New arrivals"},
                {"id": "capable", "label": "Most capable"},
            ],
            "data": [
                {"id": "z-first-in-data", "group": "future"},
                {"id": "a-second-in-data", "group": "future"},
                {"id": "glm-5-3", "group": "capable"},
                {"id": "gpt-oss-120b"},
            ]
        }))
        .unwrap();
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        let dates = date_range(day("2026-10-01"), day("2026-10-01"));
        let rows = [
            row("2026-10-01", "a-second-in-data", "app", None, "plan", 1.0),
            row("2026-10-01", "z-first-in-data", "app", None, "plan", 100.0),
            row("2026-10-01", "glm-5-3", "app", None, "plan", 50.0),
            row("2026-10-01", "gpt-oss-120b", "app", None, "plan", 500.0),
        ];
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert_eq!(
            chart
                .series
                .iter()
                .map(|series| (series.label.as_deref(), series.key))
                .collect::<Vec<_>>(),
            [
                (Some("z-first-in-data"), SeriesKey::Model(0)),
                (Some("a-second-in-data"), SeriesKey::Model(1)),
                (Some("glm-5-3"), SeriesKey::Model(2)),
                (Some("gpt-oss-120b"), SeriesKey::Model(3)),
            ]
        );
        assert_eq!(
            chart
                .series
                .iter()
                .map(|series| {
                    series
                        .group
                        .as_ref()
                        .map(|group| (group.id.as_str(), group.label.as_str()))
                })
                .collect::<Vec<_>>(),
            [
                Some(("future", "New arrivals")),
                Some(("future", "New arrivals")),
                Some(("capable", "Most capable")),
                None,
            ]
        );
        let requests = build_daily_chart(
            &dates,
            &rows,
            Metric::Requests,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert_eq!(requests.series, chart.series);
    }

    #[test]
    fn current_catalog_makes_omitted_and_unavailable_legacy_models_unranked() {
        let mut omitted = current_catalog();
        omitted
            .data
            .retain(|model| model.id != "deepseek-v4-1-flash-1017-uncensored");
        let ranking = ModelRanking::from_catalog(Some(&omitted));
        assert_eq!(
            ranking.model_slot("deepseek-v4-1-flash-1017-uncensored"),
            None
        );

        let mut unavailable = current_catalog();
        unavailable
            .data
            .iter_mut()
            .find(|model| model.id == "deepseek-v4-1-flash-1017-uncensored")
            .unwrap()
            .available_in_agent = Some(false);
        let ranking = ModelRanking::from_catalog(Some(&unavailable));
        assert_eq!(
            ranking.model_slot("deepseek-v4-1-flash-1017-uncensored"),
            None
        );
    }

    #[test]
    fn more_models_appears_only_after_the_legend_is_full() {
        let dates = date_range(day("2026-10-01"), day("2026-10-01"));
        let mut rows = Vec::new();
        for index in 0..MAX_NAMED_MODELS {
            rows.push(row(
                "2026-10-01",
                &format!("m{index}"),
                "app",
                None,
                "plan",
                (MAX_NAMED_MODELS - index) as f64,
            ));
        }
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ModelRanking::default(),
        );
        assert!(
            chart
                .series
                .iter()
                .all(|series| series.key != SeriesKey::MoreModels)
        );
        assert_eq!(chart.series.len(), MAX_NAMED_MODELS);

        rows.push(row("2026-10-01", "overflow", "app", None, "plan", 0.5));
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ModelRanking::default(),
        );
        assert_eq!(chart.series.len(), MAX_NAMED_MODELS);
        assert_eq!(chart.series.last().unwrap().key, SeriesKey::MoreModels);
        assert_eq!(chart.series.last().unwrap().label, None);
        assert_eq!(
            chart.days[0].folded,
            [("m6".to_string(), 1.0), ("overflow".to_string(), 0.5)]
        );

        rows.push(row("2026-10-01", "also", "app", None, "plan", 0.25));
        let chart = build_daily_chart(
            &dates,
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ModelRanking::default(),
        );
        assert_eq!(chart.series.len(), MAX_NAMED_MODELS);
        assert_eq!(chart.series.last().unwrap().label, None);
        assert_eq!(
            chart.days[0].folded,
            [
                ("m5".to_string(), 2.0),
                ("m6".to_string(), 1.0),
                ("overflow".to_string(), 0.5)
            ]
        );
    }

    #[test]
    fn catalog_absent_historical_usage_survives_overflow() {
        let mut catalog = current_catalog();
        let mut rows: Vec<UsageDayRow> = catalog
            .groups
            .iter()
            .flat_map(|group| {
                catalog
                    .data
                    .iter()
                    .filter(move |model| model.group.as_deref() == Some(group.id.as_str()))
                    .map(|model| row("2026-10-01", &model.id, "app", None, "plan", 1.0))
            })
            .collect();
        catalog
            .data
            .retain(|model| model.id != "deepseek-v4-1-flash");
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        assert_eq!(ranking.model_slot("deepseek-v4-1-flash"), None);
        rows.push(row(
            "2026-10-01",
            "gpt-oss-120b",
            "app",
            None,
            "plan",
            500.0,
        ));
        rows.push(row(
            "2026-10-01",
            "llama3-3-70b",
            "app",
            None,
            "plan",
            400.0,
        ));
        let expected: f64 = rows.iter().map(|row| row.counts.credits).sum();
        let chart = build_daily_chart(
            &date_range(day("2026-10-01"), day("2026-10-01")),
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert_eq!(
            chart.series.last().map(|series| series.key),
            Some(SeriesKey::MoreModels)
        );
        assert!(
            chart.days[0]
                .folded
                .contains(&("deepseek-v4-1-flash".to_string(), 1.0))
        );
        assert_eq!(chart.days[0].total, expected);
        assert_eq!(chart.days[0].values.iter().sum::<f64>(), expected);
    }

    #[test]
    fn served_catalog_growth_never_wraps_color_slots() {
        let catalog: maple_sdk::ModelCatalogResponse = serde_json::from_value(serde_json::json!({
            "object": "list",
            "groups": [{"id": "growing", "label": "Growing"}],
            "data": [
                {"id": "model-0", "group": "growing"},
                {"id": "model-1", "group": "growing"},
                {"id": "model-2", "group": "growing"},
                {"id": "model-3", "group": "growing"},
                {"id": "model-4", "group": "growing"},
                {"id": "model-5", "group": "growing"},
                {"id": "model-6", "group": "growing"},
                {"id": "model-7", "group": "growing"},
            ]
        }))
        .unwrap();
        let ranking = ModelRanking::from_catalog(Some(&catalog));
        assert_eq!(ranking.model_slot("model-6"), Some(6));
        assert_eq!(ranking.model_slot("model-7"), None);
        assert_eq!(ranking.color_slot("model-7", &["model-7"]), Some(0));

        let rows: Vec<UsageDayRow> = (0..8)
            .map(|index| {
                row(
                    "2026-10-01",
                    &format!("model-{index}"),
                    "app",
                    None,
                    "plan",
                    (index + 1) as f64,
                )
            })
            .collect();
        let expected: f64 = rows.iter().map(|row| row.counts.credits).sum();
        let chart = build_daily_chart(
            &date_range(day("2026-10-01"), day("2026-10-01")),
            &rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert!(chart.series.iter().all(|series| match series.key {
            SeriesKey::Model(slot) => slot < MAX_NAMED_MODELS,
            _ => true,
        }));
        assert_eq!(
            chart.series.last().map(|series| series.key),
            Some(SeriesKey::MoreModels)
        );
        assert_eq!(
            chart.days[0].folded,
            [("model-7".to_string(), 8.0), ("model-6".to_string(), 7.0)]
        );
        assert_eq!(chart.days[0].total, expected);

        let mut priority_rows: Vec<UsageDayRow> = (0..5)
            .map(|index| {
                row(
                    "2026-10-01",
                    &format!("model-{index}"),
                    "app",
                    None,
                    "plan",
                    (index + 1) as f64,
                )
            })
            .collect();
        priority_rows.push(row("2026-10-01", "model-7", "app", None, "plan", 1.0));
        priority_rows.push(row(
            "2026-10-01",
            "unlisted-heavy",
            "app",
            None,
            "plan",
            1_000.0,
        ));
        priority_rows.push(row(
            "2026-10-01",
            "gpt-oss-120b",
            "app",
            None,
            "plan",
            500.0,
        ));
        let priority = build_daily_chart(
            &date_range(day("2026-10-01"), day("2026-10-01")),
            &priority_rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        // A late catalog member is still better than an uncatalogued model,
        // regardless of usage or its position beyond the seven fixed slots.
        assert_eq!(
            priority
                .series
                .iter()
                .filter_map(|series| series.model.as_deref())
                .collect::<Vec<_>>(),
            [
                "model-0", "model-1", "model-2", "model-3", "model-4", "model-7"
            ]
        );
        assert_eq!(priority.series[5].key, SeriesKey::Model(5));
        assert_eq!(
            priority.series.last().map(|series| series.key),
            Some(SeriesKey::MoreModels)
        );
        assert_eq!(
            priority.days[0].folded,
            [
                ("unlisted-heavy".to_string(), 1_000.0),
                ("gpt-oss-120b".to_string(), 500.0)
            ]
        );

        priority_rows.retain(|row| row.model != "unlisted-heavy");
        let mixed = build_daily_chart(
            &date_range(day("2026-10-01"), day("2026-10-01")),
            &priority_rows,
            Metric::Credits,
            Split::Model,
            &UsageModelLabels::default(),
            &ranking,
        );
        assert_eq!(
            (mixed.series[5].model.as_deref(), mixed.series[5].key),
            (Some("model-7"), SeriesKey::Model(5))
        );
        assert_eq!(
            (mixed.series[6].model.as_deref(), mixed.series[6].key),
            (Some("gpt-oss-120b"), SeriesKey::Model(6))
        );
    }

    #[test]
    fn metric_changes_keep_only_splits_that_still_apply() {
        let kind = PlanKind::Credits;
        assert_eq!(
            Split::Pool.after_metric_change(Metric::Requests, kind),
            Split::Model
        );
        assert_eq!(
            Split::Origin.after_metric_change(Metric::Requests, kind),
            Split::Origin
        );
        assert_eq!(
            Split::Model.after_metric_change(Metric::Tokens, kind),
            Split::Token
        );
        assert_eq!(
            Split::Token.after_metric_change(Metric::Credits, kind),
            Split::Model
        );
        assert!(!Split::options(Metric::Credits, PlanKind::Team).contains(&Split::Pool));
    }

    #[test]
    fn period_progress_projects_the_plan_pace() {
        let start = DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .unwrap()
            .to_utc();
        let window = PlanWindow {
            start,
            end: start + chrono::Duration::days(29),
            start_raw: String::new(),
            end_raw: String::new(),
        };
        let first = local_date(start);
        let now = start + chrono::Duration::days(3);
        let progress = build_period_progress(&window, 400.0, 1000.0, now).unwrap();
        assert_eq!(progress.pace, 100.0);
        assert_eq!(progress.projected, 400.0 + 100.0 * 26.0);
        // 600 left at 100 a day runs out six days after today.
        assert_eq!(progress.run_out, Some(first + Days::new(9)));
        assert!(!progress.exhausted);

        let spent = build_period_progress(&window, 1000.0, 1000.0, now).unwrap();
        assert!(spent.exhausted);
        assert_eq!(spent.run_out, None);
    }

    #[test]
    fn months_bucket_the_last_year_by_app() {
        let month = |month: &str, app: Option<&str>, credits: f64| UsageMonthRow {
            month: month.into(),
            source: "app".into(),
            client_app: app.map(str::to_string),
            pool: "plan".into(),
            counts: UsageCounts {
                credits,
                requests: 2,
                ..UsageCounts::default()
            },
        };
        let months = build_months(
            &[
                month("2026-10", Some("work"), 5.0),
                month("2025-10", None, 7.0),
                month("2025-11", None, 3.0),
            ],
            (2026, 10),
            12,
        );
        assert_eq!(months.len(), 12);
        assert_eq!((months[0].year, months[0].month), (2025, 11));
        assert_eq!(months[0].origins[Origin::Chat.index()], 3.0);
        assert!(months[11].partial && !months[10].partial);
        assert_eq!(months[11].origins[Origin::Work.index()], 5.0);
    }

    #[test]
    fn active_days_count_each_day_once_and_only_in_range() {
        let dates = date_range(day("2026-10-01"), day("2026-10-07"));
        let rows = [
            row("2026-10-01", "glm-5-3", "app", None, "plan", 2.0),
            row("2026-10-01", "kimi-k3", "api", None, "extra", 1.0),
            row("2026-10-03", "glm-5-3", "app", None, "plan", 8.0),
            row("2026-10-09", "glm-5-3", "app", None, "plan", 1.0),
        ];
        assert_eq!(active_days(&dates, &rows), 2);
        assert_eq!(active_days(&dates, &[]), 0);
    }

    #[test]
    fn ranges_cover_thirty_days_or_a_calendar_month_to_date() {
        let today = day("2026-11-08");
        assert_eq!(UsageRange::Last30Days.dates(today).len(), 30);
        let options = UsageRange::options(today);
        assert_eq!(options.len(), 14);
        assert_eq!(
            options[1],
            UsageRange::Month {
                year: 2026,
                month: 11
            }
        );
        assert_eq!(
            options[13],
            UsageRange::Month {
                year: 2025,
                month: 11
            }
        );
        assert_eq!(options[1].dates(today).len(), 8);
        assert_eq!(options[2].dates(today).len(), 31);
        assert_eq!(options[13].query(), "month:2025-11");
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
    }

    #[test]
    fn numbers_format_like_the_web_page() {
        assert_eq!(fmt_count(10_499.6), "10,500");
        assert_eq!(fmt_compact(950.0), "950");
        assert_eq!(fmt_compact(12.5), "12.5");
        assert_eq!(fmt_compact(20_000.0), "20K");
        assert_eq!(fmt_compact(1_234_567.0), "1.2M");
        assert_eq!(fmt_compact(123_456.0), "123.5K");
        assert_eq!(fmt_compact(999_999.0), "1M");
        assert_eq!(fmt_price(1_500.0), "1,500");
        assert_eq!(fmt_price(0.25), "0.25");
        assert_eq!(fmt_price(2.5), "2.5");
        assert_eq!(percent(1.0, 3.0), 33);
        assert_eq!(percent(1.0, 0.0), 0);
    }
}
