use super::*;
use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use fluent_syntax::{ast, ast::Entry, parser, serializer};
use fluent_templates::Loader;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use syn::visit::Visit;

const ENGLISH_CATALOG: &str = include_str!("../../../locales/en/app.ftl");

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
    for locale in ["de", "de-DE", "de-AT", "de-CH", "de_DE.UTF-8", "de_DE@euro"] {
        assert_eq!(resolve(Some(locale)), langid!("de"));
    }
    for locale in ["ko", "ko-KR", "ko-KP", "ko_KR.UTF-8", "ko-Kore-KR"] {
        assert_eq!(resolve(Some(locale)), langid!("ko"));
    }
    for locale in ["es", "es-ES", "es-MX", "es_ES.UTF-8"] {
        assert_eq!(resolve(Some(locale)), langid!("es"));
    }
}

#[test]
fn spanish_preference_formats_existing_surfaces_and_initializes_once() {
    let _localization = TestLocalizationGuard::enable();
    ACTIVE.with_borrow_mut(|state| {
        state.locale = ENGLISH;
        state.loaded = false;
        state.messages.clear();
    });
    initialize_saved_preference(Some("es"));
    assert_eq!(tr("settings-language").as_ref(), "Idioma");
    assert_eq!(preference_label(Some("es")).as_ref(), "Español");
    assert_eq!(
        tr("update-confirmation-title").as_ref(),
        "¿Reiniciar Maple para actualizar?"
    );
    assert_eq!(
        tr_args(
            "update-confirmation-title-version",
            [("version", "0.0.16".into())]
        )
        .as_ref(),
        "¿Reiniciar Maple para instalar la versión 0.0.16?"
    );
    assert_eq!(
        tr_args("settings-language-system", [("language", "Español".into())]).as_ref(),
        "Sistema (Español)"
    );
    initialize_saved_preference(Some("en"));
    assert_eq!(tr("settings-language").as_ref(), "Idioma");
    ACTIVE.with_borrow_mut(|state| {
        state.locale = ENGLISH;
        state.loaded = false;
        state.messages.clear();
        state.system_label = None;
    });
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
        "System (English)"
    );
}

#[test]
fn saved_preference_is_loaded_once_and_plain_labels_are_cached() {
    let _localization = TestLocalizationGuard::enable();
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
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|part| part.ident == "text")
        {
            use syn::parse::Parser;
            let args = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(node.tokens.clone())
                .expect("parse accessible text arguments for localization checks");
            // The optional first argument assigns an element id, not visible text.
            for expr in &args {
                if let syn::Expr::Assign(assign) = expr
                    && matches!(&*assign.left, syn::Expr::Path(path) if path.path.is_ident("id"))
                {
                    continue;
                }
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(text),
                    ..
                }) = expr
                    && text.value().chars().any(char::is_alphabetic)
                {
                    self.literals.push(text.value());
                }
                self.visit_expr(expr);
            }
        } else {
            syn::visit::visit_macro(self, node);
        }
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.ident != "tests" {
            syn::visit::visit_item_mod(self, node);
        }
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        // Runtime presenters select keys in match arms as well as direct calls.
        if node.value().starts_with("runtime-") || node.value().starts_with("chat-vim-notice-") {
            self.keys.insert(node.value());
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*node.func
            && path.path.segments.last().is_some_and(|segment| {
                segment.ident == "tr"
                    || segment.ident == "tr_args"
                    || segment.ident == "localized"
                    || segment.ident == "key"
                    || segment.ident == "message"
                    || segment.ident == "context"
                    || segment.ident == "context_args"
                    || segment.ident == "with_context"
                    || segment.ident == "Local"
            })
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(key),
                ..
            })) = node.args.first()
            && (key.value().starts_with("settings-")
                || key.value().starts_with("update-")
                || key.value().starts_with("flows-")
                || key.value().starts_with("native-")
                || key.value().starts_with("chat-")
                || key.value().starts_with("runtime-"))
        {
            self.keys.insert(key.value());
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if (node.method == "get" || node.method == "context" || node.method == "with_context")
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(key),
                ..
            })) = node.args.first()
            && (key.value().starts_with("settings-")
                || key.value().starts_with("flows-")
                || key.value().starts_with("native-")
                || key.value().starts_with("chat-")
                || key.value().starts_with("runtime-"))
        {
            self.keys.insert(key.value());
        }
        if ["child", "aria_label", "tooltip", "placeholder"]
            .iter()
            .any(|name| node.method == name)
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(text),
                ..
            })) = node.args.first()
            && text.value().chars().any(char::is_alphabetic)
        {
            self.literals.push(text.value());
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

#[test]
fn accessible_text_is_checked_for_raw_labels_and_translation_keys() {
    let source = syn::parse_file(r#"
        fn view() {
            gpui::text!("Untranslated label");
            text!(id = "stable-element-id", "Another label",);
            gpui::text!(tr("settings-missing-key"));
            text!(id = ("row", 1), tr("chat-answer"));
            gpui::text!(if busy { tr("flows-login-completing") } else { tr("flows-login-complete") });
        }
    "#).unwrap();
    let mut visitor = UiStrings::default();
    visitor.visit_file(&source);
    assert_eq!(visitor.literals, ["Untranslated label", "Another label"]);
    assert_eq!(
        visitor.keys,
        [
            "settings-missing-key",
            "chat-answer",
            "flows-login-completing",
            "flows-login-complete"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    let keys = catalog_keys(include_str!("../../../locales/en/app.ftl"));
    assert!(!keys.contains("settings-missing-key"));
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
fn nested_notice_context_reformats_each_layer_and_preserves_raw_details() {
    let _locale = TestLocalizationGuard::enable();
    let semantic = UiText::context(
        "chat-attachment-failed",
        "error",
        UiText::context(
            "chat-staging-open-failed",
            "error",
            UiText::key("chat-task-cancelled"),
        ),
    );
    let raw = UiText::context(
        "chat-staging-open-failed",
        "error",
        "OS detail /tmp/raw".into(),
    );
    let cloned = semantic.clone();
    for (locale, outer, inner, cancelled) in [
        (
            "en",
            "Attachment preparation failed:",
            "Could not open the attachment:",
            "The task was cancelled",
        ),
        (
            "es",
            "No se pudo preparar el archivo adjunto:",
            "No se pudo abrir el archivo adjunto:",
            "La tarea se canceló",
        ),
        (
            "en",
            "Attachment preparation failed:",
            "Could not open the attachment:",
            "The task was cancelled",
        ),
    ] {
        apply_preference(Some(locale));
        let text = semantic.resolve();
        assert!(text.starts_with(outer));
        assert!(text.contains(inner));
        assert!(text.contains(cancelled));
        assert_eq!(text, cloned.resolve());
        assert_eq!(semantic, cloned);
        assert!(raw.resolve().starts_with(inner));
        assert!(raw.resolve().contains("OS detail /tmp/raw"));
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
    visitor.visit_file(&syn::parse_file(include_str!("../timeline_labels.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../host_errors.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/general.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/account.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/advanced.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/api_credits.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/api_keys.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/billing.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/local_api.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/mcp.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/catalog.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/acp.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/tools.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/chart.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/explorer.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/labels.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/plan.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/sections.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../settings/usage/ui.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../remote.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../login.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../plan_gate.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../store_gate.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../store_repair.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../menus.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../titlebar.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../widgets.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../text_input.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../secret_input.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../../desktop.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/dialogs.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/feedback.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/thinking.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/effort.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../effort_slider.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/project_folder.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/composer.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/sidebar.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/sidebar_computers.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/sidebar_paging.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/transcript.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/queue.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/sending.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/uploads.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/navigation.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/paging.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/attachments.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/staging.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/speech.rs")).unwrap());
    visitor.visit_file(&syn::parse_file(include_str!("../chat/computers.rs")).unwrap());
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

#[test]
fn live_switch_invalidates_plain_and_system_labels() {
    let _locale = TestLocalizationGuard::enable();
    for (locale, label, system) in [
        ("en", "Language", "System"),
        ("es", "Idioma", "Sistema"),
        ("en", "Language", "System"),
    ] {
        apply_preference(Some(locale));
        assert_eq!(tr("settings-language").as_ref(), label);
        assert!(preference_label(None).starts_with(system));
    }
}

#[test]
fn cached_messages_and_notices_follow_locale_without_translating_raw_text() {
    let _locale = TestLocalizationGuard::enable();
    let cache = MessageCache::default();
    let notice = UiText::key("settings-account-password-changed");
    let raw = UiText::from("A server-owned error".to_string());
    for (locale, singular, plural, success) in [
        (
            "en",
            "active request",
            "active requests",
            "Password changed.",
        ),
        (
            "es",
            "solicitud activa",
            "solicitudes activas",
            "Contraseña cambiada.",
        ),
        (
            "de",
            "aktive Anfrage",
            "aktive Anfragen",
            "Passwort geändert.",
        ),
        (
            "ko",
            "활성 요청 1개",
            "활성 요청 2개",
            "비밀번호를 변경했어요.",
        ),
        (
            "en",
            "active request",
            "active requests",
            "Password changed.",
        ),
    ] {
        apply_preference(Some(locale));
        assert!(
            cache
                .get("settings-local-api-requests", [("count", 1.into())])
                .ends_with(singular)
        );
        assert!(
            cache
                .get("settings-local-api-requests", [("count", 2.into())])
                .ends_with(plural)
        );
        assert_eq!(notice.resolve().as_ref(), success);
        assert_eq!(raw.resolve().as_ref(), "A server-owned error");
    }
}

#[test]
fn parameterized_catalogs_format_all_messages_without_missing_arguments() {
    // The union of argument names allows every translated pattern to be
    // exercised, including plural branches, without copying a message list.
    let english = include_str!("../../../locales/en/app.ftl");
    let spanish = include_str!("../../../locales/es/app.ftl");
    let german = include_str!("../../../locales/de/app.ftl");
    let korean = include_str!("../../../locales/ko/app.ftl");
    let names: BTreeSet<_> = [english, spanish, german, korean]
        .into_iter()
        .flat_map(|source| {
            source
                .split('$')
                .skip(1)
                .map(|tail| {
                    tail.chars()
                        .take_while(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for (locale, source) in [
        (langid!("en"), english),
        (langid!("es"), spanish),
        (langid!("de"), german),
        (langid!("ko"), korean),
    ] {
        let mut bundle = FluentBundle::new(vec![locale]);
        bundle
            .add_resource(FluentResource::try_new(source.to_string()).unwrap())
            .unwrap();
        for count in [0, 1, 2] {
            let mut args = FluentArgs::new();
            for name in &names {
                args.set(
                    name.as_str(),
                    if name == "count" {
                        FluentValue::from(count)
                    } else {
                        FluentValue::from("sample")
                    },
                );
            }
            for key in catalog_keys(source)
                .into_iter()
                .filter(|key| !key.contains('.') && !key.starts_with('-'))
            {
                let pattern = bundle.get_message(&key).unwrap().value().unwrap();
                let mut errors = Vec::new();
                let text = bundle.format_pattern(pattern, Some(&args), &mut errors);
                assert!(errors.is_empty(), "unformattable message: {key}");
                assert!(!text.trim().is_empty(), "empty message: {key}");
            }
        }
    }
}

/// String literals in production code, including those inside macro bodies.
#[derive(Default)]
struct ProductionStrings(BTreeSet<String>);

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("test")
            || (attribute.path().is_ident("cfg")
                && attribute
                    .parse_args::<syn::Ident>()
                    .is_ok_and(|predicate| predicate == "test"))
    })
}

fn macro_strings(mut cursor: syn::buffer::Cursor<'_>, strings: &mut BTreeSet<String>) {
    while !cursor.eof() {
        if let Some((inside, _, _, next)) = cursor.any_group() {
            macro_strings(inside, strings);
            cursor = next;
        } else if let Some((literal, next)) = cursor.literal() {
            if let Ok(text) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                strings.insert(text.value());
            }
            cursor = next;
        } else if let Some((_, next)) = cursor.token_tree() {
            cursor = next;
        }
    }
}

impl<'ast> Visit<'ast> for ProductionStrings {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_only(&node.attrs) {
            syn::visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !test_only(&node.attrs) {
            syn::visit::visit_item_fn(self, node);
        }
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.0.insert(node.value());
    }

    // syn does not parse macro bodies, and `text!` and `vec!` hold many keys.
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let tokens = syn::buffer::TokenBuffer::new2(node.tokens.clone());
        macro_strings(tokens.begin(), &mut self.0);
    }
}

fn visit_production_source(path: &Path, visitor: &mut ProductionStrings) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            visit_production_source(&path, visitor);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !path.to_string_lossy().ends_with("tests.rs")
        {
            visitor.visit_file(&syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap());
        }
    }
}

#[test]
fn production_strings_read_macros_and_skip_tests() {
    let source = syn::parse_file(
        r#"
        fn view() {
            tr("settings-direct");
            text!(tr("settings-in-macro"));
        }
        #[test]
        fn lookup() {
            tr("settings-test-function");
        }
        #[cfg(test)]
        mod checks {
            fn helper() {
                tr("settings-test-module");
            }
        }
        "#,
    )
    .unwrap();
    let mut strings = ProductionStrings::default();
    strings.visit_file(&source);
    assert_eq!(
        strings.0,
        ["settings-direct", "settings-in-macro"]
            .map(String::from)
            .into()
    );
}

// Lookups take `&'static str`, so a key built at runtime cannot reach the
// catalog and every used message appears as a literal somewhere in the source.
#[test]
fn every_source_message_is_used_by_production_code() {
    let mut strings = ProductionStrings::default();
    visit_production_source(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut strings,
    );
    let unused: Vec<_> = catalog_keys(ENGLISH_CATALOG)
        .into_iter()
        .filter(|key| !strings.0.contains(key))
        .collect();
    assert!(unused.is_empty(), "unused catalog messages: {unused:?}");
}

fn inline_variables<'s>(
    expression: &ast::InlineExpression<&'s str>,
    variables: &mut BTreeSet<&'s str>,
) {
    match expression {
        ast::InlineExpression::VariableReference { id } => {
            variables.insert(id.name);
        }
        ast::InlineExpression::Placeable { expression } => {
            expression_variables(expression, variables);
        }
        _ => {}
    }
}

fn expression_variables<'s>(
    expression: &ast::Expression<&'s str>,
    variables: &mut BTreeSet<&'s str>,
) {
    match expression {
        ast::Expression::Inline(inline) => inline_variables(inline, variables),
        ast::Expression::Select { selector, variants } => {
            inline_variables(selector, variables);
            for variant in variants {
                pattern_variables(&variant.value, variables);
            }
        }
    }
}

fn pattern_variables<'s>(pattern: &ast::Pattern<&'s str>, variables: &mut BTreeSet<&'s str>) {
    for element in &pattern.elements {
        if let ast::PatternElement::Placeable { expression } = element {
            expression_variables(expression, variables);
        }
    }
}

fn names_variable(comment: &str, variable: &str) -> bool {
    let name = format!("${variable}");
    comment.match_indices(&name).any(|(start, _)| {
        !comment[start + name.len()..]
            .starts_with(|next: char| next.is_alphanumeric() || matches!(next, '-' | '_'))
    })
}

#[test]
fn source_messages_explain_their_variables_to_translators() {
    let resource = parser::parse(ENGLISH_CATALOG).expect("valid Fluent syntax");
    let mut undocumented = Vec::new();
    for entry in &resource.body {
        let Entry::Message(message) = entry else {
            continue;
        };
        let mut variables = BTreeSet::new();
        if let Some(value) = &message.value {
            pattern_variables(value, &mut variables);
        }
        let comment = message
            .comment
            .as_ref()
            .map(|comment| comment.content.join(" "))
            .unwrap_or_default();
        for variable in variables {
            let positional = variable
                .strip_prefix("arg")
                .is_some_and(|rest| rest.chars().all(|ch| ch.is_ascii_digit()));
            if positional || !names_variable(&comment, variable) {
                undocumented.push(format!("{}: ${variable}", message.id.name));
            }
        }
    }
    assert!(
        undocumented.is_empty(),
        "give these variables descriptive names and a translator comment: {undocumented:?}"
    );
}

const REVISIONS_HEADER: &str = "# English each translation was last reviewed against. Written by `just translations-reviewed`.";

fn revisions_path(locale: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("translation-revisions")
        .join(format!("{locale}.txt"))
}

fn fingerprint(text: &str) -> u64 {
    // FNV-1a keeps these stable across Rust releases, unlike `DefaultHasher`.
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// One line per English message. Comments are left out, so improving a
/// translator note never marks its translations out of date.
fn translation_revisions(source: &str) -> String {
    // Windows checkouts can turn the catalog's line endings into CRLF.
    let source = source.replace("\r\n", "\n");
    let resource = parser::parse(source.as_str()).expect("valid Fluent syntax");
    let mut lines: Vec<_> = resource
        .body
        .into_iter()
        .filter_map(|entry| {
            let (key, entry) = match entry {
                Entry::Message(message) => (
                    message.id.name.to_owned(),
                    Entry::Message(ast::Message {
                        comment: None,
                        ..message
                    }),
                ),
                Entry::Term(term) => (
                    format!("-{}", term.id.name),
                    Entry::Term(ast::Term {
                        comment: None,
                        ..term
                    }),
                ),
                _ => return None,
            };
            let message = serializer::serialize(&ast::Resource { body: vec![entry] });
            Some(format!("{key} {:016x}", fingerprint(&message)))
        })
        .collect();
    lines.sort();
    format!("{REVISIONS_HEADER}\n{}\n", lines.join("\n"))
}

/// Messages added, removed or reworded since the recorded review.
fn stale_translations(source: &str, recorded: &str) -> BTreeSet<String> {
    let lines = |text: &str| -> BTreeSet<String> {
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .map(str::to_owned)
            .collect()
    };
    lines(&translation_revisions(source))
        .symmetric_difference(&lines(recorded))
        .filter_map(|line| line.split(' ').next().map(str::to_owned))
        .collect()
}

#[test]
fn translations_were_reviewed_against_the_current_english() {
    for locale in LANGUAGES.iter().flatten().filter(|locale| **locale != "en") {
        let recorded = std::fs::read_to_string(revisions_path(locale)).unwrap_or_default();
        let stale = stale_translations(ENGLISH_CATALOG, &recorded);
        assert!(
            stale.is_empty(),
            "English changed for {stale:?}. Update these in app/locales/{locale}/app.ftl, \
             then run `just translations-reviewed {locale}`."
        );
    }
}

#[test]
fn translation_revisions_follow_english_text_only() {
    let reviewed = translation_revisions("kept = Same\n# Note\nchanged = Old\nremoved = Gone\n");
    assert_eq!(
        stale_translations(
            "# A better note\nkept = Same\nchanged = New\nadded = Fresh\n",
            &reviewed
        ),
        ["added", "changed", "removed"].map(String::from).into()
    );

    let plural =
        "files = { $count ->\n    [one] { $count } file\n   *[other] { $count } files\n}\n";
    let reviewed = translation_revisions(plural);
    assert_eq!(
        stale_translations(
            &plural.replace("[one] { $count } file", "[one] One file"),
            &reviewed
        ),
        ["files"].map(String::from).into()
    );
    assert!(stale_translations(&plural.replace('\n', "\r\n"), &reviewed).is_empty());
}

#[test]
#[ignore = "rewrites a revision file; run it through `just translations-reviewed`"]
fn record_reviewed_translations() {
    let locale = std::env::var("MAPLE_TRANSLATION_LOCALE").expect("locale to record");
    assert!(
        locale != "en" && LANGUAGES.contains(&Some(locale.as_str())),
        "{locale} is not an enabled translation"
    );
    std::fs::write(
        revisions_path(&locale),
        translation_revisions(ENGLISH_CATALOG),
    )
    .unwrap();
}

#[test]
fn timeline_semantics_reformat_without_changing_content() {
    use crate::ui::timeline_labels;
    let _locale = TestLocalizationGuard::enable();
    let item: maple_agent::agent::AgentTimelineItem = serde_json::from_value(serde_json::json!({
        "id": "tool", "itemType": "tool", "createdMs": 1, "merge": "replace",
        "title": "Terminal: echo hola", "text": "user content",
        "label": {"kind": "tool", "name": "read_file", "detail": "original.txt"}
    }))
    .unwrap();
    let cache = MessageCache::default();
    for (locale, expected) in [
        ("en", "Read file"),
        ("es", "Leer archivo"),
        ("en", "Read file"),
    ] {
        apply_preference(Some(locale));
        assert_eq!(
            timeline_labels::tool_label(&item, &cache).unwrap().as_ref(),
            expected
        );
        let title = timeline_labels::title(&item, &cache).unwrap();
        assert!(!title.contains(['\u{2068}', '\u{2069}']));
        assert!(title.contains(expected));
        assert!(title.contains("original.txt"));
        assert!(timeline_labels::body(&item, &cache).is_none());
        assert_eq!(item.text.as_deref(), Some("user content"));
    }
    let mut unknown = item.clone();
    unknown.label.as_mut().unwrap().kind = "future-kind".into();
    assert!(timeline_labels::title(&unknown, &cache).is_none());
    unknown.label = None;
    assert!(timeline_labels::title(&unknown, &cache).is_none());
}

#[test]
fn typed_host_failure_switches_locale_and_preserves_raw_diagnostic() {
    use crate::{backend::HostFailure, ui::host_errors::failure_text};
    let _locale = TestLocalizationGuard::enable();
    let failure = HostFailure::new("store-locked", "raw diagnostic");
    let notice = failure_text(&failure);
    apply_preference(Some("en"));
    assert!(notice.resolve().contains("Maple is locked"));
    apply_preference(Some("es"));
    assert!(notice.resolve().contains("Maple está bloqueado"));
    assert_eq!(failure.message, "raw diagnostic");
    for code in ["unknown", "future-code"] {
        assert_eq!(
            failure_text(&HostFailure::new(code, "untouched content"))
                .resolve()
                .as_ref(),
            "untouched content"
        );
    }
}

#[test]
fn builtin_tool_title_keeps_original_english_detail_and_raw_identifiers() {
    use crate::ui::timeline_labels;
    let _locale = TestLocalizationGuard::enable();
    let item: maple_agent::agent::AgentTimelineItem = serde_json::from_value(serde_json::json!({
        "id": "tool", "itemType": "tool", "createdMs": 1, "merge": "replace",
        "title": "read: original.txt", "label": {"kind": "tool", "name": "read", "detail": "original.txt"}
    })).unwrap();
    let cache = MessageCache::default();
    assert!(timeline_labels::tool_label(&item, &cache).is_none());
    assert_eq!(
        timeline_labels::title(&item, &cache).unwrap().as_ref(),
        "read: original.txt"
    );
    apply_preference(Some("es"));
    assert_eq!(
        timeline_labels::title(&item, &cache).unwrap().as_ref(),
        "leer: original.txt"
    );
    assert_eq!(item.label.as_ref().unwrap().name.as_deref(), Some("read"));
}

#[test]
fn retained_runtime_notices_switch_locale_and_keep_legacy_fallback() {
    use crate::ui::store_gate::StoreOperationNotice;
    let _locale = TestLocalizationGuard::enable();
    let notice: StoreOperationNotice = serde_json::from_value(serde_json::json!({
        "operation_id":"same-operation", "message":"original fallback",
        "label":"change_store_password", "pending":true
    }))
    .unwrap();
    let presentation = notice.presentation();
    for (locale, expected) in [
        ("en", "Your unlock password changed on this computer."),
        (
            "es",
            "Tu contraseña de desbloqueo cambió en esta computadora.",
        ),
        ("en", "Your unlock password changed on this computer."),
    ] {
        apply_preference(Some(locale));
        assert!(presentation.resolve().starts_with(expected));
        assert_eq!(notice.operation_id, "same-operation");
        assert_eq!(notice.message, "original fallback");
    }
    for label in [serde_json::Value::Null, serde_json::json!("future-notice")] {
        let legacy: StoreOperationNotice = serde_json::from_value(serde_json::json!({
            "operation_id":"legacy", "message":"opaque old notice", "label":label
        }))
        .unwrap();
        assert_eq!(
            legacy.presentation().resolve().as_ref(),
            "opaque old notice"
        );
    }
}

#[test]
fn runtime_usage_body_and_error_headings_follow_locale_without_mutating_diagnostics() {
    use crate::ui::timeline_labels;
    let _locale = TestLocalizationGuard::enable();
    let mut item: maple_agent::agent::AgentTimelineItem =
        serde_json::from_value(serde_json::json!({
            "id":"error", "itemType":"error", "createdMs":1, "merge":"replace",
            "text":"opaque retained diagnostic", "label":{"kind":"error-usage"}
        }))
        .unwrap();
    let cache = MessageCache::default();
    for (locale, title, body) in [
        (
            "en",
            "Out of usage",
            "You've run out of usage. See your options in Billing so you can keep working.",
        ),
        (
            "es",
            "Uso agotado",
            "Se agotó tu uso disponible. Consulta tus opciones en Facturación para seguir trabajando.",
        ),
        (
            "en",
            "Out of usage",
            "You've run out of usage. See your options in Billing so you can keep working.",
        ),
    ] {
        apply_preference(Some(locale));
        assert_eq!(
            timeline_labels::title(&item, &cache).unwrap().as_ref(),
            title
        );
        assert_eq!(timeline_labels::body(&item, &cache).unwrap().as_ref(), body);
        assert_eq!(item.text.as_deref(), Some("opaque retained diagnostic"));
    }
    item.label.as_mut().unwrap().kind = "future-error".into();
    assert!(timeline_labels::body(&item, &cache).is_none());
    assert!(timeline_labels::title(&item, &cache).is_none());
}

#[test]
fn runtime_auth_sources_keep_distinct_english_and_localize() {
    use crate::{backend::HostFailure, ui::host_errors::failure_text};
    let _locale = TestLocalizationGuard::enable();
    for (code, english, spanish) in [
        (
            "api-authentication",
            "Maple API authentication failed",
            "Falló la autenticación con la API de Maple",
        ),
        (
            "account-auth-rejected",
            "Maple API authentication was rejected",
            "La API de Maple rechazó la autenticación",
        ),
        (
            "billing-token-rejected",
            "billing API rejected the session token",
            "la API de facturación rechazó el token de sesión",
        ),
    ] {
        let text = failure_text(&HostFailure::new(code, english));
        apply_preference(Some("en"));
        assert_eq!(text.resolve().as_ref(), english);
        apply_preference(Some("es"));
        assert_eq!(text.resolve().as_ref(), spanish);
    }
}

#[test]
fn runtime_memory_requirement_and_password_retention_survive_locale_switch() {
    use crate::{backend::HostFailure, ui::host_errors::failure_text};
    let _locale = TestLocalizationGuard::enable();
    let failure =
        HostFailure::from(maple_agent::store::keys::KeyError::NotEnoughMemory { required_mib: 64 });
    let text = failure_text(&failure);
    for locale in ["en", "es", "en"] {
        apply_preference(Some(locale));
        assert!(text.resolve().contains("64"));
        assert!(text.resolve().contains("MiB"));
    }
    let sync = HostFailure::from(maple_agent::store::keys::KeyError::SynchronizationPending {
        kind: std::io::ErrorKind::Other,
    });
    apply_preference(Some("en"));
    assert!(
        failure_text(&sync)
            .resolve()
            .contains("Keep both passwords")
    );
    apply_preference(Some("es"));
    assert!(
        failure_text(&sync)
            .resolve()
            .contains("Conserva ambas contraseñas")
    );
}

#[test]
fn nested_context_keeps_outer_arguments_and_inner_wins_over_duplicate() {
    let _locale = TestLocalizationGuard::enable();
    let notice = UiText::context_args(
        "chat-project-integration-save-failed",
        [
            ("name", "Raw MCP".into()),
            ("project", "/raw/project".into()),
            ("error", "must not win".into()),
        ],
        "error",
        UiText::key("chat-task-cancelled"),
    );
    for (locale, inner) in [
        ("en", "The task was cancelled"),
        ("es", "La tarea se canceló"),
        ("en", "The task was cancelled"),
    ] {
        apply_preference(Some(locale));
        let text = notice.resolve();
        assert!(text.contains("Raw MCP"));
        assert!(text.contains("/raw/project"));
        assert!(text.contains(inner));
        assert!(!text.contains("must not win"));
    }
}

#[test]
fn cleared_store_returns_to_system_then_hydrates_next_store() {
    let _guard = TestLocalizationGuard::enable();
    apply_preference(Some("es"));
    clear_saved_preference();
    assert_eq!(current_locale(), ENGLISH);
    initialize_saved_preference(Some("es"));
    assert_eq!(current_locale(), langid!("es"));
}

#[test]
fn malformed_timeline_presentation_keeps_raw_row_and_authority() {
    for label in [
        serde_json::json!({}),
        serde_json::json!({"kind":42}),
        serde_json::json!({"kind":"tool","detail":[]}),
        serde_json::json!("future"),
    ] {
        let item: maple_agent::agent::AgentTimelineItem =
            serde_json::from_value(serde_json::json!({
                "id":"raw-id", "itemType":"error", "text":"original diagnostic", "createdMs":1,
                "merge":"replace", "label":label,
                "persisted":{"taskId":"task", "rowsEpoch":2, "rowSeq":3, "updateSeq":4}
            }))
            .unwrap();
        assert!(item.label.is_none());
        assert_eq!(item.text.as_deref(), Some("original diagnostic"));
        assert_eq!(item.persisted.unwrap().row_seq, 3);
    }
}

#[test]
fn provider_body_switches_language_without_changing_raw_retry_text() {
    let _guard = TestLocalizationGuard::enable();
    let item: maple_agent::agent::AgentTimelineItem = serde_json::from_value(serde_json::json!({
        "id":"error", "itemType":"error", "text":"Network error: could not connect to Maple",
        "createdMs":1,"merge":"replace", "label":{"kind":"error-agent","name":"connect"}
    }))
    .unwrap();
    let cache = MessageCache::default();
    for (locale, expected) in [
        ("en", "Network error: could not connect to Maple"),
        ("es", "Error de red: no se pudo conectar con Maple"),
        ("en", "Network error: could not connect to Maple"),
    ] {
        apply_preference(Some(locale));
        assert_eq!(
            crate::ui::timeline_labels::body(&item, &cache)
                .unwrap()
                .as_ref(),
            expected
        );
        assert_eq!(
            item.text.as_deref(),
            Some("Network error: could not connect to Maple")
        );
    }
}

#[test]
fn future_error_kinds_do_not_inherit_known_provider_copy() {
    let item: maple_agent::agent::AgentTimelineItem = serde_json::from_value(serde_json::json!({
        "id":"error", "itemType":"error", "text":"future diagnostic", "createdMs":1,
        "merge":"replace", "label":{"kind":"error-future","name":"connect"}
    }))
    .unwrap();
    assert!(crate::ui::timeline_labels::body(&item, &MessageCache::default()).is_none());
}

#[test]
fn german_saved_preference_formats_native_labels_and_arguments() {
    let _locale = TestLocalizationGuard::enable();
    initialize_saved_preference(Some("de-AT"));
    assert_eq!(current_locale(), langid!("de"));
    assert_eq!(preference_label(Some("de")).as_ref(), "Deutsch");
    assert_eq!(tr("settings-language").as_ref(), "Sprache");
    let title = tr_args(
        "update-confirmation-title-version",
        [("version", "1.2.3".into())],
    );
    assert_eq!(
        title.as_ref(),
        "Maple neu starten, um v1.2.3 zu installieren?"
    );
    assert!(!title.contains(['\u{2068}', '\u{2069}']));
    assert_eq!(
        tr_args(
            "settings-date",
            [
                ("day", "9".into()),
                ("month", "März".into()),
                ("year", "2026".into())
            ]
        )
        .as_ref(),
        "9. März 2026"
    );
    initialize_saved_preference(Some("en"));
    assert_eq!(current_locale(), langid!("de"));
    apply_preference(Some("en"));
    assert_eq!(tr("settings-language").as_ref(), "Language");
}

#[test]
fn korean_saved_preference_formats_native_labels_and_arguments() {
    let _locale = TestLocalizationGuard::enable();
    initialize_saved_preference(Some("ko-KR"));
    assert_eq!(current_locale(), langid!("ko"));
    assert_eq!(preference_label(Some("ko")).as_ref(), "한국어");
    assert_eq!(tr("settings-language").as_ref(), "언어");
    let title = tr_args(
        "update-confirmation-title-version",
        [("version", "1.2.3".into())],
    );
    assert_eq!(
        title.as_ref(),
        "v1.2.3 설치를 위해 Maple을 다시 시작할까요?"
    );
    assert!(!title.contains(['\u{2068}', '\u{2069}']));
    assert_eq!(
        tr_args(
            "settings-date",
            [
                ("day", "9".into()),
                ("month", tr("settings-month-march").to_string().into()),
                ("year", "2026".into())
            ]
        )
        .as_ref(),
        "2026년 3월 9일"
    );
    // Korean has one plural category, so a count never changes the wording.
    for count in [1, 2] {
        assert_eq!(
            tr_args("settings-local-api-requests", [("count", count.into())]).as_ref(),
            format!("활성 요청 {count}개")
        );
    }
    initialize_saved_preference(Some("en"));
    assert_eq!(current_locale(), langid!("ko"));
    apply_preference(Some("en"));
    assert_eq!(tr("settings-language").as_ref(), "Language");
}
