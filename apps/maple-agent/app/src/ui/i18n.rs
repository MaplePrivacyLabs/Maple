//! Embedded Fluent catalogs. Only presentation text belongs here, never protocol
//! values, task content, or model prompts. Plain labels are cached per process
//! locale; parameterized messages should be formatted when their state changes.
use std::{cell::RefCell, collections::HashMap, sync::LazyLock};

use fluent_bundle::FluentValue;
use fluent_templates::{LanguageIdentifier, langid};
use gpui::SharedString;

fluent_templates::static_loader! {
    static CATALOGS = {
        locales: "./locales",
        fallback_language: "en",
    };
}

const ENGLISH: LanguageIdentifier = langid!("en");

/// Isolate locale state between UI tests without changing process environment.
#[cfg(test)]
pub(super) struct TestLocalizationGuard;
#[cfg(test)]
impl TestLocalizationGuard {
    pub(super) fn enable() -> Self {
        Self
    }
}
#[cfg(test)]
impl Drop for TestLocalizationGuard {
    fn drop(&mut self) {
        ACTIVE.with_borrow_mut(|state| {
            state.locale = ENGLISH;
            state.loaded = false;
            state.messages.clear();
            state.system_label = None;
        });
    }
}

/// Enabled catalogs. None is the persisted system preference. Add a language
/// here only when its catalog has passed the completeness checks.
pub(super) const LANGUAGES: &[Option<&str>] = &[None, Some("en"), Some("es")];
static NATIVE_NAMES: LazyLock<HashMap<&'static str, SharedString>> = LazyLock::new(|| {
    LANGUAGES
        .iter()
        .flatten()
        .map(|locale| {
            (
                *locale,
                lookup(
                    &locale.parse().expect("enabled locale"),
                    "language-name",
                    &HashMap::new(),
                )
                .into(),
            )
        })
        .collect()
});
static SYSTEM: LazyLock<LanguageIdentifier> = LazyLock::new(|| {
    // English assertions must not depend on the machine running the suite.
    #[cfg(test)]
    {
        ENGLISH
    }
    #[cfg(not(test))]
    {
        resolve(sys_locale::get_locale().as_deref())
    }
});

struct LocaleState {
    locale: LanguageIdentifier,
    loaded: bool,
    messages: HashMap<&'static str, SharedString>,
    system_label: Option<SharedString>,
}

thread_local! {
    // GPUI renders on its owning thread. Tests have independent UI threads.
    static ACTIVE: RefCell<LocaleState> = RefCell::new(LocaleState {
        locale: SYSTEM.clone(), loaded: false, messages: HashMap::new(), system_label: None,
    });
}

fn resolve(requested: Option<&str>) -> LanguageIdentifier {
    resolve_enabled(requested, LANGUAGES)
}

fn resolve_enabled(requested: Option<&str>, enabled: &[Option<&str>]) -> LanguageIdentifier {
    let Some(requested) = requested else {
        return ENGLISH;
    };
    // Accept POSIX locale spellings as well as BCP-47 from platform APIs.
    let normalized = requested
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");
    let Ok(mut locale) = normalized.parse::<LanguageIdentifier>() else {
        return ENGLISH;
    };
    loop {
        let tag = locale.to_string();
        if enabled.iter().flatten().any(|enabled| *enabled == tag) {
            return locale;
        }
        if locale.variants().next().is_some() {
            locale.clear_variants();
        } else if locale.region.is_some() {
            locale.region = None;
        } else if locale.script.is_some() {
            locale.script = None;
        } else {
            return ENGLISH;
        }
    }
}

/// Called once after the host hydrates saved settings. Before unlock the UI
/// uses the OS default. User edits subsequently apply through `apply_preference`.
pub(crate) fn initialize_saved_preference(preference: Option<&str>) {
    ACTIVE.with_borrow_mut(|state| {
        if state.loaded {
            return;
        }
        state.locale = preference.map_or_else(|| SYSTEM.clone(), |value| resolve(Some(value)));
        state.messages.clear();
        state.system_label = None;
        state.loaded = true;
    });
}

/// Apply a user selection and invalidate translated labels on the owning UI thread.
pub(crate) fn apply_preference(preference: Option<&str>) {
    ACTIVE.with_borrow_mut(|state| {
        state.locale = preference.map_or_else(|| SYSTEM.clone(), |value| resolve(Some(value)));
        state.loaded = true;
        state.messages.clear();
        state.system_label = None;
    });
}

pub(crate) fn current_locale() -> LanguageIdentifier {
    ACTIVE.with_borrow(|state| state.locale.clone())
}

pub(super) fn preference_label(preference: Option<&str>) -> SharedString {
    if let Some(locale) = preference {
        // Only enabled values can be picked. Unknown saved preferences resolve
        // to English while retaining their value for a future upgraded app.
        return NATIVE_NAMES
            .get(locale)
            .unwrap_or(&NATIVE_NAMES["en"])
            .clone();
    }
    const KEY: &str = "settings-language-system";
    if let Some(label) = ACTIVE.with_borrow(|state| state.system_label.clone()) {
        return label;
    }
    let name = lookup(&SYSTEM, "language-name", &HashMap::new());
    let label = tr_args(KEY, [("language", name.into())]);
    ACTIVE.with_borrow_mut(|state| {
        state.system_label = Some(label.clone());
    });
    label
}

/// Explicit catalog lookup for the host's OAuth response worker. It does not
/// consult or mutate the worker's thread-local UI preference.
pub(crate) fn oauth_completion_text(locale: &str, key: &'static str) -> String {
    lookup(&resolve(Some(locale)), key, &HashMap::new())
}

pub(crate) fn tr(key: &'static str) -> SharedString {
    ACTIVE.with_borrow_mut(|state| {
        state
            .messages
            .entry(key)
            .or_insert_with(|| lookup(&state.locale, key, &HashMap::new()).into())
            .clone()
    })
}

/// Values retain their Fluent type: pass numbers as numbers for CLDR plurals.
/// Fluent's bidi isolation remains enabled for interpolated text.
pub(crate) fn tr_args<'a>(
    key: &'static str,
    args: impl IntoIterator<Item = (&'a str, FluentValue<'a>)>,
) -> SharedString {
    let args = args.into_iter().collect();
    ACTIVE.with_borrow(|state| lookup(&state.locale, key, &args).into())
}

type MessageArgs = Vec<(&'static str, FluentValue<'static>)>;
type CachedMessage = (MessageArgs, SharedString);
type ResolvedMessage = Option<(LanguageIdentifier, SharedString)>;

/// View-owned cache for parameterized labels. Values are bounded per message,
/// discarded with the view, and invalidated together on a language change.
#[derive(Default)]
pub(crate) struct MessageCache(RefCell<MessageCacheState>);

#[derive(Default)]
struct MessageCacheState {
    locale: Option<LanguageIdentifier>,
    messages: HashMap<&'static str, Vec<CachedMessage>>,
    computed: HashMap<&'static str, Vec<(String, SharedString)>>,
}

impl MessageCache {
    /// Cache derived presentation before parsing dates or other raw host fields.
    pub(crate) fn computed(
        &self,
        key: &'static str,
        input: &str,
        compute: impl FnOnce() -> SharedString,
    ) -> SharedString {
        let locale = current_locale();
        {
            let mut cache = self.0.borrow_mut();
            if cache.locale.as_ref() != Some(&locale) {
                cache.messages.clear();
                cache.computed.clear();
                cache.locale = Some(locale);
            }
            if let Some((_, text)) = cache
                .computed
                .get(key)
                .and_then(|entries| entries.iter().find(|(previous, _)| previous == input))
            {
                return text.clone();
            }
        }
        // The callback may resolve parameterized messages in this same cache.
        let text = compute();
        let mut cache = self.0.borrow_mut();
        let entries = cache.computed.entry(key).or_default();
        if entries.len() == 512 {
            entries.remove(0);
        }
        entries.push((input.to_owned(), text.clone()));
        text
    }

    pub(crate) fn get(
        &self,
        key: &'static str,
        args: impl IntoIterator<Item = (&'static str, FluentValue<'static>)>,
    ) -> SharedString {
        let args: Vec<_> = args.into_iter().collect();
        let locale = current_locale();
        let mut cache = self.0.borrow_mut();
        if cache.locale.as_ref() != Some(&locale) {
            cache.messages.clear();
            cache.computed.clear();
            cache.locale = Some(locale);
        }
        let entries = cache.messages.entry(key).or_default();
        if let Some((_, text)) = entries.iter().find(|(previous, _)| previous == &args) {
            return text.clone();
        }
        let text = tr_args(key, args.clone());
        // A page may show several rows using the same message. Bound retention
        // even when a long-lived view displays many different names or counts.
        if entries.len() == 512 {
            entries.remove(0);
        }
        entries.push((args, text.clone()));
        text
    }
}

/// A notice retains its semantic message so switching languages does not
/// leave old success or validation copy on a settings page. Host errors stay raw.
#[derive(Clone, Debug)]
pub(crate) enum UiText {
    Raw(String),
    Message {
        key: &'static str,
        args: MessageArgs,
        cached: std::rc::Rc<RefCell<ResolvedMessage>>,
    },
}
impl PartialEq for UiText {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Raw(a), Self::Raw(b)) => a == b,
            (
                Self::Message {
                    key: a, args: aa, ..
                },
                Self::Message {
                    key: b, args: ba, ..
                },
            ) => a == b && aa == ba,
            _ => false,
        }
    }
}
impl UiText {
    pub(crate) fn key(key: &'static str) -> Self {
        Self::message(key, [])
    }
    pub(crate) fn message(
        key: &'static str,
        args: impl IntoIterator<Item = (&'static str, FluentValue<'static>)>,
    ) -> Self {
        Self::Message {
            key,
            args: args.into_iter().collect(),
            cached: Default::default(),
        }
    }
    pub(crate) fn resolve(&self) -> SharedString {
        match self {
            Self::Raw(value) => value.clone().into(),
            Self::Message { key, args, cached } => {
                let locale = current_locale();
                if let Some((previous, text)) = &*cached.borrow()
                    && previous == &locale
                {
                    return text.clone();
                }
                let text = if args.is_empty() {
                    tr(key)
                } else {
                    tr_args(key, args.clone())
                };
                *cached.borrow_mut() = Some((locale, text.clone()));
                text
            }
        }
    }
}
impl From<String> for UiText {
    fn from(value: String) -> Self {
        Self::Raw(value)
    }
}
impl From<&str> for UiText {
    fn from(value: &str) -> Self {
        Self::Raw(value.to_owned())
    }
}
impl std::fmt::Display for UiText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.resolve().fmt(f)
    }
}
impl gpui::IntoElement for UiText {
    type Element = <SharedString as gpui::IntoElement>::Element;
    fn into_element(self) -> Self::Element {
        self.resolve().into_element()
    }
}

fn lookup(locale: &LanguageIdentifier, key: &str, args: &HashMap<&str, FluentValue<'_>>) -> String {
    lookup_from(&CATALOGS, locale, key, args)
}

fn lookup_from(
    catalogs: &fluent_templates::StaticLoader,
    locale: &LanguageIdentifier,
    key: &str,
    args: &HashMap<&str, FluentValue<'_>>,
) -> String {
    if let Ok(value) = catalogs.lookup_single_language(locale, key, Some(args))
        && !value.trim().is_empty()
    {
        return value;
    }
    // Do not log arguments or resolver errors: they can contain user content.
    log::warn!("Localization fallback for key {key} in locale {locale}");
    if *locale != ENGLISH
        && let Ok(value) = catalogs.lookup_single_language(&ENGLISH, key, Some(args))
        && !value.trim().is_empty()
    {
        return value;
    }
    key.to_owned()
}

#[cfg(test)]
#[path = "i18n/tests.rs"]
mod tests;
