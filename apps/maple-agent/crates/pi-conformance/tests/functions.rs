use libtest_mimic::{Arguments, Trial};
use pi_conformance::{integrity, reference_root, replay};

fn main() {
    let args = Arguments::from_args();
    let root = reference_root();
    integrity::check(&root).expect("corpus hashes must be current");
    replay::check_structure(&root).expect("recorded DSL and outputs must be valid");
    let status = replay::status(&root).expect("corpus status must be valid");
    let tests = status
        .functions
        .into_iter()
        .map(|entry| {
            let root = root.clone();
            Trial::test(format!("functions/{}", entry.id), move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                let result = runtime.block_on(replay::replay_function(&root, &entry.id));
                replay::check_progress(&entry, result).map_err(Into::into)
            })
        })
        .collect();
    libtest_mimic::run(&args, tests).exit();
}
