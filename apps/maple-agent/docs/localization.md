# Agent localization

The app uses embedded Fluent catalogs for Settings, sign-in, plan and local-store
flows, recovery, chat, the composer and sidebar, project dialogs, and native
menus, tray items and desktop prompts. The integration app catalog and ACP
setup instructions are included. Catalog search follows translated summaries and
categories; generated client configuration and external app menu names stay exact.
English is the source of truth. The `es`
catalog targets Mexican Spanish; it does not register a separate `es-MX` catalog.
User content, provider diagnostics and machine-facing text remain unchanged.

## Locale and live switching

Localization is enabled by default. Settings → General → Language offers System,
English, Español, Deutsch and 한국어. Changes are saved through the host and apply immediately;
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
Tests use isolated thread-local state and default to English. Chat and sidebar
invalidate translated labels, attachment previews and slash descriptions, then
remeasure virtualized rows while preserving task selection, drafts, filters and
the scroll anchor. Native menus and tray items relabel on locale changes. Cached
views must invalidate formatted text explicitly; refreshing windows alone does
not rebuild their stored strings.

Before the local store opens, setup, unlock and recovery views use the OS locale.
The saved preference lives inside the store and becomes available after unlock;
there is no separate plaintext preference.

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
  unknown host errors remain raw, while recognized semantic host failures use
  catalog-backed notices. Keep semantic errors intact through asynchronous
  callbacks; resolving them before the UI receives them freezes their language.
  Use `UiText::context` or `context_args` for a localized wrapper around another
  semantic notice; both layers resolve for the current locale. Worker-side
  `ui::task::Failure` carries sendable keys, arguments and original diagnostics;
  convert it to `UiText` only on the UI thread.
- Translate accessibility labels too. Keep element IDs, keymap slots, commands,
  protocol values and other machine-readable identifiers stable.
- Keep complete sentences together. Give every variable a descriptive name and
  explain it in a translator comment on the English message.
  Fluent bidi isolation is disabled for the supported left-to-right languages,
  so native controls and editable fallback names do not acquire invisible marks. GPUI shaping/fallback-font checks are required before enabling scripts
  that need them.

Lookup tries the selected catalog, then English, then the key itself. Empty or
unformattable values also fall back. Logs contain only the key and catalog, not
arguments or resolver errors that might contain user text. Cached plain labels
log a fallback once per first resolution rather than on every frame.

## Validation and incremental extraction

The existing `agent-test` cargo suite checks catalog syntax, duplicate IDs,
identical shipped key sets, enabled/catalog parity, literal UI lookup keys,
fallback behavior, argument formatting, numeric plurals, unused messages and
undocumented variables. The converted-module
check rejects direct literals in common label methods; expand its module list
as extraction proceeds. It is deliberately a narrow heuristic, not proof that
all user-facing text has been found. Existing unconverted modules remain debt;
new user-facing text should use the catalog rather than increasing that debt.
No new workflow or CI lane is needed.

`app/translation-revisions/{locale}.txt` records the English each translation was
last reviewed against. When English is added, removed or reworded, the catalog
test fails and names those messages. Update them in the translated catalog, then
run `just translations-reviewed <locale>` and commit the result. Comment changes
do not count. The record cannot tell whether anyone translated anything, so
reviewers should expect each changed revision line to come with a changed
translation.

A partial catalog can fall back safely at runtime, but it is not enabled or
shipped until completeness checks pass. To add a language, add its complete
catalog and native `language-name`, register it in `i18n::LANGUAGES`, then run
`just translations-reviewed <locale>`.
Validate layout, font fallback, bidi marks, and platform-native language
metadata before production enablement. Spanish currently has Linux preview
evidence only; macOS/Windows runtime checks remain separate. The macOS bundle
declares English, Spanish, German and Korean, and the existing debug/release bundle generators
include Spanish, German and Korean permission-description resources. Generator tests do not prove
native permission-dialog behavior on macOS.

## Compatibility and remaining coverage

Extraction is substantially larger than just `child(...)` calls. Settings
includes an adapter for the built-in shortcut catalog, with coverage tests for
new catalog entries. Background operations carry presentation data to the UI;
they do not read the GPUI thread's active locale.

- `HostFailure` adds source-owned codes and arguments beside the original
  diagnostic. Ordinary local and remote response paths preserve this metadata;
  the UI accepts known codes and validates their arguments. Unknown codes,
  malformed arguments and legacy responses display the original diagnostic.
  A typed producer alone does not translate a caller that still consumes its
  legacy String adapter. Do not infer codes by matching English error text.
- Initial remote-upload admission, persisted send-receipt outcomes and
  string-only error events retain their legacy contracts. Recovery decisions
  must continue to use original diagnostics and receipt authority, never
  translated display text. Provider diagnostics remain raw.
- Timeline projections add optional semantic label metadata. `TIMELINE_FORMAT`
  3 rebuilds derived projections, with plain/keyed parity tests; original
  stored text remains unchanged. Known metadata supplies translated titles and
  bodies. Missing or unknown metadata falls back to original text without
  classifying English strings. Partial updates preserve existing tool detail.
- MCP sign-in failures that change what the UI does (no address, an unusable
  address, nothing answering, no MCP server there) cross the host as
  `HostFailure` codes. Its other failures and the browser page after sign-in
  stay English. A catalog app saves its English summary when connected, so a
  connected row shows the translated summary while that text is unchanged and
  leaves anything a person wrote as written.
- Model-facing tool descriptions, system prompts, notices sent to models, user
  content and generated content are never translated by UI extraction.
- Local hosted-OAuth completion captures a supported locale per attempt and
  escapes localized HTML. Remote legacy OAuth and headless builds retain an
  English fallback. The standalone Windows NSIS installer remains English;
  installer localization is separate from in-app text.
- Settings dates use translated month names and date patterns. The plan-usage
  DTO includes optional raw product/reset inputs so the UI can format them;
  older hosts remain readable. Numeric separators and money still use the
  existing application formats; Fluent plural selection does not provide
  complete locale formatting.
- Spell checking remains `en_US`; UI language and content language are separate.
- RTL layout/mirroring is not supported by this implementation. Do not enable
  RTL locales until the GPUI/platform limitation is addressed. CJK and other
  scripts require font-fallback testing on every supported desktop platform.

## Mexican Spanish terminology

Use **computadora** for a computer and **equipo** for a team; avoid using the
same noun for both on a screen. Use **almacén** for the local store,
**cuadro de mensaje** for the composer, **salida** for tool output, and **Salir**
for quitting the app (closing a window can leave the host running). Keep CLI
commands, tokens, paths, model names, and external product names unchanged.
Use informal **tú** and Mexican Spanish rather than Spain-specific forms.

The English dictionary is enabled only for English UI. Switching to Spanish
clears its underlines and suggestions; switching back checks the retained text.
A Spanish dictionary remains future work. Optional timeline/error presentation
metadata is decoded leniently: malformed metadata falls back to the original
text, while response identity, authoritative results, and row identity still
use strict decoding. Provider diagnostics remain untouched for Pi retry and
context-overflow decisions; their presentation codes select translated bodies.

The nonblocking dispatch-macro consolidation and global locale observers remain
follow-up maintenance work.
These changes do not establish Windows runtime evidence: exercise live language
switching, native prompts/menus and DirectWrite on Windows before release.

## German terminology

The `de` catalog uses standard German with informal **du**. Regional OS locales
such as `de-DE`, `de-AT` and `de-CH` resolve to this shared catalog. Use **Aufgabe**
for task, **Datenspeicher** for the local store, **Eingabefeld** for composer,
**Werkzeug** for tool and **entfernter Computer** for remote computer. Keep
product names, external editor controls, commands and confirmation words exact.
The English-only spell checker is disabled for German, as for Spanish.
German macOS permission resources accompany the embedded catalog. Native macOS
and Windows layout, font and permission-dialog checks remain required before
release; generator and catalog tests do not establish those runtime results.

## Korean terminology

The `ko` catalog is written in Hangul with friendly polite **해요체** for
sentences (`~해요`, `~했어요`, `~하세요`, `~할까요?`), matching the informal voice
of the Spanish and German catalogs, and short noun forms for buttons, menus and
labels (`저장`, `다시 시도`). Do not use formal `~합니다` endings or casual 반말. Regional OS locales such as `ko-KR` resolve to this
shared catalog. Use **작업** for task, **로컬 저장소** for the local store,
**메시지 입력창** for composer, **도구** for tool, **연동** for integration,
**원격 컴퓨터** for remote computer and **요금제** for plan. Keep product names,
external editor controls, commands and confirmation words (`DELETE`, `ERASE`)
exact. Korean has a single plural category, so counted messages are plain
sentences with a counter (`{ $count }개`) instead of plural selectors. Avoid a
particle whose form depends on the preceding sound directly after a variable;
restructure the sentence, or write both forms (`을(를)`, `이(가)`). Month names
are `1월` to `12월` in both long and short forms, and dates read year, month, day.
The English-only spell checker is disabled for Korean, as for Spanish and German.
Menu type-ahead compares one typed key with the first character of a label and
has no IME composition, so it cannot match Hangul labels typed on a Korean
keyboard; arrow keys and Enter still work. Composed type-ahead is future work.

The bundled Geist, Manrope and Array fonts have no Hangul glyphs, so Korean text
is drawn with the platform's fallback font and its weights and metrics differ
from Latin text. Linux needs an installed Korean font such as Noto Sans CJK KR;
without one the text renders as missing-glyph boxes. Korean macOS permission
resources accompany the embedded catalog. Native macOS and Windows layout,
font-fallback and permission-dialog checks remain required before release;
generator and catalog tests do not establish those runtime results.
