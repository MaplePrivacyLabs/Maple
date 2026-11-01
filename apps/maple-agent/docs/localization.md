# Agent localization

The localization foundation uses embedded Fluent catalogs.
It currently covers the update confirmation dialog (including accessibility
text) and the language setting. English is the only enabled catalog. Most of
the app is still English; this is infrastructure for incremental extraction,
not a claim of complete localization.

## Locale and lifetime

Settings → General → Language offers System and English. The optional `locale`
preference uses the existing host-synced UI settings row. Old or malformed
preferences use System; unknown language tags resolve to English without
preventing startup or overwriting the saved value.

The OS locale is read once, normalized from platform/POSIX spelling, and matched
against enabled catalogs, first exactly, then without variants, region, and
script. English is the final default. Before the store is unlocked, the UI uses
the OS default. The first loaded host settings snapshot selects the process
locale. Language edits apply after **quitting and reopening Maple**; closing
and reopening Settings is not enough. This avoids incomplete live updates of
cached GPUI panels, precomputed sidebar sections, and native menus. No per-frame
or language-change window refresh is introduced. Unit/GPUI tests default to
English regardless of the executing machine's locale.

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
metadata before enablement. The current macOS bundle declares English only;
additional languages require owned packaging changes and evidence.

## Explicit follow-ups

Extraction is substantially larger than just `child(...)` calls: shortcut names
and categories, desktop code, backend/host/remote errors, and other non-UI files
also contain display text. Work module by module, starting with Settings/login.

- Host/ACP/backend errors remain English strings. Localizing them needs typed
  semantic errors plus arguments across the process boundary, formatted by the
  consuming UI; it is not a reason to translate shared host strings in place.
- Persisted timeline labels remain unchanged. Their future migration requires
  semantic kinds, a `TIMELINE_FORMAT` bump and plain/keyed projection tests.
- Model-facing tool descriptions, system prompts, notices sent to models, user
  content and generated content are never translated by UI extraction.
- Native/tray menus, OS dialogs and the standalone OAuth completion HTML are
  outside this first milestone. Each has its own lifecycle/packaging contract.
- Dates, localized numeric separators and money need a separate formatting
  decision; Fluent plural selection does not provide complete locale formatting.
- Spell checking remains `en_US`; UI language and content language are separate.
- RTL layout/mirroring is not supported by this implementation. Do not enable
  RTL locales until the GPUI/platform limitation is addressed. CJK and other
  scripts require font-fallback testing on every supported desktop platform.
