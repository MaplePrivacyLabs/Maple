//! Discovery surfaces: the root command palette and which-key.
//!
//! Both existed in the prototype as GPUI overlays (`app/src/harness/
//! palette.rs`, `which_key.rs`). Their value was never the rendering; it was
//! that they were *derived* from the same registry and resolved keymap that
//! drive dispatch, so a user could always find the action behind a button,
//! the key behind an action, and the reason an action was unavailable. This
//! module keeps those derivations as plain data structures:
//!
//! - [`PaletteIndex`] searches labels, IDs, descriptions, categories, and
//!   current bindings, and ranks matches deterministically. Activation is
//!   the caller's job: it must go through
//!   [`ActionHost::invoke_palette`](crate::ActionHost::invoke_palette) so the
//!   gesture carries `DirectUser`/`CommandPalette` provenance.
//! - [`WhichKeyTrie`] compiles effective bindings into a prefix trie so a
//!   pending multi-stroke sequence can show its continuations, filtered to
//!   the contexts that are currently active.

use std::collections::BTreeMap;

use crate::{
    ActionDescriptor, ActionId, ActionRegistry, Availability, ContextExpression, KeymapBinding,
    ResolvedBinding, ResolvedBindingState, ResolvedKeymap,
};

/// One searchable palette entry.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteEntry {
    pub action_id: ActionId,
    pub label: String,
    pub description: String,
    pub category: String,
    /// Effective key sequences bound to this action in the active profile,
    /// with the context each applies in.
    pub bindings: Vec<(String, String)>,
    /// Ex-style alias reachable from the Vim colon layer (`:settings`).
    pub alias: Option<String>,
}

/// A ranked palette hit.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteHit<'a> {
    pub entry: &'a PaletteEntry,
    pub score: u32,
    pub availability: Availability,
}

/// Searchable index over the registry and the active resolved keymap.
#[derive(Clone, Debug, Default)]
pub struct PaletteIndex {
    entries: Vec<PaletteEntry>,
}

impl PaletteIndex {
    /// Builds the index. `aliases` maps colon-layer words to action IDs
    /// (`"settings" -> settings.open`).
    pub fn build(
        registry: &ActionRegistry,
        keymap: Option<&ResolvedKeymap>,
        aliases: &BTreeMap<String, ActionId>,
    ) -> Self {
        let mut by_action: BTreeMap<&ActionId, Vec<(String, String)>> = BTreeMap::new();
        if let Some(keymap) = keymap {
            for binding in &keymap.bindings {
                if binding.state != ResolvedBindingState::Effective {
                    continue;
                }
                if let Some(action_id) = binding.binding.action_id() {
                    by_action
                        .entry(action_id)
                        .or_default()
                        .push((binding.sequence.to_string(), binding.context.to_string()));
                }
            }
        }
        let alias_for: BTreeMap<&ActionId, &String> =
            aliases.iter().map(|(a, id)| (id, a)).collect();
        let entries = registry
            .iter()
            .map(|(id, descriptor)| PaletteEntry {
                action_id: id.clone(),
                label: descriptor.label.clone(),
                description: descriptor.description.clone(),
                category: descriptor.category.clone(),
                bindings: by_action.get(id).cloned().unwrap_or_default(),
                alias: alias_for.get(id).map(|a| (*a).clone()),
            })
            .collect();
        Self { entries }
    }

    pub fn entries(&self) -> &[PaletteEntry] {
        &self.entries
    }

    /// Searches the index. `availability` lets the caller supply live
    /// availability per action so disabled entries stay visible with their
    /// reason instead of disappearing.
    pub fn search<'a>(
        &'a self,
        query: &str,
        availability: impl Fn(&ActionId) -> Availability,
    ) -> Vec<PaletteHit<'a>> {
        let query = query.trim().to_ascii_lowercase();
        let mut hits: Vec<PaletteHit<'a>> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let score = score(entry, &query)?;
                Some(PaletteHit {
                    entry,
                    score,
                    availability: availability(&entry.action_id),
                })
            })
            .collect();
        // Available before disabled, then score, then stable ID order.
        hits.sort_by(|a, b| {
            b.availability
                .is_available()
                .cmp(&a.availability.is_available())
                .then(b.score.cmp(&a.score))
                .then(a.entry.action_id.cmp(&b.entry.action_id))
        });
        hits
    }
}

/// Deterministic ranking: exact alias or ID beats a label prefix, which beats
/// a word-prefix match, which beats a substring anywhere.
fn score(entry: &PaletteEntry, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1);
    }
    let id = entry.action_id.as_str();
    let label = entry.label.to_ascii_lowercase();
    if entry.alias.as_deref() == Some(query) || id == query {
        return Some(1000);
    }
    if label.starts_with(query) || id.starts_with(query) {
        return Some(800);
    }
    if label.split_whitespace().any(|w| w.starts_with(query))
        || id.split(['.', '_']).any(|w| w.starts_with(query))
    {
        return Some(600);
    }
    if label.contains(query) || id.contains(query) {
        return Some(400);
    }
    if entry.category.to_ascii_lowercase().contains(query) {
        return Some(300);
    }
    if entry
        .bindings
        .iter()
        .any(|(sequence, _)| sequence.contains(query))
    {
        return Some(250);
    }
    if entry.description.to_ascii_lowercase().contains(query) {
        return Some(200);
    }
    None
}

/// A which-key continuation: the next stroke and what it leads to.
#[derive(Clone, Debug, PartialEq)]
pub struct Continuation {
    pub stroke: String,
    /// `Some` when this stroke completes a binding.
    pub action: Option<(ActionId, String)>,
    /// Number of longer bindings that still start with this stroke.
    pub deeper: usize,
    pub context: String,
}

#[derive(Debug, Default)]
struct Node {
    children: BTreeMap<String, Node>,
    /// Complete bindings ending exactly here, keyed by context.
    leaves: Vec<(ContextExpression, ActionId)>,
}

/// Prefix trie over effective bindings, for pending-chord help.
#[derive(Debug, Default)]
pub struct WhichKeyTrie {
    root: Node,
}

impl WhichKeyTrie {
    pub fn build(keymap: &ResolvedKeymap, registry: &ActionRegistry) -> Self {
        let mut trie = Self::default();
        for binding in &keymap.bindings {
            trie.insert(binding, registry);
        }
        trie
    }

    fn insert(&mut self, binding: &ResolvedBinding, registry: &ActionRegistry) {
        if binding.state != ResolvedBindingState::Effective {
            return;
        }
        let KeymapBinding::Action { action_id, .. } = &binding.binding else {
            return;
        };
        if !registry.contains(action_id) {
            return;
        }
        let mut node = &mut self.root;
        for stroke in binding.sequence.strokes() {
            node = node.children.entry(stroke.to_owned()).or_default();
        }
        node.leaves
            .push((binding.context.clone(), action_id.clone()));
    }

    /// Continuations after `pending` strokes, restricted to bindings whose
    /// context expression is one of `active_contexts` (already evaluated by
    /// the caller against the focus/context stack). Labels come from the
    /// registry so which-key shows human copy, not just IDs.
    pub fn continuations(
        &self,
        pending: &[&str],
        active_contexts: &[ContextExpression],
        registry: &ActionRegistry,
    ) -> Vec<Continuation> {
        let mut node = &self.root;
        for stroke in pending {
            match node.children.get(*stroke) {
                Some(next) => node = next,
                None => return Vec::new(),
            }
        }
        let is_active = |context: &ContextExpression| active_contexts.contains(context);
        let mut out = Vec::new();
        for (stroke, child) in &node.children {
            let leaf = child.leaves.iter().find(|(context, _)| is_active(context));
            let deeper = count_active_leaves(child, &is_active) - usize::from(leaf.is_some());
            if leaf.is_none() && deeper == 0 {
                continue;
            }
            out.push(Continuation {
                stroke: stroke.clone(),
                action: leaf.map(|(_, id)| (id.clone(), label_for(registry, id))),
                deeper,
                context: leaf
                    .map(|(context, _)| context.to_string())
                    .unwrap_or_default(),
            });
        }
        out
    }
}

fn count_active_leaves(node: &Node, is_active: &dyn Fn(&ContextExpression) -> bool) -> usize {
    node.leaves.iter().filter(|(c, _)| is_active(c)).count()
        + node
            .children
            .values()
            .map(|child| count_active_leaves(child, is_active))
            .sum::<usize>()
}

fn label_for(registry: &ActionRegistry, id: &ActionId) -> String {
    registry
        .descriptor(id)
        .map(|d: &ActionDescriptor| d.label.clone())
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DisabledReasonCode, KeymapDocument, ShortcutProfile, catalog::initial_registry,
        resolve_keymap,
    };

    fn template(registry: &ActionRegistry, profile: ShortcutProfile) -> KeymapDocument {
        // Build a template document from the catalog's default bindings.
        let mut by_context: BTreeMap<String, serde_json::Map<String, serde_json::Value>> =
            BTreeMap::new();
        for (id, descriptor) in registry.iter() {
            for binding in &descriptor.default_bindings {
                if binding.profile == profile {
                    by_context
                        .entry(binding.context.clone())
                        .or_default()
                        .insert(
                            binding.sequence.clone(),
                            serde_json::Value::String(id.to_string()),
                        );
                }
            }
        }
        let entries: Vec<serde_json::Value> = by_context
            .into_iter()
            .map(
                |(context, bindings)| serde_json::json!({"context": context, "bindings": bindings}),
            )
            .collect();
        KeymapDocument::parse(&serde_json::Value::Array(entries).to_string()).unwrap()
    }

    fn keymap(profile: ShortcutProfile) -> (ActionRegistry, ResolvedKeymap) {
        let registry = initial_registry().unwrap();
        let standard = template(&registry, ShortcutProfile::Standard);
        let vim = template(&registry, ShortcutProfile::Vim);
        let resolved = resolve_keymap(
            profile,
            &standard,
            &vim,
            &KeymapDocument::default(),
            &registry,
        )
        .unwrap();
        (registry, resolved)
    }

    #[test]
    fn palette_ranks_alias_then_prefix_and_keeps_disabled_visible() {
        let (registry, resolved) = keymap(ShortcutProfile::Standard);
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "settings".to_owned(),
            ActionId::parse("settings.open").unwrap(),
        );
        let index = PaletteIndex::build(&registry, Some(&resolved), &aliases);
        let hits = index.search("settings", |_| Availability::Available);
        assert_eq!(hits[0].entry.action_id.as_str(), "settings.open");
        assert_eq!(hits[0].score, 1000);
        assert!(
            hits.iter()
                .any(|h| h.entry.action_id.as_str() == "settings.set_theme")
        );

        let disabled = DisabledReasonCode::parse("no_active_task").unwrap();
        let hits = index.search("task", |id| {
            if id.as_str() == "task.rename" {
                Availability::disabled(disabled.clone(), "No task is selected")
            } else {
                Availability::Available
            }
        });
        let rename = hits
            .iter()
            .position(|h| h.entry.action_id.as_str() == "task.rename")
            .unwrap();
        assert!(!hits[rename].availability.is_available());
        assert!(hits[..rename].iter().all(|h| h.availability.is_available()));
    }

    #[test]
    fn palette_shows_current_bindings_from_the_resolved_keymap() {
        let (registry, resolved) = keymap(ShortcutProfile::Standard);
        let index = PaletteIndex::build(&registry, Some(&resolved), &BTreeMap::new());
        let quit = index
            .entries()
            .iter()
            .find(|e| e.action_id.as_str() == "app.quit")
            .unwrap();
        assert_eq!(
            quit.bindings,
            vec![("cmd-q".to_owned(), "MapleApp".to_owned())]
        );
        let by_key = index.search("cmd-q", |_| Availability::Available);
        assert_eq!(by_key[0].entry.action_id.as_str(), "app.quit");
    }

    #[test]
    fn which_key_lists_space_leader_continuations_in_vim() {
        let (registry, resolved) = keymap(ShortcutProfile::Vim);
        let trie = WhichKeyTrie::build(&resolved, &registry);
        let active = vec![
            ContextExpression::parse("MapleApp && app_vim_mode == normal").unwrap(),
            ContextExpression::parse("Chat && app_vim_mode == normal").unwrap(),
        ];
        let next = trie.continuations(&["space"], &active, &registry);
        let strokes: Vec<&str> = next.iter().map(|c| c.stroke.as_str()).collect();
        assert_eq!(strokes, vec!["n", "p", "s"]);
        let settings = next.iter().find(|c| c.stroke == "s").unwrap();
        assert_eq!(settings.action.as_ref().unwrap().1, "Open Settings");
        assert_eq!(settings.deeper, 0);
        // Sidebar-only bindings do not leak into a context that is not active.
        let sidebar_only = trie.continuations(&[], &active, &registry);
        assert!(sidebar_only.iter().all(|c| c.stroke != "j"));
    }

    #[test]
    fn which_key_reports_prefixes_and_dead_ends() {
        let (registry, resolved) = keymap(ShortcutProfile::Standard);
        let trie = WhichKeyTrie::build(&resolved, &registry);
        let active = vec![ContextExpression::parse("MapleApp").unwrap()];
        let root = trie.continuations(&[], &active, &registry);
        let cmd_k = root.iter().find(|c| c.stroke == "cmd-k").unwrap();
        assert!(cmd_k.action.is_none());
        assert_eq!(cmd_k.deeper, 1);
        assert!(trie.continuations(&["nope"], &active, &registry).is_empty());
    }
}
