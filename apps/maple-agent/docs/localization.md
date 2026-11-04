# Agent localization

The localization foundation uses embedded Fluent catalogs.
It covers all Settings pages, their local password/encryption forms, shortcut names
and search, the unified API page, editor setup, remote-computer controls, and the
update confirmation dialog. General
includes dropdown choices, the chat preview and test notification. English and
Spanish are enabled.
Most of the app is still English; this is infrastructure for incremental
extraction, not a claim of complete localization.

## Locale and live switching

Localization is enabled by default. Settings → General → Language offers System,
English and Español. Changes are saved through the host and apply immediately;
no environment variable or restart is needed.

The OS locale supplies the initial default. The first host settings snapshot
loads the saved preference. A user selection clears the plain-label and System
label caches and refreshes GPUI windows. General settings resolves labels during
render; an open update confirmation reformats its versioned title only when its
locale changes. Settings also rebuilds its filtered shortcut list and remote
status projection. Catalog-backed input placeholders and accessibility labels
follow the current locale without replacing typed text. Saved control values,
confirmation words (`DELETE` and `ERASE`), commands and element IDs remain stable.
Editor setup translates the instructions while preserving generated configuration
snippets and the names of controls in external editors. The Local API card retains
semantic status text so cached host snapshots also follow language changes.

Locale state currently belongs to the GPUI thread. Call presentation helpers on
that thread; background workers should return semantic data for the UI to format.
Tests use isolated thread-local state and default to English. Pages outside Settings and
native/tray menus remain English until extracted. Future translated cached views
must invalidate formatted text on locale changes; refreshing windows alone does
not rebuild their stored strings.

## Adding UI text

Catalogs live in `app/locales/{locale}/app.ftl`. `fluent-templates` embeds them
at compile time; there is no runtime catalog directory or disk access. The
existing Nix fileset includes `app/`; `build.rs` tracks the catalog directory,
including additions/removals. `fluent-bundle` supplies typed argument values.

- Use stable module-prefixed message IDs, such as `update-confirmation-title`.
- Use `ui::i18n::tr("message-id")` for plain labels. Each resolved label is cached
  as a `SharedString` for the process locale; rendering only clones that handle.
- Use `tr_args("message-id", [("count", count.into())])` for interpolation.
  Format these when the input state changes and retain the result in the view;
  do not format dynamic messages on every render. Pass numbers as numbers so
  Fluent can select CLDR plural branches. The System language label is cached.
- Settings uses view-owned `MessageCache` instances for parameterized render
  labels. Each message retains at most 512 argument sets; a locale change clears
  the cache, and dropping the view releases it. `UiText` retains semantic keys
  and arguments for notices, resolving them again after a language change. Raw
  host errors are explicitly separate from these localized notices.
- Translate accessibility labels too. Keep element IDs, keymap slots, commands,
  protocol values and other machine-readable identifiers stable.
- Keep complete sentences together. Add translator comments explaining args.
  Fluent bidi isolation (U+2068/U+2069) remains enabled; assertions must account
  for it. GPUI shaping/fallback-font checks are required before enabling scripts
  that need them.

Lookup tries the selected catalog, then English, then the key itself. Empty or
unformattable values also fall back. Logs contain only the key and catalog, not
arguments or resolver errors that might contain user text. Cached plain labels
log a fallback once per first resolution rather than on every frame.

## Validation and incremental extraction

The existing `agent-test` cargo suite checks catalog syntax, duplicate IDs,
identical shipped key sets, enabled/catalog parity, literal UI lookup keys,
fallback behavior, argument isolation and numeric plurals. The converted-module
check rejects direct literals in common label methods; expand its module list
as extraction proceeds. It is deliberately a narrow heuristic, not proof that
all user-facing text has been found. Existing unconverted modules remain debt;
new user-facing text should use the catalog rather than increasing that debt.
No new workflow or CI lane is needed.

A partial catalog can fall back safely at runtime, but it is not enabled or
shipped until completeness checks pass. To add a language, add its complete
catalog and native `language-name`, then register it in `i18n::LANGUAGES`.
Validate layout, font fallback, bidi marks, and platform-native language
metadata before production enablement. Spanish currently has Linux preview
evidence only; macOS/Windows runtime and Spanish native language metadata remain
follow-ups. The current macOS bundle declares English only, so native OS dialogs
are outside this preview's Spanish coverage.

## Explicit follow-ups

Extraction is substantially larger than just `child(...)` calls. Settings now
includes an adapter for the built-in shortcut catalog, with coverage tests for
new catalog entries. Desktop code, login, backend/host errors and other non-UI
files still contain display text.

- Host/ACP/backend errors and host-owned integration detection details remain
  English strings. Typed remote update outcomes and credit validation errors
  are formatted in the UI. Localizing the remaining host strings needs typed
  semantic errors plus arguments across the process boundary, formatted by the
  consuming UI; it is not a reason to translate shared host strings in place.
- Persisted timeline labels remain unchanged. Their future migration requires
  semantic kinds, a `TIMELINE_FORMAT` bump and plain/keyed projection tests.
- Model-facing tool descriptions, system prompts, notices sent to models, user
  content and generated content are never translated by UI extraction.
- Native/tray menus, OS dialogs and the standalone OAuth completion HTML are
  outside this first milestone. Each has its own lifecycle/packaging contract.
- Settings dates use translated month names and date patterns. The plan-usage
  DTO includes optional raw product/reset inputs so the UI can format them;
  older hosts remain readable. Numeric separators and money still use the
  existing application formats; Fluent plural selection does not provide
  complete locale formatting.
- Spell checking remains `en_US`; UI language and content language are separate.
- RTL layout/mirroring is not supported by this implementation. Do not enable
  RTL locales until the GPUI/platform limitation is addressed. CJK and other
  scripts require font-fallback testing on every supported desktop platform.
