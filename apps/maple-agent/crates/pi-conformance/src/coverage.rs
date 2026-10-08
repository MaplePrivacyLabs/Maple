//! One-to-one upstream inventory coverage and Rust test correspondence.

use crate::{
    CheckResult,
    dependencies::PI_CRATES,
    integrity::{check_hash, safe_relative},
    json, read, toml,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{Attribute, Item, Meta};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub pin: InventoryPin,
    pub files: Vec<InventoryFile>,
    pub tests: Vec<InventoryTest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryPin {
    pub tag: String,
    pub rev: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryTest {
    pub id: String,
    pub file: String,
    pub ancestors: Vec<String>,
    pub title: String,
    pub line: u64,
    pub column: u64,
    pub mode: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Map {
    meta: MapMeta,
    #[serde(default)]
    file: Vec<InventoryFile>,
    #[serde(default)]
    test: Vec<Mapping>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapMeta {
    pub pin: String,
    pub revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    id: String,
    file: String,
    line: u64,
    status: Status,
    reason: Option<String>,
    adaptation: Option<String>,
    phase: Option<u32>,
    rust: Option<String>,
    #[serde(default)]
    corpus: Vec<String>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ported,
    Adapted,
    Corpus,
    Excluded,
    Pending,
}

#[derive(Debug, Default)]
pub struct CoverageSummary {
    pub inventory_tests: usize,
    pub pending_tests: usize,
    pub translated_tests: usize,
    pub corpus_tests: usize,
    pub excluded_tests: usize,
}

pub fn snake(name: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('_');
            }
            separator = false;
            result.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
    }
    if result.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        result.insert(0, 'r');
    }
    if result.is_empty() {
        result.push_str("unnamed");
    }
    result
}

fn crate_name(file: &str) -> CheckResult<&'static str> {
    if file == "packages/coding-agent/test/mcp-extension.test.ts" {
        return Ok("pi-mcp");
    }
    match file.split('/').nth(1) {
        Some("ai") => Ok("pi-ai"),
        Some("agent") => Ok("pi-agent-core"),
        Some("coding-agent") => Ok("pi-coding-agent"),
        _ => Err(format!("unexpected upstream test file {file}")),
    }
}

fn derived_target(test: &InventoryTest, collision: usize) -> CheckResult<String> {
    let (_, relative) = test
        .file
        .split_once("/test/")
        .ok_or("upstream test path has no /test/ component")?;
    let relative = relative
        .strip_suffix(".test.ts")
        .or_else(|| relative.strip_suffix(".spec.ts"))
        .ok_or("upstream test path must end in .test.ts or .spec.ts")?;
    let mut components = vec![crate_name(&test.file)?.to_string(), "upstream".into()];
    components.extend(relative.split('/').map(snake));
    components.extend(test.ancestors.iter().map(|ancestor| snake(ancestor)));
    let mut title = snake(&test.title);
    if collision > 1 {
        title.push_str(&format!("__{collision}"));
    }
    components.push(title);
    Ok(components.join("::"))
}

fn duplicate_ordinal(test: &InventoryTest) -> CheckResult<(String, usize)> {
    let mut chain = vec![test.file.as_str()];
    chain.extend(test.ancestors.iter().map(String::as_str));
    chain.push(&test.title);
    let base = chain.join(" > ");
    let ordinal = if test.id == base {
        Some(1)
    } else {
        test.id
            .strip_prefix(&format!("{base} #"))
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value >= 2)
    };
    ordinal
        .map(|ordinal| (base.clone(), ordinal))
        .ok_or_else(|| {
            format!(
                "inventory ID differs from title chain {base:?}: got {:?}",
                test.id
            )
        })
}

fn derived_targets(tests: &[InventoryTest]) -> CheckResult<BTreeMap<&str, String>> {
    let mut ordered = tests
        .iter()
        .map(|test| Ok((test, duplicate_ordinal(test)?)))
        .collect::<CheckResult<Vec<_>>>()?;
    // The serialized inventory is sorted lexically, where #10 precedes #2.
    // Derive collision suffixes in source-location then numeric duplicate order.
    ordered.sort_by(
        |(left, (left_base, left_ordinal)), (right, (right_base, right_ordinal))| {
            (&left.file, left.line, left.column, left_base, left_ordinal).cmp(&(
                &right.file,
                right.line,
                right.column,
                right_base,
                right_ordinal,
            ))
        },
    );
    let mut collisions = BTreeMap::new();
    let mut output = BTreeMap::new();
    for (test, _) in ordered {
        let count = collisions.entry(derived_target(test, 1)?).or_insert(0);
        *count += 1;
        output.insert(test.id.as_str(), derived_target(test, *count)?);
    }
    Ok(output)
}

fn test_attribute(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let path = attribute.path();
        path.is_ident("test")
            || (path.segments.len() == 2
                && path.segments[0].ident == "tokio"
                && path.segments[1].ident == "test")
    })
}

fn ignored(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("ignore")
            || (attribute.path().is_ident("cfg_attr")
                && match &attribute.meta {
                    Meta::List(list) => list
                        .tokens
                        .to_string()
                        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        .any(|token| token == "ignore"),
                    _ => false,
                })
    })
}

fn unconditional(attributes: &[Attribute], location: &str) -> CheckResult {
    if attributes
        .iter()
        .any(|attribute| attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr"))
    {
        return Err(format!(
            "conditional Rust test or module {location} cannot satisfy upstream coverage; cfg and cfg_attr require explicit platform coverage support"
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct RustTest {
    ignored: bool,
}

/// Follow Rust's module declarations from the actual integration-test root.
/// Looking for the function's spelling in arbitrary source is insufficient:
/// comments, calls, and unlinked files must not satisfy a translated test.
fn rust_tests(agent: &Path) -> CheckResult<BTreeMap<String, RustTest>> {
    fn visit(
        path: &Path,
        module_directory: &Path,
        prefix: &[String],
        output: &mut BTreeMap<String, RustTest>,
        visited: &mut BTreeSet<PathBuf>,
    ) -> CheckResult {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !visited.insert(canonical) {
            return Err(format!("repeated Rust test module {}", path.display()));
        }
        let file = syn::parse_file(&read(path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        unconditional(&file.attrs, &path.display().to_string())?;
        visit_items(
            &file.items,
            module_directory,
            path.parent().expect("Rust module has a parent"),
            prefix,
            output,
            visited,
        )
    }
    fn visit_items(
        items: &[Item],
        directory: &Path,
        path_directory: &Path,
        prefix: &[String],
        output: &mut BTreeMap<String, RustTest>,
        visited: &mut BTreeSet<PathBuf>,
    ) -> CheckResult {
        for item in items {
            match item {
                Item::Fn(function) if test_attribute(&function.attrs) => {
                    let mut name = prefix.to_vec();
                    name.push(function.sig.ident.to_string());
                    let name = name.join("::");
                    unconditional(&function.attrs, &name)?;
                    if output
                        .insert(
                            name.clone(),
                            RustTest {
                                ignored: ignored(&function.attrs),
                            },
                        )
                        .is_some()
                    {
                        return Err(format!("duplicate Rust test path {name}"));
                    }
                }
                Item::Mod(module) => {
                    let mut name = prefix.to_vec();
                    name.push(module.ident.to_string());
                    unconditional(&module.attrs, &name.join("::"))?;
                    let nested_directory = directory.join(module.ident.to_string());
                    if let Some((_, items)) = &module.content {
                        visit_items(
                            items,
                            &nested_directory,
                            &nested_directory,
                            &name,
                            output,
                            visited,
                        )?;
                    } else {
                        let override_path = module
                            .attrs
                            .iter()
                            .find(|attribute| attribute.path().is_ident("path"))
                            .and_then(|attribute| match &attribute.meta {
                                Meta::NameValue(value) => match &value.value {
                                    syn::Expr::Lit(expression) => match &expression.lit {
                                        syn::Lit::Str(value) => Some(value.value()),
                                        _ => None,
                                    },
                                    _ => None,
                                },
                                _ => None,
                            });
                        let overridden = override_path.is_some();
                        let path = if let Some(path) = override_path {
                            safe_relative(&path)?;
                            // A path attribute in an out-of-line module is
                            // relative to the source file, even for foo.rs
                            // whose ordinary children live in foo/.
                            path_directory.join(path)
                        } else if directory.join(format!("{}.rs", module.ident)).exists() {
                            directory.join(format!("{}.rs", module.ident))
                        } else {
                            nested_directory.join("mod.rs")
                        };
                        let child_directory = if overridden {
                            // Rust resolves descendants of #[path] modules
                            // relative to the selected file's parent directory.
                            path.parent().expect("joined module path has a parent")
                        } else {
                            &nested_directory
                        };
                        visit(&path, child_directory, &name, output, visited)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    for name in PI_CRATES.into_iter().take(4) {
        let directory = agent.join("crates").join(name).join("tests");
        let root = directory.join("main.rs");
        if root.exists() {
            visit(
                &root,
                &directory,
                &[name.to_string()],
                &mut output,
                &mut BTreeSet::new(),
            )?;
        }
    }
    Ok(output)
}

const REASONS: [&str; 8] = [
    "live-provider",
    "skip-module",
    "replace-module",
    "later-module",
    "ts-runtime",
    "test-infra",
    "platform-process",
    "language",
];
const PHASE_TWO: [&str; 2] = [
    "packages/ai/test/openai-completions-retry.test.ts",
    "packages/coding-agent/test/suite/agent-session-mcp.test.ts",
];

fn check_phase(entry: &Mapping) -> CheckResult {
    let deferred_file = PHASE_TWO.contains(&entry.file.as_str());
    let deferred_mapping = entry.phase == Some(2)
        && entry.status == Status::Excluded
        && entry.reason.as_deref() == Some("replace-module");
    if (deferred_file && !deferred_mapping) || (entry.phase.is_some() && !deferred_file) {
        return Err(format!(
            "invalid or missing phase-2 exclusion for {}",
            entry.id
        ));
    }
    Ok(())
}

pub fn check(reference: &Path, agent: &Path) -> CheckResult<CoverageSummary> {
    let inventory: Inventory = json(&reference.join("corpus/upstream/inventory.json"))?;
    let mapping: Map = toml(&reference.join("coverage/upstream-map.toml"))?;
    let pin: serde_json::Value = json(&reference.join("pin.json"))?;
    if inventory.schema_version != 1
        || pin["tag"] != inventory.pin.tag
        || pin["rev"] != inventory.pin.rev
        || inventory.pin.tag != mapping.meta.pin
        || inventory.pin.rev != mapping.meta.revision
    {
        return Err("inventory, coverage map, and reference pin must agree".into());
    }
    let mut inventory_files = BTreeMap::new();
    for file in &inventory.files {
        safe_relative(&file.path)?;
        if !check_hash(&file.sha256)
            || inventory_files
                .insert(file.path.as_str(), file.sha256.as_str())
                .is_some()
        {
            return Err(format!("duplicate or invalid inventory file {}", file.path));
        }
    }
    let mut mapped_files = BTreeMap::new();
    for file in &mapping.file {
        if mapped_files
            .insert(file.path.as_str(), file.sha256.as_str())
            .is_some()
        {
            return Err(format!("duplicate coverage file {}", file.path));
        }
    }
    if inventory_files != mapped_files {
        let changed: Vec<_> = inventory
            .tests
            .iter()
            .filter(|test| {
                inventory_files.get(test.file.as_str()) != mapped_files.get(test.file.as_str())
            })
            .map(|test| &test.id)
            .collect();
        return Err(format!(
            "upstream file hashes changed or coverage file inventory is incomplete; review these test IDs: {changed:?}"
        ));
    }
    let mut mapped = BTreeMap::new();
    for entry in &mapping.test {
        if mapped.insert(entry.id.as_str(), entry).is_some() {
            return Err(format!("multiple coverage mappings for {}", entry.id));
        }
    }
    let mut seen = BTreeSet::new();
    let targets = derived_targets(&inventory.tests)?;
    let mut duplicates: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let rust = rust_tests(agent)?;
    let mut rust_targets = BTreeSet::new();
    let mut pending_targets = BTreeSet::new();
    let mut summary = CoverageSummary::default();
    for test in &inventory.tests {
        if !seen.insert(test.id.as_str())
            || !inventory_files.contains_key(test.file.as_str())
            || test.line == 0
            || test.column == 0
            || test.mode.is_empty()
            || test.status.is_empty()
        {
            return Err(format!("invalid or duplicate inventory test {}", test.id));
        }
        let (base_id, ordinal) = duplicate_ordinal(test)?;
        duplicates.entry(base_id).or_default().insert(ordinal);
        let entry = mapped
            .get(test.id.as_str())
            .ok_or_else(|| format!("unmapped upstream test {}", test.id))?;
        if entry.file != test.file || entry.line != test.line {
            return Err(format!("coverage location differs for {}", test.id));
        }
        if let Some(reason) = &entry.reason
            && !REASONS.contains(&reason.as_str())
        {
            return Err(format!(
                "unknown exclusion reason {reason:?} for {}",
                test.id
            ));
        }
        check_phase(entry)?;
        let target = entry
            .rust
            .clone()
            .unwrap_or_else(|| targets[test.id.as_str()].clone());
        for scenario in &entry.corpus {
            safe_relative(scenario)?;
            if !reference
                .join("corpus/scenarios")
                .join(scenario)
                .join("scenario.json")
                .is_file()
            {
                return Err(format!(
                    "{} references missing recorded scenario {scenario}",
                    test.id
                ));
            }
        }
        match entry.status {
            Status::Ported | Status::Adapted => {
                if entry.status == Status::Adapted
                    && entry
                        .adaptation
                        .as_deref()
                        .is_none_or(|value| value.trim().is_empty())
                {
                    return Err(format!("adapted test needs an adaptation: {}", test.id));
                }
                if !rust_targets.insert(target.clone()) {
                    return Err(format!(
                        "multiple upstream tests resolve to {target}; use distinct tests"
                    ));
                }
                let function = rust.get(&target).ok_or_else(|| {
                    format!("{} has no compiled #[test] function at {target}", test.id)
                })?;
                if function.ignored {
                    return Err(format!("translated test {target} must not be ignored"));
                }
                summary.translated_tests += 1;
            }
            Status::Corpus => {
                if entry.corpus.is_empty() {
                    return Err(format!(
                        "corpus coverage needs a recorded scenario: {}",
                        test.id
                    ));
                }
                summary.corpus_tests += 1;
            }
            Status::Excluded => {
                if entry.reason.is_none() {
                    return Err(format!("excluded test needs a reason: {}", test.id));
                }
                summary.excluded_tests += 1;
            }
            Status::Pending => {
                pending_targets.insert(target);
                summary.pending_tests += 1;
            }
        }
        summary.inventory_tests += 1;
    }
    let obsolete: Vec<_> = mapped.keys().filter(|id| !seen.contains(**id)).collect();
    if !obsolete.is_empty() {
        return Err(format!(
            "coverage contains IDs absent from the pinned inventory: {obsolete:?}"
        ));
    }
    for (target, function) in &rust {
        if function.ignored && !pending_targets.contains(target) {
            return Err(format!(
                "ignored Rust test {target} has no pending upstream mapping"
            ));
        }
    }
    for (id, ordinals) in duplicates {
        if ordinals.iter().copied().ne(1..=ordinals.len()) {
            return Err(format!(
                "inventory duplicate suffixes are not contiguous for {id}"
            ));
        }
    }
    check_results(reference, &inventory, &inventory_files)?;
    Ok(summary)
}

fn check_results(
    reference: &Path,
    inventory: &Inventory,
    inventory_files: &BTreeMap<&str, &str>,
) -> CheckResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Results {
        schema_version: u32,
        files: Vec<InventoryFile>,
        tests: Vec<ResultEntry>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ResultEntry {
        id: String,
        status: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct SelectedTests {
        schema_version: u32,
        files: Vec<SelectedTestFile>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SelectedTestFile {
        path: String,
        classification: String,
        #[allow(dead_code)]
        note: String,
    }
    let results: Results = json(&reference.join("corpus/upstream/results.json"))?;
    if results.schema_version != 1 || results.files.is_empty() || results.tests.is_empty() {
        return Err("upstream results must contain selected files and tests".into());
    }
    let mut seen = BTreeSet::new();
    let inventory_ids: BTreeSet<_> = inventory
        .tests
        .iter()
        .map(|test| test.id.as_str())
        .collect();
    let mut executed_files = BTreeSet::new();
    for file in results.files {
        if inventory_files.get(file.path.as_str()).copied() != Some(file.sha256.as_str())
            || !executed_files.insert(file.path.clone())
        {
            return Err(format!(
                "unknown, changed, or duplicate executed upstream file {}",
                file.path
            ));
        }
    }
    let selected: SelectedTests = json(&reference.join("selection/test-files.json"))?;
    if selected.schema_version != 1 {
        return Err("unsupported selected test-file schema".into());
    }
    let mut selected_files = BTreeSet::new();
    let mut expected_files = BTreeSet::new();
    for file in selected.files {
        if !inventory_files.contains_key(file.path.as_str())
            || !selected_files.insert(file.path.clone())
            || !matches!(file.classification.as_str(), "T" | "P" | "E" | "R" | "A")
        {
            return Err(format!(
                "unknown, duplicate, or invalid curated test file {}",
                file.path
            ));
        }
        if file.classification != "E" && !PHASE_TWO.contains(&file.path.as_str()) {
            expected_files.insert(file.path);
        }
    }
    if executed_files != expected_files {
        let missing: Vec<_> = expected_files.difference(&executed_files).collect();
        let extra: Vec<_> = executed_files.difference(&expected_files).collect();
        return Err(format!(
            "upstream result files differ from the curated execution set; missing: {missing:?}; extra: {extra:?}"
        ));
    }
    for result in results.tests {
        if !inventory_ids.contains(result.id.as_str())
            || !seen.insert(result.id.clone())
            || !matches!(
                result.status.as_str(),
                "passed" | "skipped" | "todo" | "pending"
            )
        {
            return Err(format!(
                "unknown, duplicate, or failed upstream result {}: {}",
                result.id, result.status
            ));
        }
    }
    for test in &inventory.tests {
        if executed_files.contains(&test.file) != seen.contains(&test.id) {
            return Err(format!(
                "upstream results do not exactly cover executed files: {}",
                test.id
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_matches_the_declared_port_rule() {
        assert_eq!(
            snake("AgentLoop with AgentMessage"),
            "agentloop_with_agentmessage"
        );
        assert_eq!(
            snake("8935-parallel-preflight-abort"),
            "r8935_parallel_preflight_abort"
        );
        assert_eq!(snake("spaces / punctuation..."), "spaces_punctuation");
    }

    #[test]
    fn only_test_attributes_qualify_functions() {
        let source = syn::parse_file("// fn a() {}\nfn a() {}\n#[test] fn b() { a(); }\n#[tokio::test(flavor = \"current_thread\")] async fn c() {}\n").unwrap();
        let names: Vec<_> = source
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(function) if test_attribute(&function.attrs) => {
                    Some(function.sig.ident.to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(names, ["b", "c"]);
    }

    #[test]
    fn disabled_functions_and_ancestor_modules_cannot_claim_coverage() {
        for source in [
            "#[test] #[cfg(any())] fn hidden() {}",
            "#[cfg(any())] mod hidden { #[test] fn test() {} }",
            "#[cfg_attr(all(), ignore)] #[test] fn hidden() {}",
            "#[cfg_attr(all(), cfg(any()))] mod hidden { #[test] fn test() {} }",
            "#![cfg(any())]\n#[test] fn hidden() {}",
        ] {
            let file = syn::parse_file(source).unwrap();
            let attributes = if file.attrs.is_empty() {
                match &file.items[0] {
                    Item::Fn(function) => &function.attrs,
                    Item::Mod(module) => &module.attrs,
                    _ => unreachable!(),
                }
            } else {
                &file.attrs
            };
            assert!(unconditional(attributes, "test").is_err(), "{source}");
        }
    }

    #[test]
    fn deferred_test_files_require_explicit_phase_two_metadata() {
        let mut entry = Mapping {
            id: "test".into(),
            file: PHASE_TWO[0].into(),
            line: 1,
            status: Status::Excluded,
            reason: Some("replace-module".into()),
            adaptation: None,
            phase: None,
            rust: None,
            corpus: Vec::new(),
            note: None,
        };
        assert!(check_phase(&entry).is_err());
        entry.phase = Some(2);
        assert!(check_phase(&entry).is_ok());
        entry.phase = Some(3);
        assert!(check_phase(&entry).is_err());
        entry.phase = Some(2);
        entry.file = "packages/ai/test/validation.test.ts".into();
        assert!(check_phase(&entry).is_err());
    }

    #[test]
    fn duplicate_suffixes_are_numeric_after_inventory_sorting() {
        let base = "packages/ai/test/example.test.ts > repeat";
        let mut tests: Vec<_> = (1..=12)
            .map(|number| InventoryTest {
                id: if number == 1 {
                    base.into()
                } else {
                    format!("{base} #{number}")
                },
                file: "packages/ai/test/example.test.ts".into(),
                ancestors: Vec::new(),
                title: "repeat".into(),
                line: 1,
                column: 1,
                mode: "run".into(),
                status: "pending".into(),
            })
            .collect();
        tests.sort_by(|left, right| left.id.cmp(&right.id));
        let targets = derived_targets(&tests).unwrap();
        assert!(targets[format!("{base} #2").as_str()].ends_with("::repeat__2"));
        assert!(targets[format!("{base} #10").as_str()].ends_with("::repeat__10"));
    }

    #[test]
    fn walker_follows_path_modules_and_rejects_disabled_ancestors() {
        let temporary = tempfile::tempdir().unwrap();
        let tests = temporary.path().join("crates/pi-ai/tests");
        std::fs::create_dir_all(tests.join("upstream/helpers")).unwrap();
        std::fs::write(tests.join("main.rs"), "mod upstream;").unwrap();
        std::fs::write(
            tests.join("upstream/mod.rs"),
            "#[path=\"helpers/custom.rs\"] mod renamed;",
        )
        .unwrap();
        std::fs::write(tests.join("upstream/helpers/custom.rs"), "mod nested;").unwrap();
        std::fs::write(
            tests.join("upstream/helpers/nested.rs"),
            "// #[test] fn fake() {}\nfn helper() {}\n#[test] fn real() {helper();}",
        )
        .unwrap();
        let found = rust_tests(temporary.path()).unwrap();
        assert_eq!(
            found.keys().map(String::as_str).collect::<Vec<_>>(),
            ["pi-ai::upstream::renamed::nested::real"]
        );
        std::fs::write(tests.join("main.rs"), "#[cfg(any())] mod upstream;").unwrap();
        assert!(
            rust_tests(temporary.path())
                .unwrap_err()
                .contains("conditional")
        );
    }

    #[test]
    fn path_attributes_in_non_mod_files_use_the_source_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let tests = temporary.path().join("crates/pi-ai/tests");
        std::fs::create_dir_all(tests.join("upstream/inline")).unwrap();
        std::fs::write(tests.join("main.rs"), "mod upstream;").unwrap();
        std::fs::write(
            tests.join("upstream.rs"),
            "#[path=\"upstream/custom.rs\"] mod renamed; mod ordinary; \
             mod inline { #[path=\"other.rs\"] mod child; }",
        )
        .unwrap();
        for path in [
            "upstream/custom.rs",
            "upstream/ordinary.rs",
            "upstream/inline/other.rs",
        ] {
            std::fs::write(tests.join(path), "#[test] fn real() {}").unwrap();
        }
        let found = rust_tests(temporary.path()).unwrap();
        assert_eq!(
            found.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "pi-ai::upstream::inline::child::real",
                "pi-ai::upstream::ordinary::real",
                "pi-ai::upstream::renamed::real",
            ]
        );
    }
}
