use super::*;
use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use fluent_syntax::{ast::Entry, parser};
use fluent_templates::Loader;
use std::{collections::BTreeSet, path::Path};
use syn::visit::Visit;

#[test]
fn locale_resolution_accepts_os_spellings_and_falls_back_safely() {
    for locale in [
        None,
        Some("en"),
        Some("en-US"),
        Some("en_GB.UTF-8"),
        Some("en_US@variant"),
        Some("C"),
        Some("fr-FR"),
        Some("../../en"),
        Some(""),
    ] {
        assert_eq!(resolve(locale), ENGLISH);
    }
}

#[test]
fn resolution_prefers_exact_catalogs_then_language_without_losing_script_first() {
    let enabled = [
        None,
        Some("en"),
        Some("fr"),
        Some("fr-CA"),
        Some("zh"),
        Some("zh-Hant"),
    ];
    for (requested, expected) in [
        ("fr-CA", "fr-CA"),
        ("fr_FR.UTF-8", "fr"),
        ("zh-Hant-TW", "zh-Hant"),
        ("zh-Hans-CN", "zh"),
        ("de-DE", "en"),
    ] {
        assert_eq!(
            resolve_enabled(Some(requested), &enabled).to_string(),
            expected
        );
    }
}

#[test]
fn missing_keys_and_locales_fall_back_without_blank_text() {
    let args = HashMap::new();
    assert_eq!(
        lookup(&langid!("fr"), "update-confirmation-later", &args),
        "Later"
    );
    assert_eq!(lookup(&ENGLISH, "missing-key", &args), "missing-key");
    // A missing variable must not leak Fluent's error placeholder into UI.
    assert_eq!(
        lookup(&ENGLISH, "settings-language-system", &args),
        "settings-language-system"
    );
    assert_eq!(
        tr_args("settings-language-system", [("language", "English".into())]).as_ref(),
        "System (\u{2068}English\u{2069})"
    );
}

#[test]
fn saved_preference_is_loaded_once_and_plain_labels_are_cached() {
    ACTIVE.with_borrow_mut(|state| {
        state.locale = langid!("fr");
        state.loaded = false;
        state.messages.clear();
    });
    initialize_saved_preference(Some("en-US"));
    assert_eq!(tr("update-confirmation-later").as_ref(), "Later");
    ACTIVE.with_borrow_mut(|state| {
        assert_eq!(state.locale, ENGLISH);
        assert_eq!(state.messages.len(), 1);
        // A cached label remains usable even if a catalog cannot be queried.
        state
            .messages
            .insert("update-confirmation-later", "cached".into());
    });
    ACTIVE.with_borrow_mut(|state| state.locale = langid!("fr"));
    initialize_saved_preference(Some("en"));
    ACTIVE.with_borrow(|state| assert_eq!(state.locale, langid!("fr")));
    assert_eq!(tr("update-confirmation-later").as_ref(), "cached");
    ACTIVE.with_borrow_mut(|state| {
        state.messages.clear();
        state.locale = ENGLISH;
    });
}

fn catalog_keys(source: &str) -> BTreeSet<String> {
    let resource = parser::parse(source).expect("valid Fluent syntax");
    let mut keys = BTreeSet::new();
    for entry in resource.body {
        match entry {
            Entry::Message(message) => {
                assert!(
                    message.value.is_some(),
                    "message {} must have a value",
                    message.id.name
                );
                assert!(keys.insert(message.id.name.to_owned()), "duplicate message");
                for attribute in message.attributes {
                    assert!(keys.insert(format!("{}.{}", message.id.name, attribute.id.name)));
                }
            }
            Entry::Term(term) => {
                assert!(keys.insert(format!("-{}", term.id.name)), "duplicate term");
            }
            _ => {}
        }
    }
    keys
}

#[test]
fn shipped_catalogs_parse_and_have_identical_keys() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let english = catalog_keys(&std::fs::read_to_string(root.join("en/app.ftl")).unwrap());
    let mut locales = BTreeSet::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.file_type().unwrap().is_dir(),
            "catalogs live in locale/app.ftl"
        );
        let locale = entry.file_name().into_string().unwrap();
        locale.parse::<LanguageIdentifier>().unwrap();
        assert_eq!(
            std::fs::read_dir(entry.path()).unwrap().count(),
            1,
            "one app.ftl per locale"
        );
        let source = std::fs::read_to_string(entry.path().join("app.ftl")).unwrap();
        assert_eq!(catalog_keys(&source), english, "catalog {locale}");
        locales.insert(locale);
    }
    assert_eq!(
        locales,
        LANGUAGES
            .iter()
            .flatten()
            .map(|locale| locale.to_string())
            .collect()
    );
    assert_eq!(
        locales,
        CATALOGS.locales().map(ToString::to_string).collect()
    );
}

#[test]
fn numeric_arguments_use_locale_plural_rules() {
    let mut bundle = FluentBundle::new(vec![langid!("ru")]);
    bundle
        .add_resource(
            FluentResource::try_new(
                "count = { $count ->\n [one] one\n [few] few\n [many] many\n *[other] other\n}"
                    .to_owned(),
            )
            .unwrap(),
        )
        .unwrap();
    let pattern = bundle.get_message("count").unwrap().value().unwrap();
    for (count, expected) in [(1, "one"), (2, "few"), (5, "many"), (21, "one")] {
        let mut args = FluentArgs::new();
        args.set("count", count);
        let mut errors = Vec::new();
        assert_eq!(
            bundle.format_pattern(pattern, Some(&args), &mut errors),
            expected
        );
        assert!(errors.is_empty());
    }
}

#[derive(Default)]
struct UiStrings {
    keys: BTreeSet<String>,
    literals: Vec<String>,
}

impl<'ast> Visit<'ast> for UiStrings {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.ident != "tests" {
            syn::visit::visit_item_mod(self, node);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*node.func
            && path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "tr" || segment.ident == "tr_args")
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(key),
                ..
            })) = node.args.first()
        {
            self.keys.insert(key.value());
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if ["child", "aria_label", "tooltip", "placeholder"]
            .iter()
            .any(|name| node.method == name)
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(text),
                ..
            })) = node.args.first()
        {
            self.literals.push(text.value());
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn visit_ui(path: &Path, visitor: &mut UiStrings) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            visit_ui(&path, visitor);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !path.ends_with("i18n/tests.rs")
        {
            visitor.visit_file(&syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap());
        }
    }
}

#[test]
fn ui_lookup_keys_exist_in_source_catalog() {
    let mut visitor = UiStrings::default();
    visit_ui(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui"),
        &mut visitor,
    );
    let keys = catalog_keys(include_str!("../../../locales/en/app.ftl"));
    assert!(!visitor.keys.is_empty());
    for key in visitor.keys {
        assert!(keys.contains(&key), "missing source message: {key}");
    }
}

#[test]
fn converted_modules_do_not_reintroduce_inline_labels() {
    // Grow this list with each extraction. Unconverted modules are explicitly
    // outside the gate rather than freezing hundreds of existing literals.
    let mut visitor = UiStrings::default();
    visitor.visit_file(&syn::parse_file(include_str!("../update_confirmation.rs")).unwrap());
    assert!(
        visitor.literals.is_empty(),
        "inline UI text: {:?}",
        visitor.literals
    );
}

fluent_templates::static_loader! {
    static PARTIAL = {
        locales: "./tests/fixtures/i18n",
        fallback_language: "en",
    };
}

#[test]
fn partial_catalog_falls_back_per_message_including_format_errors() {
    for (key, expected) in [
        ("present", "Français"),
        ("missing", "English fallback"),
        ("invalid", "Safe fallback"),
        ("empty", "Nonempty fallback"),
        ("unknown", "unknown"),
    ] {
        assert_eq!(
            lookup_from(&PARTIAL, &langid!("fr"), key, &HashMap::new()),
            expected
        );
    }
}
