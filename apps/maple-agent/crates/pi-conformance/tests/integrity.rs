use pi_conformance::{
    agent_root, coverage, dependencies, integrity, reference_root, replay, selection,
};

#[test]
fn committed_corpus_matches_all_recording_inputs_and_outputs() {
    integrity::check(&reference_root()).unwrap();
}

#[test]
fn every_upstream_test_is_accounted_for() {
    let summary = coverage::check(&reference_root(), &agent_root()).unwrap();
    eprintln!("Upstream coverage: {summary:?}");
    if std::env::var_os("PI_PORT_GATE").is_some() {
        assert_eq!(
            summary.pending_tests, 0,
            "the final port gate requires zero pending upstream tests"
        );
    }
}

#[test]
fn pi_crates_have_no_maple_or_external_path_dependencies() {
    dependencies::check(&agent_root()).unwrap();
}

#[test]
fn source_selection_and_approved_deviations_are_consistent() {
    let complete = std::env::var_os("PI_PORT_GATE").is_some();
    let pending = selection::check_sources(&reference_root(), &agent_root(), complete).unwrap();
    eprintln!("Selected source modules still pending: {pending}");
    selection::check_deviations(&reference_root()).unwrap();
}

#[test]
fn recorded_scenarios_are_strict_and_accounted_for() {
    let root = reference_root();
    replay::check_structure(&root).unwrap();
    if std::env::var_os("PI_PORT_GATE").is_some() {
        let status = replay::status(&root).unwrap();
        let pending: Vec<_> = status
            .scenario
            .iter()
            .chain(&status.functions)
            .filter(|entry| entry.status == replay::Progress::Pending)
            .map(|entry| &entry.id)
            .collect();
        assert!(
            pending.is_empty(),
            "the final port gate requires zero pending corpus entries: {pending:?}"
        );
    }
}
