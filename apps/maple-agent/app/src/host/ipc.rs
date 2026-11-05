//! OS-local endpoints and startup arbitration. No TCP listener or credentials.

use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
};
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type Connection = Box<dyn Stream>;

#[derive(Clone)]
pub struct Endpoint {
    pub profile: String,
    pub remote_host: Option<String>,
    directory: PathBuf,
    address: PathBuf,
    ownership: PathBuf,
    previous_directory: Option<PathBuf>,
}

impl Endpoint {
    #[cfg(test)]
    pub(crate) fn for_test(directory: PathBuf) -> Self {
        #[cfg(unix)]
        let address = directory.join("ipc.sock");
        #[cfg(windows)]
        let address = PathBuf::from(format!(r"\\.\pipe\maple-test-{}", uuid::Uuid::new_v4()));
        Self {
            profile: "test-profile".into(),
            remote_host: None,
            ownership: directory.join("owner.lock"),
            previous_directory: None,
            directory,
            address,
        }
    }
    pub fn new(api_url: &str) -> Result<Self, String> {
        Self::from_roots(
            api_url,
            crate::backend::instruction_config_root(),
            crate::backend::profile_data_root(),
        )
    }

    fn from_roots(api_url: &str, config: PathBuf, data: PathBuf) -> Result<Self, String> {
        let paths = maple_agent::store::StorePaths::new(&data)
            .prepare_control_root()
            .map_err(|e| e.to_string())?;
        let config_identity = canonical_identity_path(&config)?;
        // Store identity is independent of the instruction/config root. That
        // root remains part of the handshake profile to reject mismatched loads.
        let storage = serde_json::to_vec(&(
            paths.root(),
            crate::profile::DATA_NAMESPACE,
            os_user_identity()?,
        ))
        .map_err(|e| e.to_string())?;
        let namespace = format!("{:x}", Sha256::digest(&storage));
        let previous_storage = serde_json::to_vec(&(
            &config_identity,
            canonical_identity_path(&data)?,
            crate::profile::DATA_NAMESPACE,
            os_user_identity()?,
        ))
        .map_err(|e| e.to_string())?;
        let previous_namespace = format!("{:x}", Sha256::digest(previous_storage));
        let identity = serde_json::to_vec(&(
            namespace.as_str(),
            config_identity,
            (
                cfg!(feature = "desktop"),
                cfg!(feature = "acp"),
                cfg!(feature = "proxy"),
            ),
            api_url,
            crate::billing::configured_billing_api_url(),
            crate::backend::local::configured_client_id(),
            format!(
                "{:?}",
                maple_agent::open_secret_config::configured_pcr0_environment()?
            ),
        ))
        .map_err(|e| e.to_string())?;
        let profile = format!("{:x}", Sha256::digest(identity));
        #[cfg(unix)]
        let directory = {
            // Short even when XDG roots are long (macOS sun_path is 104 bytes).
            let uid = unsafe { libc::geteuid() };
            let parent = PathBuf::from(format!("/tmp/maple-agent-{uid}"));
            private_directory(&parent)?;
            parent.join(&namespace[..24])
        };
        #[cfg(windows)]
        let directory = paths.root().join("host");
        private_directory(&directory)?;
        #[cfg(unix)]
        let address = directory.join("ipc.sock");
        #[cfg(windows)]
        let address = PathBuf::from(format!(r"\\.\pipe\maple-agent-{namespace}"));
        #[cfg(unix)]
        let previous_directory = Some(PathBuf::from(format!(
            "/tmp/maple-agent-{}/{}",
            unsafe { libc::geteuid() },
            &previous_namespace[..24]
        )));
        #[cfg(windows)]
        let previous_directory = {
            let _ = previous_namespace;
            Some(data.join("host"))
        };
        Ok(Self {
            profile,
            remote_host: None,
            ownership: paths.owner_lock(),
            previous_directory,
            directory,
            address,
        })
    }

    pub fn ownership_lock(&self) -> Result<File, String> {
        open_lock(&self.ownership, true)
    }

    #[cfg(any(feature = "desktop", test))]
    pub(super) fn existing_ownership_lock(&self) -> Result<File, String> {
        let profile = self
            .ownership
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or("Cannot locate the existing Maple host owner lock")?;
        let paths = maple_agent::store::StorePaths::new(profile);
        if paths.owner_lock() != self.ownership {
            return Err("Unexpected Maple host owner lock location".into());
        }
        paths
            .open_existing_owner_lock()
            .map_err(|_| "Cannot verify the existing Maple host owner lock".into())
    }

    /// Control metadata only: never construct the previous backend or read
    /// its state. Holding a free existing lock also prevents an old owner from
    /// joining during this new host's lifetime.
    pub fn previous_owner_guard(&self) -> Result<Option<File>, String> {
        let Some(directory) = &self.previous_directory else {
            return Ok(None);
        };
        let lock_path = directory.join("host.lock");
        let file = match open_lock(&lock_path, false) {
            Ok(file) => file,
            Err(error) => match lock_path.try_exists() {
                Ok(false) => return Ok(None),
                // An unreadable control path is not proof of an absent host.
                Ok(true) | Err(_) => return Err(error),
            },
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(
                "A previous Maple host is running for this profile. Stop that exact host before opening this version; previous app data is untouched.".into()
            ),
            Err(error) => Err(format!("Cannot inspect previous host ownership: {error}")),
        }
    }

    pub fn repair_directory(&self) -> Result<(), String> {
        if let Some(parent) = self.directory.parent() {
            private_directory(parent)?;
        }
        private_directory(&self.directory)
    }

    pub fn lock(&self, name: &str) -> Result<File, String> {
        open_lock(&self.directory.join(name), true)
    }

    fn address(&self) -> &std::path::Path {
        &self.address
    }

    pub async fn connect(&self) -> Result<Connection, String> {
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(self.address())
                .await
                .map(|s| Box::new(s) as Connection)
                .map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
            // A concurrent client can take the available pipe instance before
            // accept creates the next one. Busy is not a missing host: wait
            // briefly without blocking the executor or starting another host.
            let stream = tokio::time::timeout(super::START_TIMEOUT, async {
                loop {
                    match tokio::net::windows::named_pipe::ClientOptions::new().open(self.address())
                    {
                        Ok(stream) => return Ok(stream),
                        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
            })
            .await
            .map_err(|_| "Maple host pipe remained busy".to_string())??;
            verify_pipe_owner(stream.as_raw_handle())?;
            Ok(Box::new(stream))
        }
    }

    pub fn bind(&self) -> Result<Listener, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, PermissionsExt};
            let path = self.address().to_path_buf();
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                if !metadata.file_type().is_socket() {
                    return Err("Host endpoint is not a socket".into());
                }
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            let listener = tokio::net::UnixListener::bind(&path).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            use std::os::unix::fs::MetadataExt;
            Ok(Listener {
                inner: listener,
                identity: (metadata.dev(), metadata.ino()),
                path,
            })
        }
        #[cfg(windows)]
        {
            Ok(Listener {
                inner: make_pipe(self.address(), true)?,
                path: self.address().to_path_buf(),
            })
        }
    }
}

fn open_lock(path: &std::path::Path, create: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options
        .create(create)
        .truncate(false)
        .read(true)
        .write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
        .open(path)
        .map_err(|e| format!("Cannot open host control lock: {e}"))
}

// Resolve an instruction-root identity without creating or reading app files.
fn canonical_identity_path(path: &std::path::Path) -> Result<PathBuf, String> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut base) => {
                for part in suffix.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or("Invalid profile root")?
                        .to_os_string(),
                );
                existing = existing.parent().ok_or("Invalid profile root")?;
            }
            Err(error) => return Err(format!("Cannot resolve profile identity: {error}")),
        }
    }
}

fn private_directory(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    };
    #[cfg(not(unix))]
    let result = std::fs::create_dir(path);
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("Cannot create host directory: {e}")),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Unsafe host directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Host directory must be private to this OS user".into());
        }
    }
    Ok(())
}

pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: tokio::net::windows::named_pipe::NamedPipeServer,
    path: PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl Listener {
    pub fn endpoint_present(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity)
        }
        #[cfg(windows)]
        {
            true
        }
    }

    pub async fn accept(&mut self) -> Result<Connection, String> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await.map_err(|e| e.to_string())?;
            let credentials = stream.peer_cred().map_err(|e| e.to_string())?;
            if credentials.uid() != unsafe { libc::geteuid() } {
                return Err("Host peer belongs to another OS user".into());
            }
            Ok(Box::new(stream))
        }
        #[cfg(windows)]
        {
            self.inner.connect().await.map_err(|e| e.to_string())?;
            let next = make_pipe(&self.path, false)?;
            Ok(Box::new(std::mem::replace(&mut self.inner, next)))
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if self.endpoint_present() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(windows)]
fn make_pipe(
    path: &std::path::Path,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, String> {
    // An elevated token's default owner may be Administrators, while clients
    // authenticate the server against TokenUser. Set that same owner at creation.
    let user = os_user_identity()?;
    make_pipe_with_sddl(path, first, &format!("O:{user}D:P(A;;GA;;;OW)(A;;GA;;;SY)"))
}

#[cfg(windows)]
fn make_pipe_with_sddl(
    path: &std::path::Path,
    first: bool,
    sddl: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
    };
    // Owner rights and SYSTEM only. Named-pipe defaults also grant Everyone
    // read access, so the default DACL is not sufficient for a control endpoint.
    let descriptor: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut security = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut security,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security,
        bInheritHandle: 0,
    };
    let result = unsafe {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(path, &attributes as *const _ as *mut _)
    };
    unsafe {
        LocalFree(security);
    }
    result.map_err(|e| e.to_string())
}

#[cfg(windows)]
fn verify_pipe_owner(pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<(), String> {
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr::null_mut,
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT},
            EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    // Verify the server as well as restricting clients: an unrelated OS user
    // must not pre-create our predictable pipe name and receive sign-in data.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut size = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
        if size == 0 {
            return Err("Cannot read local user identity".into());
        }
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        let status = GetSecurityInfo(
            pipe,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        );
        if status != 0 {
            return Err(format!("Cannot verify Maple host ownership ({status})"));
        }
        let matches = !owner.is_null() && EqualSid(owner, user.User.Sid) != 0;
        LocalFree(descriptor);
        if matches {
            Ok(())
        } else {
            Err("Maple host belongs to another OS user".into())
        }
    }
}

#[cfg(unix)]
fn os_user_identity() -> Result<String, String> {
    Ok(unsafe { libc::geteuid() }.to_string())
}

#[cfg(windows)]
fn os_user_identity() -> Result<String, String> {
    crate::windows_ownership::user_identity().map_err(|error| error.to_string())
}

#[cfg(all(windows, test))]
mod windows_pipe_tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn every_pipe_instance_is_owned_by_the_connecting_user() {
        let fixture = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_test(fixture.path().to_path_buf());
        let mut listener = endpoint.bind().unwrap();
        // The first-instance guard must still reject a second server.
        assert!(endpoint.bind().is_err());
        for _ in 0..3 {
            // Check the actual kernel object, including each replacement
            // instance created by accept, before any client is admitted.
            verify_pipe_owner(listener.inner.as_raw_handle()).unwrap();
            let (client, server) = tokio::join!(endpoint.connect(), listener.accept());
            let mut client = client.unwrap();
            let mut server = server.unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut message = [0; 4];
            server.read_exact(&mut message).await.unwrap();
            assert_eq!(&message, b"ping");
        }
        verify_pipe_owner(listener.inner.as_raw_handle()).unwrap();
    }

    #[tokio::test]
    async fn client_rejects_a_pipe_owned_by_the_tokens_other_default_owner() {
        // Elevated tokens can have an Administrators default owner even
        // though TokenUser is the interactive user. Exercise the real client
        // rejection wherever that token context is available.
        if crate::windows_ownership::default_owner_identity().unwrap()
            == os_user_identity().unwrap()
        {
            eprintln!("TokenOwner equals TokenUser; no different-owner fixture in this context");
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_test(fixture.path().to_path_buf());
        let _server =
            make_pipe_with_sddl(endpoint.address(), true, "D:P(A;;GA;;;OW)(A;;GA;;;SY)").unwrap();
        let error = match endpoint.connect().await {
            Ok(_) => panic!("admitted a pipe belonging to a different owner"),
            Err(error) => error,
        };
        assert_eq!(error, "Maple host belongs to another OS user");
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn config_roots_share_the_new_endpoint_but_not_the_handshake_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let data = fixture.path().join("profile-data");
        let first_config = fixture.path().join("first-config");
        let second_config = fixture.path().join("second-config");
        let first =
            Endpoint::from_roots("http://127.0.0.1:1", first_config.clone(), data.clone()).unwrap();
        let second =
            Endpoint::from_roots("http://127.0.0.1:1", second_config.clone(), data.clone())
                .unwrap();
        assert_eq!(first.address, second.address);
        assert_eq!(first.ownership, second.ownership);
        assert_ne!(first.profile, second.profile);
        assert_eq!(
            first.ownership,
            data.canonicalize().unwrap().join("store-v1/owner.lock")
        );
        assert!(!first_config.exists());
        assert!(!second_config.exists());
        assert!(!data.join("auth.json").exists());
        std::fs::remove_dir_all(&first.directory).unwrap();
    }

    #[test]
    fn concurrent_endpoint_resolution_leaves_the_owner_rename_gap_untouched() {
        use maple_agent::store::{Store, StorePaths, StoreState};
        use std::sync::mpsc;
        use std::time::Duration;

        let fixture = tempfile::tempdir().unwrap();
        let data = fixture.path().join("profile-data");
        let config = fixture.path().join("config");
        let endpoint =
            Endpoint::from_roots("http://127.0.0.1:1", config.clone(), data.clone()).unwrap();
        let ownership = endpoint.ownership_lock().unwrap();
        ownership.try_lock().unwrap();
        let store = Store::open(StorePaths::new(&data)).unwrap();
        let paths = store.paths().clone();
        let generation = store.store_id().unwrap();
        store.upsert_account("rename-gap-fixture", 1).unwrap();
        store.close_for_shutdown().unwrap();
        drop(store);

        // Stage only this synthetic owner's verified new-store files. The
        // replacement keeps the same identity, as an encryption export does.
        let staged = paths.root().join("db.new");
        std::fs::rename(paths.db(), &staged).unwrap();
        paths.prepare().unwrap();
        for name in ["maple.db", "generation.json"] {
            std::fs::copy(staged.join(name), paths.db().join(name)).unwrap();
        }
        let (gap_ready, at_gap) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        let owner_paths = paths.clone();
        let owner = std::thread::spawn(move || {
            let _ownership = ownership;
            std::fs::rename(owner_paths.db(), owner_paths.root().join("db.old")).unwrap();
            gap_ready.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(!owner_paths.db().exists());
            // The live owner's second rename must work directly, without a
            // spurious client-created db/ or a recovery-required detour.
            std::fs::rename(owner_paths.root().join("db.new"), owner_paths.db()).unwrap();
            let recovered =
                Store::unavailable(owner_paths.clone(), StoreState::Maintenance).unwrap();
            recovered.initialize_recovered().unwrap();
            assert_eq!(recovered.state(), StoreState::Open);
            assert_eq!(recovered.store_id().unwrap(), generation);
            assert!(
                recovered
                    .account_id("rename-gap-fixture")
                    .unwrap()
                    .is_some()
            );
            assert!(!owner_paths.root().join("db.old").exists());
            assert!(!owner_paths.root().join("db.new").exists());
            recovered.close_for_shutdown().unwrap();
        });
        at_gap.recv_timeout(Duration::from_secs(10)).unwrap();
        for _ in 0..16 {
            let client =
                Endpoint::from_roots("http://127.0.0.1:1", config.clone(), data.clone()).unwrap();
            assert_eq!(client.address, endpoint.address);
            assert_eq!(client.ownership, endpoint.ownership);
            assert!(
                !paths.db().exists(),
                "a client must not recreate the database directory"
            );
            assert!(matches!(
                client.ownership_lock().unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
        release.send(()).unwrap();
        owner.join().unwrap();
        // Only this fixture's exact, empty IPC control directory was created.
        std::fs::remove_dir(&endpoint.directory).unwrap();
    }

    #[test]
    fn previous_owner_guard_opens_only_the_existing_control_lock() {
        let fixture = tempfile::tempdir().unwrap();
        let old_controls = fixture.path().join("previous-control");
        private_directory(&old_controls).unwrap();
        let previous_data = fixture.path().join("previous-data");
        std::fs::create_dir(&previous_data).unwrap();
        std::fs::write(previous_data.join("auth.json"), b"sentinel").unwrap();
        let mut endpoint = Endpoint::for_test(fixture.path().to_path_buf());
        endpoint.previous_directory = Some(old_controls.clone());
        let owner = open_lock(&old_controls.join("host.lock"), true).unwrap();
        owner.try_lock().unwrap();
        assert!(endpoint.previous_owner_guard().is_err());
        drop(owner);
        let guard = endpoint.previous_owner_guard().unwrap().unwrap();
        assert_eq!(
            std::fs::read(previous_data.join("auth.json")).unwrap(),
            b"sentinel"
        );
        drop(guard);
        endpoint.previous_directory = Some(fixture.path().join("absent-control"));
        assert!(endpoint.previous_owner_guard().unwrap().is_none());
        assert!(!fixture.path().join("absent-control").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_previous_control_is_not_treated_as_missing() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses ordinary mode bits and cannot exercise EACCES here.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let controls = fixture.path().join("previous-control");
        private_directory(&controls).unwrap();
        drop(open_lock(&controls.join("host.lock"), true).unwrap());
        let mut endpoint = Endpoint::for_test(fixture.path().to_path_buf());
        endpoint.previous_directory = Some(controls.clone());
        std::fs::set_permissions(&controls, std::fs::Permissions::from_mode(0o0)).unwrap();
        let result = endpoint.previous_owner_guard();
        std::fs::set_permissions(&controls, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "unreadable control ownership must fail closed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_replaced_listener_never_unlinks_the_rebound_socket() {
        use std::os::unix::fs::PermissionsExt;
        // TempDir otherwise inherits umask and may be 0755. The socket
        // directory's parent must model production's verified 0700 parent.
        let fixture = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        // The real endpoint's parent is private. Model that layout rather
        // than asking repair_directory to adopt a shared test TMPDIR.
        let endpoint = Endpoint::for_test(fixture.path().join("endpoint"));
        endpoint.repair_directory().unwrap();
        let previous = endpoint.bind().unwrap();
        std::fs::remove_file(&endpoint.address).unwrap();
        assert!(!previous.endpoint_present());
        endpoint.repair_directory().unwrap();
        let rebound = endpoint.bind().unwrap();
        drop(previous);
        assert!(rebound.endpoint_present());
        drop(rebound);
        assert!(!endpoint.address.exists());
    }
}
