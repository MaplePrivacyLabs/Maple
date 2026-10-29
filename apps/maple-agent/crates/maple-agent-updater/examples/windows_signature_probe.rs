//! Manual, read-only historical Authenticode interoperability probe. The selected
//! executable is never launched; this is not installed-update qualification.

#[cfg(windows)]
use maple_agent_updater::installer::InstallError;
#[cfg(windows)]
type Result<T> = std::result::Result<T, InstallError>;

#[cfg(windows)]
#[allow(dead_code)]
#[path = "../src/installer/windows_bundle/authenticode.rs"]
mod authenticode;

#[cfg(windows)]
fn main() {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    assert_eq!(
        arguments.len(),
        2,
        "expected exact reference path and SHA256"
    );
    let path = std::path::Path::new(&arguments[0]);
    let expected = arguments[1].to_str().expect("expected SHA256");
    assert!(expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() <= 64 * 1024 * 1024,
        "reference PE exceeds bound"
    );
    assert_eq!(hex::encode(Sha256::digest(&bytes)), expected);
    assert!(
        authenticode::verify(path),
        "historical Maple admission signature was rejected"
    );
    assert!(
        authenticode::verify_startup_or_unsigned(path, false),
        "historical Maple startup signature was rejected"
    );
    println!(
        "{}",
        serde_json::json!({
            "schema": "maple-windows-historical-signature-probe-v1",
            "reference_sha256": expected,
            "admission_signature_valid": true,
            "startup_signature_valid": true,
            "historical_signature_interoperability": true,
            "new_updater_lifecycle_qualified": false,
            "final_source_release_qualified": false,
        })
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The historical signature probe requires native Windows.");
    std::process::exit(2);
}
