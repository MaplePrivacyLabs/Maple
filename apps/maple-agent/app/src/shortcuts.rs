//! Experimental shortcut overrides, validation, conflicts, and live generations.
//!
//! The shipped catalog and exclusive process-wide installer remain in `keymap`
//! so Maple retains one authoritative default map if this layer is removed.

use std::collections::{BTreeMap, BTreeSet};

use gpui::{App, KeyBinding, Keystroke};

use crate::ui::{application_vim, text_input::vim_actions};

use crate::keymap::{RENAMED_SLOT_IDS, build_binding, catalog, install};

pub(crate) type ShortcutOverrides = BTreeMap<String, Option<String>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShortcutConflictKind {
    Exact,
    Prefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShortcutContextOverlap {
    /// Both slots use the same GPUI context predicate.
    Equivalent,
    /// One known context is nested inside the other (for example TextInput
    /// inside Chat, or composer Normal inside TextInput).
    Scoped,
    /// The catalog cannot prove the two predicates disjoint.
    Possible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShortcutConflict {
    pub other_slot_id: String,
    pub kind: ShortcutConflictKind,
    pub overlap: ShortcutContextOverlap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShortcutRow {
    pub slot_id: String,
    pub label: String,
    pub category: String,
    pub context: Option<String>,
    pub default_sequence: String,
    pub current_sequence: Option<String>,
    pub modified: bool,
    pub conflicts: Vec<ShortcutConflict>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShortcutSnapshot {
    pub generation: u64,
    pub rows: Vec<ShortcutRow>,
    pub last_error: Option<String>,
    /// Non-fatal notice for persisted slots that this build cannot install.
    pub compatibility_warning: Option<String>,
}

impl ShortcutSnapshot {
    /// Validate a recorder candidate and compare it with the live map without
    /// mutating either the snapshot or GPUI.
    pub(crate) fn conflicts_for(
        &self,
        slot_id: &str,
        sequence: &str,
    ) -> Result<Vec<ShortcutConflict>, String> {
        let target = self
            .rows
            .iter()
            .find(|row| row.slot_id == slot_id)
            .ok_or_else(|| format!("unknown shortcut slot '{slot_id}'"))?;
        let candidate = parse_sequence(sequence)?;
        validate_sequence_for_context(slot_id, target.context.as_deref(), &candidate)?;
        Ok(conflicts_against_rows(
            slot_id,
            target.context.as_deref(),
            &candidate,
            &self.rows,
        ))
    }
}

/// Last-known-good shortcut generation owned by the desktop root.
pub(crate) struct ShortcutRuntime {
    snapshot: ShortcutSnapshot,
}

impl ShortcutRuntime {
    /// Install the persisted candidate, falling back to the shipped defaults
    /// if a hand-edited/old setting is invalid. Startup must always retain a
    /// complete map, including ordinary TextInput bindings.
    pub(crate) fn bootstrap(overrides: &ShortcutOverrides, cx: &mut App) -> Self {
        let (prepared, last_error) = match prepare(overrides) {
            Ok(prepared) => (prepared, None),
            Err(error) => (
                prepare(&ShortcutOverrides::new())
                    .expect("the compiled-in shortcut catalog must always be valid"),
                Some(error),
            ),
        };
        let PreparedShortcuts {
            bindings,
            rows,
            compatibility_warning,
        } = prepared;
        if let Some(warning) = &compatibility_warning {
            log::warn!("{warning}");
        }
        install(bindings, cx);
        Self {
            snapshot: ShortcutSnapshot {
                generation: 1,
                rows,
                last_error,
                compatibility_warning,
            },
        }
    }

    /// Prepare a complete replacement before clearing GPUI. Invalid known-slot
    /// contexts or sequences leave the active rows and generation untouched;
    /// only the surfaced error changes. Unknown slots are retained by the
    /// caller and reported as a non-fatal compatibility warning.
    pub(crate) fn replace(
        &mut self,
        overrides: &ShortcutOverrides,
        cx: &mut App,
    ) -> Result<(), String> {
        let prepared = match prepare(overrides) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.snapshot.last_error = Some(error.clone());
                return Err(error);
            }
        };
        let PreparedShortcuts {
            bindings,
            rows,
            compatibility_warning,
        } = prepared;
        if let Some(warning) = &compatibility_warning {
            log::warn!("{warning}");
        }
        install(bindings, cx);
        self.snapshot = ShortcutSnapshot {
            generation: self.snapshot.generation.saturating_add(1),
            rows,
            last_error: None,
            compatibility_warning,
        };
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> ShortcutSnapshot {
        self.snapshot.clone()
    }
}

/// Move overrides saved under a renamed slot id to the slot's current id,
/// unless the current id has its own. Run before editing overrides so a reset
/// or change also retires the old key.
pub(crate) fn migrate_renamed_slots(overrides: &mut ShortcutOverrides) {
    for (old, new) in RENAMED_SLOT_IDS {
        if let Some(saved) = overrides.remove(*old) {
            overrides.entry((*new).to_owned()).or_insert(saved);
        }
    }
}

pub(super) struct PreparedShortcuts {
    pub(super) bindings: Vec<KeyBinding>,
    pub(super) rows: Vec<ShortcutRow>,
    pub(super) compatibility_warning: Option<String>,
}

pub(super) fn prepare(overrides: &ShortcutOverrides) -> Result<PreparedShortcuts, String> {
    let catalog = catalog();
    let known_ids = catalog.iter().map(|slot| slot.id).collect::<BTreeSet<_>>();
    let unknown = overrides
        .keys()
        .filter(|id| {
            !known_ids.contains(id.as_str()) && !RENAMED_SLOT_IDS.iter().any(|(old, _)| old == id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let compatibility_warning = (!unknown.is_empty()).then(|| {
    format!(
        "Saved shortcut override{} for unavailable slot{} {} left untouched and ignored by this build: {}",
        if unknown.len() == 1 { "" } else { "s" },
        if unknown.len() == 1 { "" } else { "s" },
        if unknown.len() == 1 { "was" } else { "were" },
        unknown.join(", ")
    )
});

    let mut bindings = Vec::with_capacity(catalog.len());
    let mut rows = Vec::with_capacity(catalog.len());
    for slot in catalog {
        let saved = overrides.get(slot.id).or_else(|| {
            RENAMED_SLOT_IDS
                .iter()
                .find(|(_, new)| *new == slot.id)
                .and_then(|(old, _)| overrides.get(*old))
        });
        let modified = saved.is_some();
        let current_sequence = match saved {
            Some(None) => None,
            Some(Some(sequence)) => Some(display_sequence(sequence)?),
            None => Some(slot.default_sequence.to_owned()),
        };
        if let Some(sequence) = &current_sequence {
            let parsed = parse_sequence(sequence)?;
            validate_sequence_for_context(slot.id, slot.context, &parsed)?;
            bindings.push(build_binding(&slot, sequence)?);
        }
        rows.push(ShortcutRow {
            slot_id: slot.id.to_owned(),
            label: slot.label.to_owned(),
            category: slot.category.as_str().to_owned(),
            context: slot.context.map(str::to_owned),
            default_sequence: slot.default_sequence.to_owned(),
            current_sequence,
            modified,
            conflicts: Vec::new(),
        });
    }
    attach_conflicts(&mut rows)?;
    Ok(PreparedShortcuts {
        bindings,
        rows,
        compatibility_warning,
    })
}

fn display_sequence(sequence: &str) -> Result<String, String> {
    let display = sequence.split_whitespace().collect::<Vec<_>>().join(" ");
    parse_sequence(&display)?;
    Ok(display)
}

pub(super) fn parse_sequence(sequence: &str) -> Result<Vec<Keystroke>, String> {
    let strokes = sequence
        .split_whitespace()
        .map(Keystroke::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if strokes.is_empty() {
        return Err("a shortcut sequence cannot be empty".to_owned());
    }
    Ok(strokes)
}

/// Global and Chat predicates remain active above ordinary text fields. A
/// text-producing first stroke there would dispatch an action before GPUI can
/// deliver the character (or hold it in the multi-stroke pending path).
/// Focus-owned contexts such as TextInput, composer Vim, and application Vim
/// may intentionally use bare keys and are therefore left alone.
pub(super) fn validate_sequence_for_context(
    slot_id: &str,
    context: Option<&str>,
    strokes: &[Keystroke],
) -> Result<(), String> {
    let captures_ancestor_text_entry = context.is_none() || context == Some("Chat");
    if captures_ancestor_text_entry && strokes.first().is_some_and(Keystroke::is_ime_in_progress) {
        return Err(format!(
            "shortcut '{slot_id}' starts with the text-producing key '{}', which would intercept typing in text fields",
            strokes[0].unparse()
        ));
    }
    Ok(())
}

fn attach_conflicts(rows: &mut [ShortcutRow]) -> Result<(), String> {
    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        parsed.push(
            row.current_sequence
                .as_deref()
                .map(parse_sequence)
                .transpose()?,
        );
    }
    for left in 0..rows.len() {
        let Some(left_sequence) = &parsed[left] else {
            continue;
        };
        for right in left + 1..rows.len() {
            let Some(right_sequence) = &parsed[right] else {
                continue;
            };
            let Some(kind) = sequence_conflict(left_sequence, right_sequence) else {
                continue;
            };
            let Some(overlap) = context_overlap(
                rows[left].context.as_deref(),
                rows[right].context.as_deref(),
            ) else {
                continue;
            };
            // Shipped parent/child bindings deliberately shadow one another,
            // but a user-created shadow is still useful conflict information.
            if overlap == ShortcutContextOverlap::Scoped
                && !rows[left].modified
                && !rows[right].modified
            {
                continue;
            }
            rows[left].conflicts.push(ShortcutConflict {
                other_slot_id: rows[right].slot_id.clone(),
                kind,
                overlap,
            });
            rows[right].conflicts.push(ShortcutConflict {
                other_slot_id: rows[left].slot_id.clone(),
                kind,
                overlap,
            });
        }
    }
    Ok(())
}

fn conflicts_against_rows(
    slot_id: &str,
    context: Option<&str>,
    candidate: &[Keystroke],
    rows: &[ShortcutRow],
) -> Vec<ShortcutConflict> {
    rows.iter()
        .filter(|row| row.slot_id != slot_id)
        .filter_map(|row| {
            let sequence = row.current_sequence.as_deref()?;
            let parsed = parse_sequence(sequence).ok()?;
            let kind = sequence_conflict(candidate, &parsed)?;
            let overlap = context_overlap(context, row.context.as_deref())?;
            Some(ShortcutConflict {
                other_slot_id: row.slot_id.clone(),
                kind,
                overlap,
            })
        })
        .collect()
}

fn sequence_conflict(left: &[Keystroke], right: &[Keystroke]) -> Option<ShortcutConflictKind> {
    if left == right {
        Some(ShortcutConflictKind::Exact)
    } else if left.starts_with(right) || right.starts_with(left) {
        Some(ShortcutConflictKind::Prefix)
    } else {
        None
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum KnownContext {
    Global,
    Chat,
    Transcript,
    /// An open popup menu (see `ui::popup`).
    Menu,
    TextInput,
    /// Text fields with Vim off or in Insert. Normal and Visual are excluded.
    StandardText,
    ComposerNormal,
    ComposerVisual,
    ComposerInsert,
    ApplicationVimRoot,
    ApplicationVimMenu,
    ApplicationVimOtherInput,
    ApplicationVimComposerNormal,
    Other,
}

fn known_context(context: Option<&str>) -> KnownContext {
    match context {
        None => KnownContext::Global,
        Some("Chat") => KnownContext::Chat,
        Some("Transcript") => KnownContext::Transcript,
        Some(crate::ui::popup::MENU_CONTEXT) => KnownContext::Menu,
        Some("TextInput") => KnownContext::TextInput,
        Some(crate::keymap::STANDARD_TEXT_CONTEXT) => KnownContext::StandardText,
        Some(vim_actions::NORMAL_CONTEXT) => KnownContext::ComposerNormal,
        Some(vim_actions::VISUAL_CONTEXT) => KnownContext::ComposerVisual,
        Some(vim_actions::INSERT_CONTEXT) => KnownContext::ComposerInsert,
        Some(application_vim::ROOT_CONTEXT) => KnownContext::ApplicationVimRoot,
        Some(application_vim::MENU_CONTEXT) => KnownContext::ApplicationVimMenu,
        Some(application_vim::OTHER_INPUT_CONTEXT) => KnownContext::ApplicationVimOtherInput,
        Some(application_vim::COMPOSER_NORMAL_CONTEXT) => {
            KnownContext::ApplicationVimComposerNormal
        }
        Some(_) => KnownContext::Other,
    }
}

fn context_overlap(left: Option<&str>, right: Option<&str>) -> Option<ShortcutContextOverlap> {
    if left == right {
        return Some(ShortcutContextOverlap::Equivalent);
    }
    let left = known_context(left);
    let right = known_context(right);
    if left == KnownContext::Global || right == KnownContext::Global {
        return Some(ShortcutContextOverlap::Scoped);
    }
    if left == KnownContext::Other || right == KnownContext::Other {
        return Some(ShortcutContextOverlap::Possible);
    }
    if left == KnownContext::Chat || right == KnownContext::Chat {
        return Some(ShortcutContextOverlap::Scoped);
    }
    if matches!(
        (left, right),
        (KnownContext::Menu, KnownContext::ApplicationVimMenu)
            | (KnownContext::ApplicationVimMenu, KnownContext::Menu)
            // A text field inside a menu (the project menu's path field).
            | (KnownContext::Menu, KnownContext::TextInput)
            | (KnownContext::TextInput, KnownContext::Menu)
            | (KnownContext::Menu, KnownContext::StandardText)
            | (KnownContext::StandardText, KnownContext::Menu)
            // The transcript's right-click menu opens inside the transcript.
            | (KnownContext::Menu, KnownContext::Transcript)
            | (KnownContext::Transcript, KnownContext::Menu)
            | (KnownContext::Transcript, KnownContext::ApplicationVimRoot)
            | (KnownContext::ApplicationVimRoot, KnownContext::Transcript)
            | (
                KnownContext::TextInput,
                KnownContext::ApplicationVimOtherInput
            )
            | (
                KnownContext::ApplicationVimOtherInput,
                KnownContext::TextInput
            )
            | (
                KnownContext::TextInput,
                KnownContext::ApplicationVimComposerNormal
            )
            | (
                KnownContext::ApplicationVimComposerNormal,
                KnownContext::TextInput
            )
            | (
                KnownContext::ComposerNormal,
                KnownContext::ApplicationVimComposerNormal
            )
            | (
                KnownContext::ApplicationVimComposerNormal,
                KnownContext::ComposerNormal
            )
            | (KnownContext::StandardText, KnownContext::TextInput)
            | (KnownContext::TextInput, KnownContext::StandardText)
            | (KnownContext::StandardText, KnownContext::ComposerInsert)
            | (KnownContext::ComposerInsert, KnownContext::StandardText)
            | (
                KnownContext::StandardText,
                KnownContext::ApplicationVimOtherInput
            )
            | (
                KnownContext::ApplicationVimOtherInput,
                KnownContext::StandardText
            )
    ) {
        return Some(ShortcutContextOverlap::Scoped);
    }
    if matches!(
        left,
        KnownContext::ApplicationVimRoot
            | KnownContext::ApplicationVimMenu
            | KnownContext::ApplicationVimOtherInput
            | KnownContext::ApplicationVimComposerNormal
    ) || matches!(
        right,
        KnownContext::ApplicationVimRoot
            | KnownContext::ApplicationVimMenu
            | KnownContext::ApplicationVimOtherInput
            | KnownContext::ApplicationVimComposerNormal
    ) {
        // The four application predicates explicitly exclude one another.
        // Their only overlapping legacy parents are handled above.
        return None;
    }
    if matches!(left, KnownContext::TextInput)
        && matches!(
            right,
            KnownContext::ComposerNormal
                | KnownContext::ComposerVisual
                | KnownContext::ComposerInsert
        )
        || matches!(right, KnownContext::TextInput)
            && matches!(
                left,
                KnownContext::ComposerNormal
                    | KnownContext::ComposerVisual
                    | KnownContext::ComposerInsert
            )
    {
        return Some(ShortcutContextOverlap::Scoped);
    }
    // A menu, the transcript, ordinary TextInput, and the three mutually
    // exclusive composer modes cannot otherwise be active focus contexts
    // together.
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text_input::vim::Motion;
    use crate::ui::{chat, text_input};

    use gpui::{
        Action, AppContext, Context, Entity, Focusable, IntoElement, KeyBindingContextPredicate,
        Keystroke, ParentElement, Render, Styled, Window, div, px,
    };

    struct InputHost {
        input: Entity<text_input::TextInput>,
    }

    impl Render for InputHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(500.)).h(px(240.)).child(self.input.clone())
        }
    }

    #[test]
    fn shipped_catalog_prepares_without_customization_conflicts() {
        let prepared = prepare(&ShortcutOverrides::new()).unwrap();
        assert_eq!(prepared.bindings.len(), crate::keymap::catalog().len());
        assert!(
            prepared.rows.iter().all(|row| row.conflicts.is_empty()),
            "intentional parent/child context shadowing is not a user conflict"
        );
    }

    #[test]
    fn overrides_saved_under_a_renamed_slot_still_apply() {
        let mut overrides = ShortcutOverrides::new();
        overrides.insert("project_menu.next".into(), Some("ctrl-n".into()));
        let prepared = prepare(&overrides).unwrap();
        assert_eq!(prepared.compatibility_warning, None);
        let row = prepared
            .rows
            .iter()
            .find(|row| row.slot_id == "menu.next")
            .expect("renamed slot");
        assert_eq!(row.current_sequence.as_deref(), Some("ctrl-n"));
        assert!(row.modified);

        // Editing moves the override to the current id.
        migrate_renamed_slots(&mut overrides);
        assert_eq!(
            overrides.into_iter().collect::<Vec<_>>(),
            vec![("menu.next".to_string(), Some("ctrl-n".to_string()))]
        );
    }

    #[test]
    fn native_equivalent_spelling_is_an_exact_conflict() {
        let snapshot = ShortcutSnapshot {
            generation: 1,
            rows: vec![
                ShortcutRow {
                    slot_id: "one".into(),
                    label: "One".into(),
                    category: "Test".into(),
                    context: Some("Transcript".into()),
                    default_sequence: "G".into(),
                    current_sequence: Some("G".into()),
                    modified: false,
                    conflicts: Vec::new(),
                },
                ShortcutRow {
                    slot_id: "two".into(),
                    label: "Two".into(),
                    category: "Test".into(),
                    context: Some("Transcript".into()),
                    default_sequence: "x".into(),
                    current_sequence: Some("x".into()),
                    modified: false,
                    conflicts: Vec::new(),
                },
            ],
            last_error: None,
            compatibility_warning: None,
        };
        assert_eq!(
            snapshot.conflicts_for("two", "shift-g").unwrap(),
            vec![ShortcutConflict {
                other_slot_id: "one".into(),
                kind: ShortcutConflictKind::Exact,
                overlap: ShortcutContextOverlap::Equivalent,
            }]
        );
        assert_eq!(
            snapshot.conflicts_for("two", "G G").unwrap(),
            vec![ShortcutConflict {
                other_slot_id: "one".into(),
                kind: ShortcutConflictKind::Prefix,
                overlap: ShortcutContextOverlap::Equivalent,
            }]
        );
    }

    #[test]
    fn user_shadowing_is_reported_without_flagging_shipped_context_layers() {
        let defaults = prepare(&ShortcutOverrides::new()).unwrap();
        assert!(defaults.rows.iter().all(|row| row.conflicts.is_empty()));

        assert_eq!(
            ShortcutSnapshot {
                generation: 1,
                rows: defaults.rows.clone(),
                last_error: None,
                compatibility_warning: None,
            }
            .conflicts_for("chat.new_task", "secondary-q")
            .unwrap(),
            vec![ShortcutConflict {
                other_slot_id: "app.quit".into(),
                kind: ShortcutConflictKind::Exact,
                overlap: ShortcutContextOverlap::Scoped,
            }]
        );

        let mut overrides = ShortcutOverrides::new();
        overrides.insert("chat.new_task".into(), Some("secondary-q".into()));
        let prepared = prepare(&overrides).unwrap();
        let modified = prepared
            .rows
            .iter()
            .find(|row| row.slot_id == "chat.new_task")
            .unwrap();
        assert_eq!(modified.conflicts.len(), 1);
        assert_eq!(modified.conflicts[0].other_slot_id, "app.quit");
    }

    #[test]
    fn standard_text_overlaps_text_fields_and_insert_only() {
        let standard = Some(crate::keymap::STANDARD_TEXT_CONTEXT);
        assert_eq!(
            context_overlap(standard, Some("TextInput")),
            Some(ShortcutContextOverlap::Scoped)
        );
        assert_eq!(
            context_overlap(standard, Some(vim_actions::INSERT_CONTEXT)),
            Some(ShortcutContextOverlap::Scoped)
        );
        assert_eq!(
            context_overlap(standard, Some(vim_actions::NORMAL_CONTEXT)),
            None
        );
        assert_eq!(
            context_overlap(standard, Some(vim_actions::VISUAL_CONTEXT)),
            None
        );
    }

    #[test]
    fn transcript_and_application_root_are_scoped_contexts() {
        assert_eq!(
            context_overlap(Some("Transcript"), Some(application_vim::ROOT_CONTEXT)),
            Some(ShortcutContextOverlap::Scoped)
        );
        assert_eq!(
            context_overlap(Some(application_vim::ROOT_CONTEXT), Some("Transcript")),
            Some(ShortcutContextOverlap::Scoped)
        );
    }

    #[gpui::test]
    fn invalid_replacement_preserves_live_generation(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|app| {
            let mut runtime = ShortcutRuntime::bootstrap(&ShortcutOverrides::new(), app);
            let before = runtime.snapshot();
            let mut overrides = ShortcutOverrides::new();
            overrides.insert("chat.new_task".into(), Some("ctrl-a-b".into()));
            assert!(runtime.replace(&overrides, app).is_err());
            let after = runtime.snapshot();
            assert_eq!(after.generation, before.generation);
            assert_eq!(after.rows, before.rows);
            assert!(after.last_error.is_some());
        });
    }

    #[gpui::test]
    fn replacement_remaps_and_disables_exact_slots(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|app| {
            let mut runtime = ShortcutRuntime::bootstrap(&ShortcutOverrides::new(), app);
            let mut overrides = ShortcutOverrides::new();
            overrides.insert("chat.new_task".into(), Some("secondary-shift-n".into()));
            overrides.insert("chat.focus_search".into(), None);
            overrides.insert("text_input.backspace".into(), Some("ctrl-h".into()));
            overrides.insert("composer_vim.normal.motion.left.h".into(), Some("q".into()));
            runtime.replace(&overrides, app).unwrap();
            let snapshot = runtime.snapshot();
            assert_eq!(snapshot.generation, 2);
            let remapped = snapshot
                .rows
                .iter()
                .find(|row| row.slot_id == "chat.new_task")
                .unwrap();
            assert_eq!(
                remapped.current_sequence.as_deref(),
                Some("secondary-shift-n")
            );
            assert!(remapped.modified);
            let disabled = snapshot
                .rows
                .iter()
                .find(|row| row.slot_id == "chat.focus_search")
                .unwrap();
            assert_eq!(disabled.current_sequence, None);
            assert!(disabled.modified);
            assert_eq!(
                sequences_for_action(app, &chat::NewTask, Some("Chat")),
                vec![canonical_sequence_text("secondary-shift-n")]
            );
            assert_eq!(
                sequences_for_action(app, &text_input::Backspace, Some("TextInput")),
                vec![canonical_sequence_text("ctrl-h")]
            );
            assert_eq!(
                sequences_for_action(
                    app,
                    &vim_actions::VimMotion {
                        motion: Motion::Left,
                    },
                    Some(vim_actions::NORMAL_CONTEXT),
                ),
                vec![
                    canonical_sequence_text("q"),
                    canonical_sequence_text("left")
                ]
            );
        });
    }

    #[gpui::test]
    fn unknown_slot_is_ignored_without_discarding_known_overrides_at_bootstrap(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| {
            let mut overrides = ShortcutOverrides::new();
            overrides.insert("removed.slot".into(), Some("x".into()));
            overrides.insert("chat.new_task".into(), Some("secondary-shift-n".into()));
            let runtime = ShortcutRuntime::bootstrap(&overrides, app);
            let snapshot = runtime.snapshot();
            assert_eq!(snapshot.generation, 1);
            assert_eq!(snapshot.rows.len(), crate::keymap::catalog().len());
            assert!(snapshot.last_error.is_none());
            assert!(
                snapshot
                    .compatibility_warning
                    .as_deref()
                    .unwrap()
                    .contains("removed.slot")
            );
            assert_eq!(
                sequences_for_action(app, &chat::NewTask, Some("Chat")),
                vec![canonical_sequence_text("secondary-shift-n")]
            );
            assert_eq!(overrides.get("removed.slot"), Some(&Some("x".to_owned())));
        });
    }

    #[gpui::test]
    fn unknown_slot_does_not_block_known_set_disable_or_reset(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|app| {
            let mut overrides = ShortcutOverrides::from([(
                "newer-build.slot".to_owned(),
                Some("secondary-j".to_owned()),
            )]);
            let mut runtime = ShortcutRuntime::bootstrap(&overrides, app);

            for (override_value, expected, modified) in [
                (
                    Some(Some("secondary-shift-n".to_owned())),
                    Some("secondary-shift-n"),
                    true,
                ),
                (Some(None), None, true),
                (None, Some("secondary-n"), false),
            ] {
                match override_value {
                    Some(value) => {
                        overrides.insert("chat.new_task".into(), value);
                    }
                    None => {
                        overrides.remove("chat.new_task");
                    }
                }
                runtime.replace(&overrides, app).unwrap();
                let snapshot = runtime.snapshot();
                let row = snapshot
                    .rows
                    .iter()
                    .find(|row| row.slot_id == "chat.new_task")
                    .unwrap();
                assert_eq!(row.current_sequence.as_deref(), expected);
                assert_eq!(row.modified, modified);
                assert!(snapshot.last_error.is_none());
                assert!(
                    snapshot
                        .compatibility_warning
                        .as_deref()
                        .unwrap()
                        .contains("newer-build.slot")
                );
                assert!(overrides.contains_key("newer-build.slot"));
            }

            let reset = runtime.snapshot();
            assert_eq!(reset.generation, 4);
            assert_eq!(
                overrides.get("newer-build.slot"),
                Some(&Some("secondary-j".to_owned()))
            );
        });
    }

    #[test]
    fn ancestor_shortcuts_reject_text_producing_first_strokes() {
        let defaults = prepare(&ShortcutOverrides::new()).unwrap();
        let snapshot = ShortcutSnapshot {
            generation: 1,
            rows: defaults.rows,
            last_error: None,
            compatibility_warning: None,
        };

        for (slot_id, sequence) in [
            ("app.quit", "é"),
            ("chat.toggle_sidebar", "g s"),
            ("chat.toggle_sidebar", "G"),
        ] {
            let error = snapshot.conflicts_for(slot_id, sequence).unwrap_err();
            assert!(error.contains("would intercept typing in text fields"));
        }
        assert!(
            snapshot
                .conflicts_for("chat.toggle_sidebar", "secondary-g")
                .is_ok()
        );

        let mut unsafe_override = ShortcutOverrides::new();
        unsafe_override.insert("chat.toggle_sidebar".into(), Some("g s".into()));
        assert!(prepare(&unsafe_override).is_err());

        // Focus-owned contexts may intentionally consume bare keys. Keep this
        // open-ended so later contexts that explicitly exclude TextInput, such
        // as application Vim root navigation, remain valid without a whitelist.
        for context in [
            Some("TextInput"),
            Some(vim_actions::NORMAL_CONTEXT),
            Some("ApplicationVim && !TextInput && !Menu"),
        ] {
            validate_sequence_for_context(
                "focus-owned.test",
                context,
                &parse_sequence("g g").unwrap(),
            )
            .unwrap();
        }
    }

    #[gpui::test]
    fn ordinary_text_input_remap_removes_the_old_binding(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut overrides = ShortcutOverrides::new();
            overrides.insert("text_input.backspace".into(), Some("ctrl-h".into()));
            let _runtime = ShortcutRuntime::bootstrap(&overrides, cx);
            let mut input = text_input::TextInput::new("", cx);
            input.set_text("ab", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes("backspace");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "ab");
        cx.simulate_keystrokes("ctrl-h");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "a");
    }

    #[gpui::test]
    fn composer_vim_remap_dispatches_the_new_key_only(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let input = cx.new(|cx| {
            let mut overrides = ShortcutOverrides::new();
            overrides.insert("composer_vim.normal.delete_chars".into(), Some("q".into()));
            let _runtime = ShortcutRuntime::bootstrap(&overrides, cx);
            let mut input = text_input::TextInput::new("", cx).composer_vim(true);
            input.set_text("abc", cx);
            input
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| InputHost {
            input: input.clone(),
        });
        let focus = cx.update(|_window, app| input.read(app).focus_handle(app));
        cx.update(|window, app| window.focus(&focus, app));

        cx.simulate_keystrokes("x");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "abc");
        cx.simulate_keystrokes("q");
        assert_eq!(cx.update(|_window, app| input.read(app).text()), "ab");
    }

    fn sequences_for_action(app: &App, action: &dyn Action, context: Option<&str>) -> Vec<String> {
        let expected = context.map(|context| KeyBindingContextPredicate::parse(context).unwrap());
        let keymap = app.key_bindings();
        let keymap = keymap.borrow();
        keymap
            .bindings_for_action(action)
            .filter(|binding| binding.predicate().as_deref() == expected.as_ref())
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|stroke| stroke.inner().unparse())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn canonical_sequence_text(sequence: &str) -> String {
        parse_sequence(sequence)
            .unwrap()
            .iter()
            .map(Keystroke::unparse)
            .collect::<Vec<_>>()
            .join(" ")
    }
}
