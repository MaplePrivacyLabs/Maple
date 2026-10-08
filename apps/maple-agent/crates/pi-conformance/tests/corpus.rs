use libtest_mimic::{Arguments, Trial};
use pi_conformance::{integrity, reference_root, replay};

fn main() {
    let args = Arguments::from_args();
    let root = reference_root();
    // A pending interpreter can suppress a behavioral mismatch, never corrupt
    // or stale recording inputs (including when only this test binary runs).
    integrity::check(&root).expect("corpus hashes must be current");
    replay::check_structure(&root).expect("recorded DSL and outputs must be valid");
    let status = replay::status(&root).expect("corpus status must be valid");
    let tests = status
        .scenario
        .into_iter()
        .map(|entry| {
            let root = root.clone();
            Trial::test(entry.id.clone(), move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                let result = runtime.block_on(replay::replay_scenario(&root, &entry.id));
                replay::check_progress(&entry, result).map_err(Into::into)
            })
        })
        .collect();
    libtest_mimic::run(&args, tests).exit();
}
