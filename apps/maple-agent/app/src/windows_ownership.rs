//! Keep new per-user objects private even when an elevated token defaults to
//! Administrators ownership. Normalize the current process token before normal
//! startup; subsequently spawned children inherit that ownership context.
//! Existing objects, the token's DACL, privileges and elevation are untouched.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::ConvertSidToStringSidW, EqualSid, GetTokenInformation, SetTokenInformation,
        TOKEN_ADJUST_DEFAULT, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
        TokenOwner, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn process_token(access: u32) -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn information(token: &OwnedHandle, kind: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), kind, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            kind,
            storage.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

/// Call during single-threaded normal app startup, before libraries such as
/// SQLite create files without an explicit security descriptor.
pub(crate) fn prepare() -> io::Result<()> {
    let query = process_token(TOKEN_QUERY)?;
    let user = information(&query, TokenUser)?;
    let owner = information(&query, TokenOwner)?;
    // Both buffers are aligned and own the SIDs returned by GetTokenInformation.
    let user_sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let owner_sid = unsafe { (*owner.as_ptr().cast::<TOKEN_OWNER>()).Owner };
    if unsafe { EqualSid(user_sid, owner_sid) } != 0 {
        // Ordinary tokens need only query access, not TOKEN_ADJUST_DEFAULT.
        return Ok(());
    }
    let writable = process_token(TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)?;
    let desired = TOKEN_OWNER { Owner: user_sid };
    if unsafe {
        SetTokenInformation(
            writable.as_raw_handle(),
            TokenOwner,
            (&desired as *const TOKEN_OWNER).cast(),
            size_of::<TOKEN_OWNER>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let owner = information(&writable, TokenOwner)?;
    if unsafe { EqualSid(user_sid, (*owner.as_ptr().cast::<TOKEN_OWNER>()).Owner) } == 0 {
        return Err(io::Error::other(
            "Windows did not select the current user as object owner",
        ));
    }
    Ok(())
}

fn identity(kind: TOKEN_INFORMATION_CLASS) -> io::Result<String> {
    let token = process_token(TOKEN_QUERY)?;
    let storage = information(&token, kind)?;
    let sid = unsafe {
        if kind == TokenUser {
            (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid
        } else if kind == TokenOwner {
            (*storage.as_ptr().cast::<TOKEN_OWNER>()).Owner
        } else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
    };
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

pub(crate) fn user_identity() -> io::Result<String> {
    identity(TokenUser)
}

#[cfg(test)]
pub(crate) fn default_owner_identity() -> io::Result<String> {
    identity(TokenOwner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::{Store, StoreError, StorePaths, StoreState};
    use std::{path::Path, process::Command};
    use windows_sys::Win32::Security::{TOKEN_STATISTICS, TokenStatistics};

    const FIXTURE: &str = "windows_ownership::tests::owner_process_fixture";
    const ROOT: &str = "MAPLE_TEST_WINDOWS_OWNER_ROOT";
    const PARENT_TOKEN: &str = "MAPLE_TEST_WINDOWS_OWNER_PARENT_TOKEN";

    fn token_id() -> String {
        let token = process_token(TOKEN_QUERY).unwrap();
        let storage = information(&token, TokenStatistics).unwrap();
        let stats = unsafe { &*storage.as_ptr().cast::<TOKEN_STATISTICS>() };
        format!(
            "{:08x}{:08x}",
            stats.TokenId.HighPart as u32, stats.TokenId.LowPart
        )
    }

    fn snapshot(directory: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap())
            // The WAL index is reconstructible and can be memory-mapped.
            .filter(|entry| entry.file_name() != "maple.db-shm")
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    #[test]
    fn startup_owns_new_store_files_without_changing_the_parent_or_adopting_old_files() {
        let fixture = tempfile::Builder::new()
            .prefix("maple-windows-owner-")
            .tempdir()
            .unwrap();
        let before_owner = default_owner_identity().unwrap();
        let before_token = token_id();
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--ignored", "--nocapture"])
            .env(ROOT, fixture.path())
            .env(PARENT_TOKEN, &before_token)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "owner fixture failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stdout)
                .contains("owned store create/reopen and old-store refusal verified")
        );
        assert_eq!(default_owner_identity().unwrap(), before_owner);
        assert_eq!(token_id(), before_token);
    }

    #[test]
    #[ignore = "run in an isolated process by the startup ownership regression"]
    fn owner_process_fixture() {
        let root = std::path::PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("maple-windows-owner-")
        );
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        assert_ne!(
            token_id(),
            std::env::var(PARENT_TOKEN).unwrap(),
            "do not modify a token shared with the parent"
        );
        let user = user_identity().unwrap();
        let stale = root.join("old-default-owner");
        let old_snapshot = if default_owner_identity().unwrap() != user {
            assert!(matches!(
                Store::open(StorePaths::new(&stale)),
                Err(StoreError::UnsafePath)
            ));
            Some(snapshot(&stale.join("store-v1/db")))
        } else {
            None
        };

        prepare().unwrap();
        prepare().unwrap(); // Already-correct ownership is an idempotent query.
        assert_eq!(default_owner_identity().unwrap(), user);
        assert_eq!(user_identity().unwrap(), user);
        let fresh = root.join("fresh");
        for _ in 0..2 {
            let store = Store::open(StorePaths::new(&fresh)).unwrap();
            assert_eq!(store.state(), StoreState::Open);
            store.upsert_account("ownership-fixture", 1).unwrap();
            store.close_for_shutdown().unwrap();
        }
        if let Some(before) = old_snapshot {
            assert!(matches!(
                Store::open(StorePaths::new(&stale)),
                Err(StoreError::UnsafePath)
            ));
            assert_eq!(snapshot(&stale.join("store-v1/db")), before);
        }
        println!("owned store create/reopen and old-store refusal verified");
    }
}
