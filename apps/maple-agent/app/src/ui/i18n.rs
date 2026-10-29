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
/// Enabled catalogs. None is the persisted system preference. Add a language
/// here only when its catalog has passed the completeness checks.
pub(super) const LANGUAGES: &[Option<&str>] = &[None, Some("en")];
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
/// uses the OS default. Later edits deliberately take effect on next launch:
/// cached panels, precomputed sections and native menus have separate lifecycles.
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
