//! Windows packages carry the python tool's CPython inside the executable.
//! `MAPLE_CODE_MODE_EMBEDDED_RUNTIME` names the archive that
//! `scripts/prepare-python.py --archive` packed; without it nothing is embedded.

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(maple_embedded_python)");
    println!("cargo:rerun-if-env-changed=MAPLE_CODE_MODE_EMBEDDED_RUNTIME");
    let Some(archive) =
        std::env::var_os("MAPLE_CODE_MODE_EMBEDDED_RUNTIME").filter(|path| !path.is_empty())
    else {
        return;
    };
    // Not canonicalized: on Windows that yields a verbatim `\\?\` path.
    let archive = std::path::absolute(&archive)
        .unwrap_or_else(|error| panic!("MAPLE_CODE_MODE_EMBEDDED_RUNTIME {archive:?}: {error}"));
    println!("cargo:rerun-if-changed={}", archive.display());
    let bytes = std::fs::read(&archive)
        .unwrap_or_else(|error| panic!("read {}: {error}", archive.display()));
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("cargo:rustc-cfg=maple_embedded_python");
    println!(
        "cargo:rustc-env=MAPLE_CODE_MODE_EMBEDDED_RUNTIME_ARCHIVE={}",
        archive.display()
    );
    println!("cargo:rustc-env=MAPLE_CODE_MODE_EMBEDDED_RUNTIME_SHA256={digest}");
}
