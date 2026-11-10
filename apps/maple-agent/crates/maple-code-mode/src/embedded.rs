//! A CPython runtime carried inside the executable and unpacked on first use.
//!
//! Windows packages deliver the runtime this way. A Windows generation holds
//! exactly four files, and the launcher of every existing installation refuses
//! one with anything else, so the runtime cannot sit beside the executable as
//! it does on macOS and Linux. Inside the executable it is covered by the
//! executable's signature and arrives with installs and in-app updates alike.

use crate::{Error, PackagedPython};
use std::{
    fs,
    io::{self, Read},
    path::{Component, Path},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const RECEIPT: &str = ".unpacked";
const LOCK: &str = ".lock";
const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const WARM_UP_LIMIT: Duration = Duration::from_secs(120);
/// The standard-library modules the worker imports at startup.
const WARM_UP: &str = "import ast, asyncio, builtins, codecs, collections, contextvars, inspect, io, \
     json, linecache, os, platform, signal, struct, sys, threading, time, types";

/// The archive `scripts/prepare-python.py --archive` packed, and its SHA-256.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedPython {
    archive: &'static [u8],
    sha256: &'static str,
}

impl EmbeddedPython {
    /// The runtime compiled into this executable, if it has one.
    pub fn bundled() -> Option<Self> {
        #[cfg(maple_embedded_python)]
        {
            // Packaging finds this marker in the executable to confirm that
            // a runtime was embedded (scripts/windows-release.py).
            const MARKER: &str = "maple-embedded-python-sha256=";
            const MARKED: &str = concat!(
                "maple-embedded-python-sha256=",
                env!("MAPLE_CODE_MODE_EMBEDDED_RUNTIME_SHA256")
            );
            Some(Self {
                archive: include_bytes!(env!("MAPLE_CODE_MODE_EMBEDDED_RUNTIME_ARCHIVE")),
                sha256: &MARKED[MARKER.len()..],
            })
        }
        #[cfg(not(maple_embedded_python))]
        {
            None
        }
    }

    /// `sha256` is the lowercase hex SHA-256 of `archive`. It names the
    /// unpacked copy, so two different archives never share a directory.
    pub fn new(archive: &'static [u8], sha256: &'static str) -> Self {
        assert!(
            sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "an embedded Python archive is named by its SHA-256"
        );
        Self { archive, sha256 }
    }

    /// The runtime under `root`, unpacked first unless a complete copy of
    /// this archive is already there. `root` must exist, and only this user
    /// may be able to write to it: the interpreter runs from it.
    ///
    /// Blocks while it writes about 2,000 files. A copy that was interrupted
    /// or has lost a file is unpacked again. Copies of other archives, left by
    /// earlier versions, are removed.
    pub fn unpack(&self, root: &Path) -> Result<PackagedPython, Error> {
        let failed = |action: &str, error: io::Error| {
            Error::Unavailable(format!(
                "Could not {action} the bundled Python under {}: {error}",
                root.display()
            ))
        };
        let name = &self.sha256[..16];
        let directory = root.join(name);
        let manifest = directory.join("runtime.json");
        // One unpacking at a time, across processes. The lock ends with the
        // file handle, so a crashed process never leaves it held.
        let lock = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(LOCK))
            .and_then(|file| file.lock().map(|()| file))
            .map_err(|error| failed("lock", error))?;
        if complete(&directory, self.sha256)
            && let Ok(python) = PackagedPython::from_manifest(&manifest)
        {
            return Ok(python);
        }
        remove_tree(&directory).map_err(|error| failed("replace", error))?;
        let files = extract(self.archive, &directory).map_err(|error| failed("unpack", error))?;
        // Written last: a copy without it is incomplete.
        fs::write(directory.join(RECEIPT), format!("{}\n{files}", self.sha256))
            .map_err(|error| failed("finish unpacking", error))?;
        let python = PackagedPython::from_manifest(&manifest)?;
        warm_up(&python);
        remove_other_copies(root, name);
        drop(lock);
        Ok(python)
    }
}

/// Whether the copy's receipt names this archive and every file it lists is
/// still there at its unpacked size. Contents are not read again: the check
/// runs before every worker start, and the folder is the user's own.
fn complete(directory: &Path, sha256: &str) -> bool {
    let Ok(receipt) = fs::read_to_string(directory.join(RECEIPT)) else {
        return false;
    };
    let mut lines = receipt.lines();
    lines.next() == Some(sha256)
        && lines.all(|line| {
            line.split_once('\t').is_some_and(|(size, name)| {
                let mut path = directory.to_path_buf();
                path.extend(Path::new(name).components());
                fs::symlink_metadata(path)
                    .is_ok_and(|file| file.is_file() && size.parse() == Ok(file.len()))
            })
        })
}

fn remove_tree(directory: &Path) -> io::Result<()> {
    // The receipt goes first, so a partly removed copy is never taken for a
    // complete one.
    for path in [directory.join(RECEIPT), directory.to_path_buf()] {
        let removed = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn remove_other_copies(root: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == current || name == LOCK || !entry.path().is_dir() {
            continue;
        }
        if let Err(error) = remove_tree(&entry.path()) {
            log::warn!(
                "Could not remove an earlier bundled Python at {}: {error}",
                entry.path().display()
            );
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Runs a new copy's interpreter once. The first start of files Windows has
/// never run is slow (they are paged in and scanned), and it would otherwise
/// count against the worker's startup deadline. Failure here is not an error:
/// the worker reports what is wrong with the runtime.
fn warm_up(python: &PackagedPython) {
    let mut command = Command::new(&python.executable);
    command
        .args(["-I", "-B", "-c", WARM_UP])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let deadline = Instant::now() + WARM_UP_LIMIT;
    while let Ok(None) = child.try_wait() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The archive is Maple's own, but it is still held to what the packer
/// writes: regular files under plain relative names.
/// Unpacks the archive and returns its files, one `size<TAB>name` line each.
fn extract(archive: &[u8], directory: &Path) -> io::Result<String> {
    fs::create_dir(directory)?;
    let mut files = String::new();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut entries = 0usize;
    let mut bytes = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries += 1;
        bytes = bytes.saturating_add(entry.size());
        if entries > MAX_ENTRIES || bytes > MAX_BYTES {
            return Err(invalid("the Python archive is larger than any runtime"));
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid(
                "the Python archive holds something other than a file",
            ));
        }
        let path = entry.path()?.into_owned();
        let plain = path
            .to_str()
            .is_some_and(|name| !name.is_empty() && !name.contains([':', '\\']))
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_)));
        if !plain {
            return Err(invalid("the Python archive holds an unsafe path"));
        }
        // One component at a time: a verbatim Windows path (`\\?\C:\…`), which
        // canonicalizing gives, takes the archive's `/` as part of a name.
        let mut target = directory.to_path_buf();
        target.extend(path.components());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::options()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let written = io::copy(&mut entry.by_ref().take(MAX_BYTES), &mut file)?;
        files.push_str(&format!("{written}\t{}\n", path.display()));
        #[cfg(unix)]
        if entry.header().mode()? & 0o111 != 0 {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PYTHON_VERSION, RuntimeManifest};

    fn packed(files: &[(&str, &[u8])]) -> &'static [u8] {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            // Written raw, so an unsafe name reaches the reader as it is.
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap().leak()
    }

    fn runtime(marker: &[u8]) -> EmbeddedPython {
        let manifest = serde_json::to_vec(&RuntimeManifest {
            protocol_version: 1,
            implementation: "cpython".into(),
            version: PYTHON_VERSION.into(),
            distribution: "pbs-fixture".into(),
            executable: "python.exe".into(),
            worker: "worker.py".into(),
        })
        .unwrap();
        // Stands in for the digest: the copy is named by its first characters.
        let sha256 = format!("{:0<64x}", marker.len() + 10).leak();
        EmbeddedPython::new(
            packed(&[
                ("runtime.json", &manifest),
                ("python.exe", b"MZ"),
                ("worker.py", b""),
                ("Lib/json/marker", marker),
            ]),
            sha256,
        )
    }

    #[test]
    fn unpacks_once_and_reuses_the_complete_copy() {
        let root = tempfile::tempdir().unwrap();
        let python = runtime(b"one").unpack(root.path()).unwrap();
        let directory = python.manifest.parent().unwrap().to_path_buf();
        assert_eq!(python.executable, directory.join("python.exe"));
        assert_eq!(fs::read(directory.join("Lib/json/marker")).unwrap(), b"one");
        // A later start must not write the files again.
        fs::write(directory.join("Lib/json/marker"), b"own").unwrap();
        assert_eq!(runtime(b"one").unpack(root.path()).unwrap(), python);
        assert_eq!(fs::read(directory.join("Lib/json/marker")).unwrap(), b"own");
    }

    #[test]
    fn unpacks_again_after_an_interruption_or_a_lost_file() {
        let root = tempfile::tempdir().unwrap();
        let python = runtime(b"one").unpack(root.path()).unwrap();
        let directory = python.manifest.parent().unwrap().to_path_buf();
        fs::remove_file(directory.join(RECEIPT)).unwrap();
        fs::write(directory.join("Lib/json/marker"), b"partial").unwrap();
        runtime(b"one").unpack(root.path()).unwrap();
        assert_eq!(fs::read(directory.join("Lib/json/marker")).unwrap(), b"one");
        fs::remove_file(directory.join("worker.py")).unwrap();
        runtime(b"one").unpack(root.path()).unwrap();
        assert!(directory.join("worker.py").is_file());
        // Any other file counts too, lost or cut short.
        fs::remove_file(directory.join("Lib/json/marker")).unwrap();
        runtime(b"one").unpack(root.path()).unwrap();
        fs::write(directory.join("Lib/json/marker"), b"").unwrap();
        runtime(b"one").unpack(root.path()).unwrap();
        assert_eq!(fs::read(directory.join("Lib/json/marker")).unwrap(), b"one");
    }

    #[test]
    fn a_new_archive_replaces_the_copies_of_earlier_ones() {
        let root = tempfile::tempdir().unwrap();
        let old = runtime(b"one").unpack(root.path()).unwrap();
        let new = runtime(b"three").unpack(root.path()).unwrap();
        assert_ne!(old.manifest, new.manifest);
        assert!(!old.manifest.parent().unwrap().exists());
        assert!(new.executable.is_file());
    }

    #[test]
    fn refuses_paths_outside_the_runtime_and_leaves_no_complete_copy() {
        for name in [
            "../escape",
            "/absolute",
            "Lib/../../escape",
            "python.exe:stream",
        ] {
            let root = tempfile::tempdir().unwrap();
            let inside = root.path().join("runtimes");
            fs::create_dir(&inside).unwrap();
            let embedded = EmbeddedPython::new(packed(&[(name, b"x")]), runtime(b"").sha256);
            let error = embedded.unpack(&inside).unwrap_err().to_string();
            assert!(error.contains("unsafe path"), "{name}: {error}");
            assert!(!root.path().join("escape").exists());
            assert!(!inside.join(&embedded.sha256[..16]).join(RECEIPT).exists());
        }
    }
}
