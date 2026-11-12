//! Settings → Usage for accounts with the `usage_page` flag: under the plan
//! card every account sees (`billing.rs`), credits against the allowance
//! and where the pace leads, then the web app's Usage page in Maple Work: a
//! chosen range by day, model and app, the last twelve months, recent
//! requests and current model prices. The billing API and the arithmetic
//! (`usage/model.rs`) are the web page's, so both show the same numbers.

mod chart;
mod explorer;
mod labels;
mod model;
mod palette;
mod plan;
mod sections;
mod ui;

use std::rc::Rc;

use chrono::{Datelike, NaiveDate, Weekday};
use gpui::{Context, Div, SharedString, Window, div, prelude::*, px};

use self::chart::Tip;
use self::labels::*;
use self::model::{
    DailyChart, Metric, ModelRanking, MonthBucket, Origin, PeriodProgress, PlanKind, PlanWindow,
    RangeSummary, SeriesKey, Split, TokenKind, UsageRange, fmt_compact, fmt_count, fmt_price,
    percent,
};
use self::ui::*;
use super::billing::BillingTarget;
use super::{Section, SettingMenu, SettingsScreen, SettingsTarget, subsection_title};
use crate::billing::{UsageModelLabels, UsagePrices, UsageQuery, UsageRequest, UsageSummary};
use crate::ui::i18n::tr;
use crate::ui::icons::{icon, spinner};
use crate::ui::{theme, widgets};

/// Recent requests open short; each "Load more" fetches a larger page.
const REQUESTS_FIRST_PAGE: u32 = 10;
const REQUESTS_MORE_PAGE: u32 = 50;

/// Controls on the Usage page that Application Vim can land on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum UsageTarget {
    Range,
    Metric(Metric),
    Split(Split),
    LoadMore,
}

#[derive(Default)]
struct RequestsState {
    rows: Vec<UsageRequest>,
    /// Each row's time in the local zone, parsed once as its page arrives.
    times: Vec<Option<chrono::DateTime<chrono::Local>>>,
    labels: UsageModelLabels,
    next_cursor: Option<String>,
    loading: bool,
    /// The last reload failed: with no rows there is nothing to show, and
    /// any rows are from an earlier load.
    failed: bool,
    /// A later page failed: the rows stay, with a retry.
    more_failed: bool,
    loaded: bool,
    /// Bumped by a reload, so a page from before it is dropped.
    seq: u64,
}

pub(super) struct UsageState {
    /// The account's `usage_page` flag; `None` until it is known.
    pub(super) enabled: Option<bool>,
    flag_loading: bool,
    /// The last flag read failed, so `enabled` is a fail-closed `false`
    /// that the next billing load asks again.
    flag_failed: bool,
    /// IANA zone the history is bucketed in.
    tz: String,
    pub(super) range: UsageRange,
    pub(super) metric: Metric,
    pub(super) split: Split,
    summary: Option<UsageSummary>,
    /// Catalog grouping for highlights; an older response falls back in model.
    catalog: Option<maple_sdk::ModelCatalogResponse>,
    catalog_loading: bool,
    /// The range `summary` answers.
    summary_range: Option<UsageRange>,
    /// The range whose last load failed, until a load succeeds.
    failed_range: Option<UsageRange>,
    summary_loading: bool,
    summary_seq: u64,
    requests: RequestsState,
    prices: Option<UsagePrices>,
    prices_failed: bool,
    prices_loading: bool,
    /// Everything the page draws from the loaded history, worked out when
    /// it, the status or the chart's controls change.
    data: Option<Rc<UsageData>>,
}

impl UsageState {
    pub(super) fn new() -> Self {
        Self {
            enabled: None,
            flag_loading: false,
            flag_failed: false,
            tz: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
            range: UsageRange::Last30Days,
            metric: Metric::Credits,
            split: Split::Model,
            summary: None,
            catalog: None,
            catalog_loading: false,
            summary_range: None,
            failed_range: None,
            summary_loading: false,
            summary_seq: 0,
            requests: RequestsState::default(),
            prices: None,
            prices_failed: false,
            prices_loading: false,
            data: None,
        }
    }

    /// The full page shows for flagged accounts on a plan with credits.
    pub(super) fn shows_insights(&self, status: Option<&crate::billing::BillingStatus>) -> bool {
        self.enabled == Some(true)
            && status.is_some_and(|status| model::plan_kind(status) != PlanKind::Free)
    }

    /// The selected range's last load failed and no retry is running.
    pub(super) fn range_failed(&self) -> bool {
        !self.summary_loading && self.failed_range == Some(self.range)
    }
}

/// The plan card's inputs.
struct PlanData {
    kind: PlanKind,
    window: Option<PlanWindow>,
    progress: Option<PeriodProgress>,
    used: f64,
    allowance: f64,
    /// Team: this account's share of the pool, from the period's daily
    /// credits once they arrive.
    team_you: Option<f64>,
}

/// The chosen range, worked out.
struct RangeData {
    dates: Vec<NaiveDate>,
    summary: RangeSummary,
    chart: DailyChart,
    /// Days in the range with any usage.
    active_days: usize,
    /// Where the plan's billing period starts, when it falls in the range.
    reset_index: Option<usize>,
    extra_span: Option<(usize, usize)>,
}

pub(super) struct UsageData {
    today: NaiveDate,
    plan: PlanData,
    /// Catalog-derived model colors shared by every Usage section.
    model_ranking: ModelRanking,
    /// The plan card's percent and reset, from the same status as the
    /// lines under it.
    plan_usage: Option<crate::billing::PlanUsage>,
    /// The selected range, or while it loads the last one; `None` after it
    /// failed, so another range's numbers never show under its name.
    range: Option<RangeData>,
    /// The last twelve months, which every range's answer carries.
    months: Option<Vec<MonthBucket>>,
    /// Models in the range without a price today, with their names.
    unpriced: Vec<(String, String)>,
}

impl SettingsScreen {
    /// Ask whether this account sees the full page: once, or again after a
    /// failed read.
    pub(super) fn load_usage_flag(&mut self, cx: &mut Context<Self>) {
        if self.usage.flag_loading || (self.usage.enabled.is_some() && !self.usage.flag_failed) {
            return;
        }
        self.usage.flag_loading = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<bool>("usage_page_enabled", (&user_id,))
                    .await
            },
            cx,
            |this, result, cx| this.usage_flag_loaded(result.ok(), cx),
        );
    }

    /// The flag read finished; `None` is a failed read, which stays off.
    fn usage_flag_loaded(&mut self, enabled: Option<bool>, cx: &mut Context<Self>) {
        self.usage.flag_loading = false;
        self.usage.flag_failed = enabled.is_none();
        self.usage.enabled = Some(enabled.unwrap_or(false));
        // A billing status still on its way loads the page when it lands,
        // with the plan window it brings; loading here as well would fetch
        // everything twice.
        if !self.usage.flag_failed
            && self.section == Section::Usage
            && self.billing.status_pending == 0
        {
            self.load_usage(cx);
        }
        self.usage_changed(cx);
    }

    /// Fetch the history, recent requests and prices for the page. Needs
    /// the flag and billing status; runs again when either arrives.
    pub(super) fn load_usage(&mut self, cx: &mut Context<Self>) {
        if self.usage.enabled == Some(true) {
            self.load_usage_catalog(cx);
        }
        if !self.usage.shows_insights(self.billing.status.as_ref()) {
            self.load_usage_flag(cx);
            return;
        }
        self.load_usage_summary(cx);
        self.load_usage_requests(true, cx);
        self.load_usage_prices(cx);
    }

    /// Optional highlight metadata. Never hold billing for this request: a
    /// stalled catalog leaves the history usable with cached or legacy colors.
    fn load_usage_catalog(&mut self, cx: &mut Context<Self>) {
        if self.usage.catalog_loading {
            return;
        }
        self.usage.catalog_loading = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request_user_id = user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<maple_sdk::ModelCatalogResponse>(
                        "model_catalog",
                        (&request_user_id,),
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                this.usage.catalog_loading = false;
                if this.user_id != user_id {
                    return;
                }
                if let Ok(catalog) = result {
                    this.usage.catalog = Some(catalog);
                    // Apply to the current selection; never resurrect an
                    // old range's summary or loading state.
                    this.rebuild_usage_data();
                }
                this.usage_changed(cx);
            },
        );
    }

    /// Today's model prices; fetched once, or again after a failure.
    pub(super) fn load_usage_prices(&mut self, cx: &mut Context<Self>) {
        if self.usage.prices.is_some() || self.usage.prices_loading {
            return;
        }
        self.usage.prices_loading = true;
        let backend = self.backend.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<UsagePrices>("usage_prices", ())
                    .await
            },
            cx,
            |this, result, cx| {
                this.usage.prices_loading = false;
                this.usage.prices_failed = result.is_err();
                if let Ok(prices) = result {
                    this.usage.prices = Some(prices);
                    this.rebuild_usage_data();
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn load_usage_summary(&mut self, cx: &mut Context<Self>) {
        self.load_usage_catalog(cx);
        let Some(status) = self.billing.status.as_ref() else {
            return;
        };
        let window = model::status_window(status);
        let query = UsageQuery {
            range: self.usage.range.query(),
            tz: self.usage.tz.clone(),
            period_start: window.as_ref().map(|window| window.start_raw.clone()),
            period_end: window.map(|window| window.end_raw),
        };
        self.usage.summary_seq += 1;
        let seq = self.usage.summary_seq;
        let range = self.usage.range;
        self.usage.summary_loading = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<UsageSummary>("usage_summary", (&user_id, query))
                    .await
            },
            cx,
            move |this, result, cx| {
                // A newer range or refresh supersedes this answer.
                if this.usage.summary_seq != seq {
                    return;
                }
                this.usage.summary_loading = false;
                match result {
                    Ok(summary) => {
                        this.usage.summary = Some(summary);
                        this.usage.summary_range = Some(range);
                        this.usage.failed_range = None;
                    }
                    Err(_) => this.usage.failed_range = Some(range),
                }
                this.rebuild_usage_data();
                this.usage_changed(cx);
            },
        );
        cx.notify();
    }

    pub(super) fn load_usage_requests(&mut self, reset: bool, cx: &mut Context<Self>) {
        let requests = &mut self.usage.requests;
        if reset {
            requests.seq += 1;
        } else if requests.loading || requests.next_cursor.is_none() {
            return;
        }
        let seq = requests.seq;
        let cursor = if reset {
            None
        } else {
            requests.next_cursor.clone()
        };
        let limit = if cursor.is_some() {
            REQUESTS_MORE_PAGE
        } else {
            REQUESTS_FIRST_PAGE
        };
        requests.loading = true;
        requests.more_failed = false;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        self.call_presented(
            async move {
                backend
                    .call_presented::<crate::billing::UsageRequestsPage>(
                        "usage_requests",
                        (&user_id, cursor, limit),
                    )
                    .await
            },
            cx,
            move |this, result, cx| {
                let requests = &mut this.usage.requests;
                if requests.seq != seq {
                    return;
                }
                requests.loading = false;
                match result {
                    Ok(page) => {
                        let times = page.requests.iter().map(|request| {
                            chrono::DateTime::parse_from_rfc3339(&request.time)
                                .ok()
                                .map(|at| at.with_timezone(&chrono::Local))
                        });
                        if reset {
                            requests.times = times.collect();
                            requests.rows = page.requests;
                            requests.labels = page.models;
                            requests.failed = false;
                        } else {
                            requests.times.extend(times);
                            requests.rows.extend(page.requests);
                            requests.labels.extend(page.models);
                        }
                        requests.next_cursor = page.next_cursor;
                        requests.loaded = true;
                    }
                    // Keep rows a refresh couldn't replace, and say so.
                    Err(_) if reset => requests.failed = true,
                    Err(_) => requests.more_failed = true,
                }
                this.usage_changed(cx);
            },
        );
        cx.notify();
    }

    /// A billing status load finished. A new status moves the plan card and
    /// the period the history is asked for; a failed load changes neither,
    /// so it only loads a page that has nothing to show yet.
    pub(super) fn usage_status_loaded(&mut self, loaded: bool, cx: &mut Context<Self>) {
        if self.section == Section::Usage
            && (loaded || (self.usage.summary.is_none() && !self.usage.summary_loading))
        {
            self.load_usage(cx);
        }
        self.rebuild_usage_data();
    }

    fn usage_changed(&mut self, cx: &mut Context<Self>) {
        if self.settings.application_vim_enabled {
            self.reconcile_application_vim_target();
        }
        cx.notify();
    }

    pub(super) fn choose_usage_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        if self.usage.range == range {
            return;
        }
        self.usage.range = range;
        self.load_usage_summary(cx);
    }

    pub(super) fn choose_usage_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        let kind = self.usage_kind();
        self.usage.split = self.usage.split.after_metric_change(metric, kind);
        self.usage.metric = metric;
        self.rebuild_usage_data();
        self.usage_changed(cx);
    }

    pub(super) fn choose_usage_split(&mut self, split: Split, cx: &mut Context<Self>) {
        self.usage.split = split;
        self.rebuild_usage_data();
        self.usage_changed(cx);
    }

    fn usage_kind(&self) -> PlanKind {
        self.billing
            .status
            .as_ref()
            .map(model::plan_kind)
            .unwrap_or(PlanKind::Credits)
    }

    fn rebuild_usage_data(&mut self) {
        let Some(status) = self.billing.status.as_ref() else {
            self.usage.data = None;
            return;
        };
        let now = chrono::Utc::now();
        let today = chrono::Local::now().date_naive();
        let kind = model::plan_kind(status);
        let window = model::status_window(status);
        let summary = self.usage.summary.as_ref();
        let period = summary.and_then(|summary| summary.current_period.as_ref());
        let used = status.used_tokens.unwrap_or(0).max(0) as f64;
        let allowance = status.total_tokens.unwrap_or(0) as f64;
        let progress = window
            .as_ref()
            .filter(|_| allowance > 0.0)
            .and_then(|window| model::build_period_progress(window, used, allowance, now));
        let plan = PlanData {
            kind,
            team_you: period.map(|period| period.days.iter().map(|day| day.plan_credits).sum()),
            window,
            progress,
            used,
            allowance,
        };
        // While a range loads, the last answer stands in, dimmed, as on the
        // web, unless this range already failed; after its own load fails,
        // only the error shows.
        let selected = Some(self.usage.range);
        let shown = summary.filter(|_| {
            self.usage.summary_range == selected
                || (self.usage.summary_loading && self.usage.failed_range != selected)
        });
        let model_ranking = model::ModelRanking::from_catalog(self.usage.catalog.as_ref());
        let range = shown.map(|summary| {
            let dates = model::response_dates(
                self.usage.range,
                &summary.range.start_date,
                &summary.range.end_date,
                &summary.days,
                today,
            );
            let metric = self.usage.metric;
            let chart = model::build_daily_chart(
                &dates,
                &summary.days,
                metric,
                self.usage.split,
                &summary.models,
                &model_ranking,
            );
            let reset_index = plan.window.as_ref().and_then(|window| {
                let start = model::local_date(window.start);
                dates
                    .iter()
                    .position(|date| *date == start)
                    .filter(|index| *index > 0)
            });
            RangeData {
                summary: model::summarize_days(&summary.days, &summary.models),
                active_days: model::active_days(&dates, &summary.days),
                extra_span: model::extra_credit_span(&chart.days),
                chart,
                reset_index,
                dates,
            }
        });
        let months = summary
            .map(|summary| model::build_months(&summary.months, (today.year(), today.month()), 12));
        let unpriced = match (&range, &self.usage.prices) {
            (Some(range), Some(prices)) => range
                .summary
                .by_model
                .iter()
                .filter(|model| !prices.models.iter().any(|price| price.id == model.model))
                .map(|model| (model.model.clone(), model.label.clone()))
                .collect(),
            _ => Vec::new(),
        };
        self.usage.data = Some(Rc::new(UsageData {
            today,
            plan,
            model_ranking,
            plan_usage: crate::billing::PlanUsage::from_status(status, chrono::Local::now()),
            range,
            months,
            unpriced,
        }));
    }

    pub(super) fn usage_targets(&self) -> Vec<SettingsTarget> {
        let thin = vec![
            SettingsTarget::PageRefresh,
            SettingsTarget::Billing(BillingTarget::ManagePlan),
        ];
        if !self.usage.shows_insights(self.billing.status.as_ref()) {
            return thin;
        }
        let usage = |target| SettingsTarget::Billing(BillingTarget::Usage(target));
        let kind = self.usage_kind();
        let mut targets = vec![
            SettingsTarget::PageRefresh,
            SettingsTarget::Billing(BillingTarget::ManagePlan),
            usage(UsageTarget::Range),
        ];
        // Show and Split by are drawn only with the range's numbers.
        if self
            .usage
            .data
            .as_ref()
            .is_some_and(|data| data.range.is_some())
        {
            targets.extend(Metric::ALL.map(|m| usage(UsageTarget::Metric(m))));
            let splits = Split::options(self.usage.metric, kind);
            if splits.len() > 1 {
                targets.extend(splits.into_iter().map(|s| usage(UsageTarget::Split(s))));
            }
        }
        if self.usage.requests.next_cursor.is_some() {
            targets.push(usage(UsageTarget::LoadMore));
        }
        targets
    }

    pub(super) fn activate_usage_target(&mut self, target: UsageTarget, cx: &mut Context<Self>) {
        match target {
            UsageTarget::Range => self.toggle_setting_menu(SettingMenu::UsageRange, cx),
            UsageTarget::Metric(metric) => self.choose_usage_metric(metric, cx),
            UsageTarget::Split(split) => self.choose_usage_split(split, cx),
            UsageTarget::LoadMore => self.load_usage_requests(false, cx),
        }
    }

    // ---- The range dropdown, through Settings' one popup -------------------

    pub(super) fn usage_range_options(&self) -> Vec<super::SettingOption> {
        let today = chrono::Local::now().date_naive();
        UsageRange::options(today)
            .into_iter()
            .map(|range| super::SettingOption {
                label: self.usage_range_option(range, today).to_string(),
                current: range == self.usage.range,
            })
            .collect()
    }

    pub(super) fn pick_usage_range(&mut self, index: usize, cx: &mut Context<Self>) {
        let today = chrono::Local::now().date_naive();
        if let Some(range) = UsageRange::options(today).get(index) {
            self.choose_usage_range(*range, cx);
        }
    }

    /// The range's name on its button.
    pub(super) fn usage_range_name(&self, range: UsageRange, today: NaiveDate) -> SharedString {
        match range {
            UsageRange::Last30Days => tr("settings-usage-range-last-30-days"),
            UsageRange::Month { .. } if range.is_current_month(today) => {
                tr("settings-usage-range-this-month")
            }
            UsageRange::Month { year, month } => self.month_year(year, month),
        }
    }

    /// The range's name in the dropdown, where this month shows its dates:
    /// `This month · Nov 1 – Nov 8`.
    fn usage_range_option(&self, range: UsageRange, today: NaiveDate) -> SharedString {
        let dates = range.dates(today);
        match (dates.first(), dates.last()) {
            (Some(first), Some(last)) if range.is_current_month(today) => self.messages.get(
                "settings-usage-range-this-month-dates",
                [("dates", self.date_span(*first, *last).to_string().into())],
            ),
            _ => self.usage_range_name(range, today),
        }
    }

    // ---- Rendering ------------------------------------------------------------

    /// The plan card's reading for accounts with the full page, from the
    /// same status as the credits and pace under it, so the two agree.
    pub(super) fn usage_plan_usage(&self) -> Option<&crate::billing::PlanUsage> {
        self.usage.data.as_ref()?.plan_usage.as_ref()
    }

    /// Everything under the plan summary for accounts with the full page:
    /// the range's history, the last twelve months, recent requests, prices
    /// and notes.
    pub(super) fn render_usage_insights(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let page = div().flex().flex_col().gap_8().pt_4();
        let Some(data) = self.usage.data.clone() else {
            return page.child(loading_block("usage-loading", tr("settings-usage-loading")));
        };
        page.child(self.render_usage_range(&data, window, cx))
            .children(
                data.months
                    .as_ref()
                    .map(|months| self.render_usage_months(months)),
            )
            .child(self.render_usage_requests(&data, cx))
            .child(self.render_usage_prices(&data, cx))
            .child(self.render_usage_about(&data))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Days;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    use super::*;
    use crate::billing::{BillingStatus, UsageCounts, UsageDayRow};
    use maple_billing::UsageCurrentPeriod;

    struct Host {
        settings: Entity<SettingsScreen>,
    }

    impl gpui::Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.settings.clone())
        }
    }

    fn usage_window(cx: &mut TestAppContext) -> (Entity<SettingsScreen>, &mut VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(crate::desktop::register_key_bindings);
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        let settings = cx.new(|cx| {
            SettingsScreen::new(
                backend.clone(),
                backend,
                "user".to_string(),
                crate::settings::AppSettings::default(),
                crate::shortcuts::ShortcutSnapshot {
                    generation: 1,
                    rows: Vec::new(),
                    last_error: None,
                    compatibility_warning: None,
                },
                Section::Usage,
                cx,
            )
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| Host {
            settings: settings.clone(),
        });
        cx.simulate_resize(gpui::size(px(1280.), px(2400.)));
        cx.update(|window, _| window.activate_window());
        // The test backend has no session: the flag and billing loads fail
        // here, before the test sets its own state.
        settle(&settings, cx, |this| {
            this.billing.status_error.is_some() && !this.usage.flag_loading
        });
        (settings, cx)
    }

    /// Run until `done`: the backend answers on its own threads, which
    /// `run_until_parked` does not wait for.
    fn settle(
        settings: &Entity<SettingsScreen>,
        cx: &mut VisualTestContext,
        done: impl Fn(&SettingsScreen) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if settings.read_with(cx, |this, _| done(this)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "backend answers in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn day(
        date: NaiveDate,
        model: &str,
        app: Option<&str>,
        pool: &str,
        credits: f64,
    ) -> UsageDayRow {
        UsageDayRow {
            date: date.format("%Y-%m-%d").to_string(),
            model: model.into(),
            source: "app".into(),
            client_app: app.map(str::to_string),
            pool: pool.into(),
            counts: UsageCounts {
                requests: 3,
                uncached_input_tokens: 4_000,
                cached_input_tokens: 1_000,
                cached_reported_requests: 3,
                output_tokens: 500,
                credits,
            },
        }
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} renders"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn history_shows_for_flagged_accounts_and_metric_changes_keep_valid_splits(
        cx: &mut TestAppContext,
    ) {
        let (settings, cx) = usage_window(cx);
        let today = chrono::Local::now().date_naive();
        let start = today - Days::new(3);
        settings.update(cx, |this, cx| {
            assert_eq!(this.usage.enabled, Some(false), "no session, no flag");
            this.billing.status = Some(BillingStatus {
                is_subscribed: true,
                product_name: Some("Pro".into()),
                total_tokens: Some(20_000),
                used_tokens: Some(240),
                usage_period_start: Some(format!("{start}T12:00:00Z")),
                usage_reset_date: Some(format!("{}T12:00:00Z", start + Days::new(30))),
                api_credit_balance: Some(1_000),
                ..BillingStatus::default()
            });
            this.plan = crate::billing::PlanUsage::from_status(
                this.billing.status.as_ref().expect("status"),
                chrono::Local::now(),
            );
            this.usage.summary = Some(UsageSummary {
                days: vec![
                    day(today - Days::new(1), "glm-5-3", Some("work"), "plan", 200.0),
                    day(today, "kimi-k3", None, "extra", 40.0),
                ],
                current_period: Some(UsageCurrentPeriod::default()),
                ..UsageSummary::default()
            });
            this.usage.summary_range = Some(this.usage.range);
            this.rebuild_usage_data();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("usage-refresh").is_some());
        assert!(
            cx.debug_bounds("usage-total-credits").is_none()
                && cx.debug_bounds("usage-plan-details").is_none(),
            "without the flag, Usage keeps the plan summary"
        );

        settings.update(cx, |this, cx| {
            this.usage.enabled = Some(true);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("usage-plan-details").is_some(),
            "the plan card leads, with credits against the allowance"
        );
        assert!(cx.debug_bounds("usage-total-credits").is_some());
        assert!(cx.debug_bounds("usage-daily-29").is_some(), "30 daily bars");
        assert!(cx.debug_bounds("usage-split-pool").is_some());

        click(cx, "usage-metric-tokens");
        settings.update(cx, |this, _| assert_eq!(this.usage.split, Split::Token));
        assert!(cx.debug_bounds("usage-split-token").is_some());
        assert!(
            cx.debug_bounds("usage-split-pool").is_none(),
            "Plan / Extra only splits credits"
        );

        click(cx, "usage-split-app");
        click(cx, "usage-metric-requests");
        settings.update(cx, |this, _| {
            assert_eq!(this.usage.metric, Metric::Requests);
            assert_eq!(this.usage.split, Split::Origin, "App still splits requests");
        });
        click(cx, "usage-metric-tokens");
        click(cx, "usage-metric-credits");
        settings.update(cx, |this, _| assert_eq!(this.usage.split, Split::Model));
    }

    #[gpui::test]
    fn failed_loads_retry_and_never_show_another_ranges_numbers(cx: &mut TestAppContext) {
        let (settings, cx) = usage_window(cx);
        let today = chrono::Local::now().date_naive();
        settings.update(cx, |this, cx| {
            // The flag read failed at open: the page stays thin, and the next
            // billing load asks again.
            assert!(this.usage.flag_failed);
            this.load_usage_flag(cx);
            assert!(this.usage.flag_loading, "a failed flag read is retried");
        });
        settle(&settings, cx, |this| !this.usage.flag_loading);
        settings.update(cx, |this, cx| {
            assert_eq!(this.usage.enabled, Some(false));
            this.usage.enabled = Some(true);
            this.usage.flag_failed = false;
            this.load_usage_flag(cx);
            assert!(!this.usage.flag_loading, "a known flag is not asked again");

            this.billing.status = Some(BillingStatus {
                is_subscribed: true,
                product_name: Some("Pro".into()),
                total_tokens: Some(20_000),
                used_tokens: Some(240),
                ..BillingStatus::default()
            });
            this.usage.summary = Some(UsageSummary {
                days: vec![day(today, "glm-5-3", Some("work"), "plan", 200.0)],
                current_period: Some(UsageCurrentPeriod::default()),
                ..UsageSummary::default()
            });
            this.usage.summary_range = Some(UsageRange::Last30Days);
            this.usage.requests.rows = vec![crate::billing::UsageRequest {
                time: format!("{today}T12:00:00Z"),
                model: "glm-5-3".into(),
                ..Default::default()
            }];
            this.usage.requests.times = vec![None];
            this.usage.requests.loaded = true;
            this.rebuild_usage_data();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("usage-total-credits").is_some());

        // A refresh of the requests fails: the old rows stay, flagged.
        settings.update(cx, |this, cx| this.load_usage_requests(true, cx));
        settle(&settings, cx, |this| !this.usage.requests.loading);
        settings.update(cx, |this, _| assert_eq!(this.usage.requests.rows.len(), 1));
        assert!(cx.debug_bounds("usage-requests-error-retry").is_some());

        // Loading another range fails: Last 30 days' numbers must not show
        // under its name.
        let earlier = UsageRange::options(today)[2];
        settings.update(cx, |this, cx| this.choose_usage_range(earlier, cx));
        settle(&settings, cx, |this| !this.usage.summary_loading);
        assert!(cx.debug_bounds("usage-range-error-retry").is_some());
        assert!(cx.debug_bounds("usage-total-credits").is_none());
        settings.update(cx, |this, _| {
            let data = this.usage.data.as_ref().expect("usage data");
            assert!(
                data.plan.team_you.is_some() && data.months.is_some(),
                "the period's credits and the last 12 months don't depend on the range"
            );
        });
        // Nor while that range is retried: a rebuild mid-load (a status
        // refresh, a new metric) must not bring them back.
        settings.update(cx, |this, cx| {
            this.load_usage_summary(cx);
            this.rebuild_usage_data();
            let data = this.usage.data.as_ref().expect("usage data");
            assert!(data.range.is_none());
            assert!(
                !this
                    .usage_targets()
                    .contains(&SettingsTarget::Billing(BillingTarget::Usage(
                        UsageTarget::Metric(Metric::Tokens)
                    ))),
                "Show is not drawn, so Vim can't land on it"
            );
        });
        settle(&settings, cx, |this| !this.usage.summary_loading);

        // Failing again for the range already shown keeps its numbers.
        settings.update(cx, |this, cx| {
            this.usage.summary = Some(UsageSummary {
                days: vec![day(today, "glm-5-3", Some("work"), "plan", 200.0)],
                ..UsageSummary::default()
            });
            this.usage.summary_range = Some(earlier);
            this.load_usage_summary(cx);
        });
        settle(&settings, cx, |this| !this.usage.summary_loading);
        assert!(cx.debug_bounds("usage-range-error-retry").is_some());
        assert!(cx.debug_bounds("usage-total-credits").is_some());
    }

    #[gpui::test]
    fn catalog_refreshes_without_billing_and_keeps_a_cached_catalog_on_error(
        cx: &mut TestAppContext,
    ) {
        let (settings, cx) = usage_window(cx);
        settings.update(cx, |this, cx| {
            assert!(this.billing.status.is_none());
            this.usage.enabled = Some(true);
            this.usage.flag_failed = false;
            this.load_usage(cx);
            assert!(
                this.usage.catalog_loading,
                "metadata does not wait for billing"
            );
            assert!(
                !this.usage.summary_loading,
                "history still needs billing status"
            );
        });
        settle(&settings, cx, |this| !this.usage.catalog_loading);
        settings.update(cx, |this, cx| {
            this.usage.catalog = Some(maple_sdk::ModelCatalogResponse {
                object: "last-good".into(),
                ..Default::default()
            });
            this.load_usage_summary(cx);
            assert!(
                this.usage.catalog_loading,
                "a cached catalog is revalidated"
            );
            assert!(
                !this.usage.summary_loading,
                "no billing status is available"
            );
        });
        settle(&settings, cx, |this| !this.usage.catalog_loading);
        settings.update(cx, |this, _cx| {
            assert_eq!(
                this.usage.catalog.as_ref().map(|catalog| catalog.object.as_str()),
                Some("last-good"),
                "the test backend has no account, so this failed refresh retains the last good catalog"
            );
        });
    }

    #[gpui::test]
    fn plan_card_reads_one_status_and_paces_before_the_history_arrives(cx: &mut TestAppContext) {
        let (settings, cx) = usage_window(cx);
        let start = chrono::Utc::now() - chrono::Duration::days(3);
        let status = |used: i64, product: &str| BillingStatus {
            is_subscribed: true,
            product_name: Some(product.into()),
            total_tokens: Some(7_000),
            used_tokens: Some(used),
            usage_period_start: Some(start.to_rfc3339()),
            usage_reset_date: Some((start + chrono::Duration::days(30)).to_rfc3339()),
            ..BillingStatus::default()
        };
        settings.update(cx, |this, _| {
            this.usage.enabled = Some(true);
            this.usage.flag_failed = false;
            this.billing.status = Some(status(2_400, "Pro"));
            // A plan reading from an older, separate load.
            this.plan =
                crate::billing::PlanUsage::from_status(&status(100, "Pro"), chrono::Local::now());
            this.rebuild_usage_data();
            assert_eq!(
                this.usage_plan_usage().map(|plan| plan.percent_used),
                Some(34),
                "the card's percent comes from the status the credits line uses"
            );
            let data = this.usage.data.clone().expect("usage data");
            assert!(this.usage.summary.is_none());
            assert!(
                this.pace_text(&data.plan).is_some(),
                "the pace needs only the status, not the history"
            );

            for used in [0, 1] {
                this.billing.status = Some(status(used, "Pro"));
                this.rebuild_usage_data();
                let data = this.usage.data.clone().expect("usage data");
                assert!(
                    this.pace_text(&data.plan).is_none(),
                    "under a credit a day the bar says enough, not \"About 0 credits a day\""
                );
            }

            assert!(
                !this.usage.shows_insights(Some(&status(0, "Free"))),
                "Free keeps the plan summary even with the flag"
            );
        });
    }
    #[gpui::test]
    fn a_team_member_is_pointed_to_their_own_extra_credits(cx: &mut TestAppContext) {
        let (settings, cx) = usage_window(cx);
        let start = chrono::Utc::now() - chrono::Duration::days(3);
        let status = |used: i64, credits: Option<i64>| BillingStatus {
            is_subscribed: true,
            product_name: Some("Team".into()),
            total_tokens: Some(7_000),
            used_tokens: Some(used),
            api_credit_balance: credits,
            usage_period_start: Some(start.to_rfc3339()),
            usage_reset_date: Some((start + chrono::Duration::days(30)).to_rfc3339()),
            ..BillingStatus::default()
        };
        settings.update(cx, |this, _| {
            this.usage.enabled = Some(true);
            this.usage.flag_failed = false;
            let mut pace = |used, credits| {
                this.billing.status = Some(status(used, credits));
                this.rebuild_usage_data();
                let data = this.usage.data.clone().expect("usage data");
                this.pace_text(&data.plan).expect("pace").to_string()
            };
            // The pool running out stops nobody who has credits of their own.
            let empty = pace(7_000, None);
            assert!(empty.contains("Add Extra credits to your own account"));
            let covered = pace(7_000, Some(500));
            assert!(covered.contains("Your own Extra credits cover your usage"));
            let ahead = pace(6_000, Some(0));
            assert!(ahead.contains("you need your own Extra credits"));
            let ahead_covered = pace(6_000, Some(500));
            assert!(ahead_covered.contains("your own Extra credits cover your usage"));
        });
    }
    /// The flag and the billing status arrive in either order on a visit;
    /// together they fetch the history once, and a status load that failed
    /// does not fetch it again.
    #[gpui::test]
    fn a_visit_fetches_the_history_once(cx: &mut TestAppContext) {
        let (settings, cx) = usage_window(cx);
        settings.update(cx, |this, cx| {
            this.billing.status = Some(BillingStatus {
                is_subscribed: true,
                product_name: Some("Pro".into()),
                total_tokens: Some(7_000),
                ..BillingStatus::default()
            });
            let fetched = this.usage.summary_seq;

            // The flag lands while the status is still on its way.
            this.billing.status_pending = 1;
            this.usage_flag_loaded(Some(true), cx);
            assert_eq!(this.usage.summary_seq, fetched, "the status will load it");
            this.billing.status_pending = 0;
            this.usage_status_loaded(true, cx);
            assert_eq!(this.usage.summary_seq, fetched + 1);

            // A failed status load leaves the running fetch alone.
            this.usage_status_loaded(false, cx);
            assert_eq!(this.usage.summary_seq, fetched + 1);

            // With no status load in flight, the flag loads the page itself.
            this.usage_flag_loaded(Some(true), cx);
            assert_eq!(this.usage.summary_seq, fetched + 2);
        });
    }
}
