//! Windows packages carry the runtime inside the executable. Pack the staged
//! distribution the way `prepare-python.py --archive` does, unpack it as the
//! app would, and run the interpreter from the unpacked copy.
#![cfg(windows)]

use maple_code_mode::{EmbeddedPython, LaunchSpec, OutcomeStatus, Runtime};
use std::{fs, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn pack(directory: &Path, name: &str, builder: &mut tar::Builder<impl std::io::Write>) {
    let mut entries = fs::read_dir(directory)
        .expect("run `just python-prepare` first")
        .map(|entry| entry.unwrap())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let leaf = entry.file_name().into_string().unwrap();
        let name = if name.is_empty() {
            leaf
        } else {
            format!("{name}/{leaf}")
        };
        if entry.file_type().unwrap().is_dir() {
            pack(&entry.path(), &name, builder);
        } else if name != ".prepared.json" {
            let contents = fs::read(entry.path()).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, &name, contents.as_slice())
                .unwrap();
        }
    }
}

#[tokio::test]
async fn the_unpacked_runtime_runs_cells_with_its_native_modules() {
    let staged = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/runtime/python");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    pack(&staged, "", &mut builder);
    let archive: &'static [u8] = builder.into_inner().unwrap().finish().unwrap().leak();
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    // Stands in for the archive's digest, which only names the copy.
    let digest: &'static str = "5a".repeat(32).leak();
    let embedded = EmbeddedPython::new(archive, digest);
    let python = embedded.unpack(&root).unwrap();
    assert!(python.executable.starts_with(&root));
    assert_eq!(embedded.unpack(&root).unwrap(), python);

    let project = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(Default::default());
    let task = runtime
        .bind(
            "embedded",
            LaunchSpec {
                python: python.clone(),
                cwd: project.path().canonicalize().unwrap(),
                env: std::env::vars_os().collect(),
            },
            CancellationToken::new(),
        )
        .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        task.execute(
            "import ctypes, hashlib, sqlite3, ssl, sys, zlib\nsys.executable",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the unpacked interpreter did not answer")
    .expect("the unpacked interpreter did not start");
    assert_eq!(outcome.status, OutcomeStatus::Ok, "{outcome:?}");
    let reported = outcome.value.as_deref().unwrap_or_default().to_lowercase();
    let leaf = root.file_name().unwrap().to_str().unwrap().to_lowercase();
    assert!(reported.contains(&leaf), "{reported}");
    runtime.shutdown("test finished").await.unwrap();
}
