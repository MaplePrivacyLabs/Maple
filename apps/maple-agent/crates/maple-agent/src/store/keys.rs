//! Password slots for the new store only. No application data or SQL enters
//! this module. KDF work must be called from a blocking worker, never the GUI.
//!
//! The public slot checksum detects accidental file damage; only the AEAD tag
//! authenticates it. Keys and passwords deliberately have no serializable or
//! revealing Debug representation.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::LazyLock,
    time::{Duration, Instant},
};

use argon2::{Algorithm, Argon2, Block, Params, Version};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    Tag, XChaCha20Poly1305, XNonce,
    aead::{AeadInOut, KeyInit},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

use super::paths::{private_new_file, sync_directory, verify_private_path};

const FORMAT: &str = "maple-store-keys/1";
const KEY_FILE_LIMIT: u64 = 16 * 1024;
const MIN_MEMORY_KIB: u32 = 64 * 1024;
const MAX_READ_MEMORY_KIB: u32 = 4 * 1024 * 1024;
const MAX_WRITE_MEMORY_KIB: u32 = 1024 * 1024;
// An interim portability bound, not a measured low-end latency claim. At most
// the traffic of 1 GiB/t1, or 256 MiB/t3. DF5 measurements may revise it.
pub const STORED_TRAFFIC_CAP_KIB: u64 = 2 * 1024 * 1024;
const FLOOR_TRAFFIC_KIB: u64 = 512 * 1024;

pub type KeyResult<T> = Result<T, KeyError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    Damaged,
    MismatchedDatabase,
    WrongPassword,
    InvalidPassword,
    WeakPassword,
    NotEnoughMemory { required_mib: u32 },
    MemoryLock,
    Random,
    UnsafePath,
    Io { kind: io::ErrorKind },
    SynchronizationPending { kind: io::ErrorKind },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Damaged => f.write_str("Maple's key file is damaged."),
            Self::MismatchedDatabase => f.write_str("Maple's key file doesn't match its data."),
            Self::WrongPassword => f.write_str("That password didn't unlock Maple."),
            Self::InvalidPassword => f.write_str("The unlock password is too long or invalid."),
            Self::WeakPassword => f.write_str("Choose a stronger unlock password."),
            Self::NotEnoughMemory { required_mib } => write!(
                f,
                "This computer doesn't have enough free memory to unlock Maple (it needs {required_mib} MiB)."
            ),
            Self::MemoryLock => f.write_str("Maple couldn't protect its encryption key in memory."),
            Self::Random => f.write_str("Maple couldn't obtain secure random bytes."),
            Self::UnsafePath => f.write_str("Maple's key file location isn't private and safe."),
            Self::Io { .. } => f.write_str("Maple couldn't read or save its key file."),
            Self::SynchronizationPending { .. } => f.write_str("The new unlock password took effect, but Maple couldn't confirm its key file was synchronized. Keep both passwords until synchronization finishes."),
        }
    }
}

impl std::error::Error for KeyError {}

impl From<io::Error> for KeyError {
    fn from(value: io::Error) -> Self {
        Self::Io { kind: value.kind() }
    }
}

impl From<super::StoreError> for KeyError {
    fn from(value: super::StoreError) -> Self {
        match value {
            super::StoreError::Io { kind } => Self::Io { kind },
            _ => Self::UnsafePath,
        }
    }
}

/// A dedicated locked mapping avoids the shared allocator-page problem where
/// dropping one small mlocked allocation can unlock another live key's page.
/// Every path wipes before unlocking and releasing the mapping.
pub struct LockedKey {
    memory: NonNull<u8>,
    allocation_len: usize,
}

// A LockedKey is exclusively mutable during construction/derivation. After it
// is returned, only immutable byte borrows are exposed. It owns its mapping.
unsafe impl Send for LockedKey {}
unsafe impl Sync for LockedKey {}

impl fmt::Debug for LockedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LockedKey([REDACTED])")
    }
}

impl LockedKey {
    pub fn random() -> KeyResult<Self> {
        let mut key = Self::zeroed()?;
        OsRng
            .try_fill_bytes(key.as_mut_bytes())
            .map_err(|_| KeyError::Random)?;
        Ok(key)
    }

    /// The input is consumed and wiped even when allocation or locking fails.
    pub fn from_bytes(bytes: [u8; 32]) -> KeyResult<Self> {
        let bytes = Zeroizing::new(bytes);
        let mut key = Self::zeroed()?;
        key.as_mut_bytes().copy_from_slice(bytes.as_ref());
        Ok(key)
    }

    /// Read a raw key directly into its locked mapping. On a short read or
    /// another input failure, Drop wipes the partially filled mapping.
    pub(super) fn read_from(input: &mut impl io::Read) -> KeyResult<Self> {
        let mut key = Self::zeroed()?;
        input.read_exact(key.as_mut_bytes())?;
        Ok(key)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        // The mapping is at least 32 bytes, aligned, live, and immutable here.
        unsafe { &*self.memory.as_ptr().cast::<[u8; 32]>() }
    }

    /// SQLCipher's raw-key literal exists only in this temporary locked page.
    /// The callback must key the native connection immediately, without copying
    /// the buffer into SQL text, an error, or any longer-lived allocation.
    pub(super) fn with_sqlcipher_key<T>(
        &self,
        callback: impl FnOnce(&[u8; 67]) -> T,
    ) -> KeyResult<T> {
        let encoded = Self::zeroed()?;
        if encoded.allocation_len < 67 {
            return Err(KeyError::MemoryLock);
        }
        let bytes = unsafe { &mut *encoded.memory.as_ptr().cast::<[u8; 67]>() };
        bytes[0] = b'x';
        bytes[1] = b'\'';
        for (ix, value) in self.as_bytes().iter().copied().enumerate() {
            bytes[2 + ix * 2] = b"0123456789abcdef"[usize::from(value >> 4)];
            bytes[3 + ix * 2] = b"0123456789abcdef"[usize::from(value & 15)];
        }
        bytes[66] = b'\'';
        Ok(callback(bytes))
    }

    fn as_mut_bytes(&mut self) -> &mut [u8; 32] {
        unsafe { &mut *self.memory.as_ptr().cast::<[u8; 32]>() }
    }

    #[cfg(unix)]
    fn zeroed() -> KeyResult<Self> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let allocation_len = usize::try_from(page)
            .ok()
            .filter(|size| *size >= 32)
            .ok_or(KeyError::MemoryLock)?;
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                allocation_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(KeyError::NotEnoughMemory { required_mib: 1 });
        }
        let Some(memory) = NonNull::new(mapping.cast::<u8>()) else {
            unsafe { libc::munmap(mapping, allocation_len) };
            return Err(KeyError::MemoryLock);
        };
        if unsafe { libc::mlock(mapping, allocation_len) } != 0 {
            unsafe { libc::munmap(mapping, allocation_len) };
            return Err(KeyError::MemoryLock);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(mapping, allocation_len, libc::MADV_DONTDUMP);
        }
        Ok(Self {
            memory,
            allocation_len,
        })
    }

    #[cfg(windows)]
    fn zeroed() -> KeyResult<Self> {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
            VirtualLock,
        };
        // VirtualAlloc rounds up to the native page size. Lock and wipe exactly
        // this 4 KiB private region; all supported Windows targets use >=4 KiB.
        let allocation_len = 4096;
        let mapping = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                allocation_len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        let Some(memory) = NonNull::new(mapping.cast::<u8>()) else {
            return Err(KeyError::NotEnoughMemory { required_mib: 1 });
        };
        if unsafe { VirtualLock(mapping, allocation_len) } == 0 {
            #[cfg(test)]
            {
                // Capture before VirtualFree or diagnostic queries can replace
                // the native failure. Never include an address or key bytes.
                let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                windows_key_diagnostics::lock_failed(error);
            }
            unsafe { VirtualFree(mapping, 0, MEM_RELEASE) };
            return Err(KeyError::MemoryLock);
        }
        #[cfg(test)]
        windows_key_diagnostics::locked(allocation_len);
        Ok(Self {
            memory,
            allocation_len,
        })
    }
}

impl Drop for LockedKey {
    fn drop(&mut self) {
        unsafe {
            std::slice::from_raw_parts_mut(self.memory.as_ptr(), self.allocation_len).zeroize();
            #[cfg(unix)]
            {
                libc::munlock(self.memory.as_ptr().cast(), self.allocation_len);
                libc::munmap(self.memory.as_ptr().cast(), self.allocation_len);
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree, VirtualUnlock};
                VirtualUnlock(self.memory.as_ptr().cast(), self.allocation_len);
                VirtualFree(self.memory.as_ptr().cast(), 0, MEM_RELEASE);
                #[cfg(test)]
                windows_key_diagnostics::released(self.allocation_len);
            }
        }
    }
}

// Temporary Windows test-process observation only. Counters own no keys or
// mappings, change no working-set limits, and cannot enable unlocked fallback.
#[cfg(all(test, windows))]
mod windows_key_diagnostics {
    use std::{
        fs::OpenOptions,
        io::{self, Write},
        sync::{
            OnceLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use windows_sys::Win32::{
        Foundation::GetLastError,
        System::{
            SystemInformation::{GetSystemInfo, SYSTEM_INFO},
            Threading::{GetCurrentProcess, GetProcessWorkingSetSize},
        },
    };

    static PAGE_BYTES: OnceLock<usize> = OnceLock::new();
    static LIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
    static PEAK_PAGES: AtomicUsize = AtomicUsize::new(0);
    static LOCK_FAILURES: AtomicUsize = AtomicUsize::new(0);
    static SUMMARY_WRITTEN: AtomicBool = AtomicBool::new(false);

    fn page_bytes() -> usize {
        *PAGE_BYTES.get_or_init(|| {
            let mut information = SYSTEM_INFO::default();
            unsafe { GetSystemInfo(&mut information) };
            information.dwPageSize as usize
        })
    }

    pub(super) fn locked(bytes: usize) {
        let pages = bytes.div_ceil(page_bytes());
        let live = LIVE_PAGES.fetch_add(pages, Ordering::SeqCst) + pages;
        PEAK_PAGES.fetch_max(live, Ordering::SeqCst);
    }

    pub(super) fn released(bytes: usize) {
        LIVE_PAGES.fetch_sub(bytes.div_ceil(page_bytes()), Ordering::SeqCst);
    }

    fn resources() -> String {
        let mut minimum = 0;
        let mut maximum = 0;
        let bounds_error =
            if unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &mut minimum, &mut maximum) }
                == 0
            {
                Some(unsafe { GetLastError() })
            } else {
                None
            };
        let live = LIVE_PAGES.load(Ordering::SeqCst);
        let peak = PEAK_PAGES.load(Ordering::SeqCst).max(live);
        format!(
            "live_pages={live}; peak_pages_so_far={peak}; page_bytes={}; \
             working_set_min_bytes={minimum}; working_set_max_bytes={maximum}; \
             working_set_query_error={bounds_error:?}; lock_failures={}",
            page_bytes(),
            LOCK_FAILURES.load(Ordering::SeqCst)
        )
    }

    pub(super) fn lock_failed(error: u32) {
        LOCK_FAILURES.fetch_add(1, Ordering::SeqCst);
        let _ = writeln!(
            io::stderr(),
            "Windows LockedKey VirtualLock failed: win32={error}; {}",
            resources()
        );
    }

    pub(super) fn write_replay_checkpoint() -> io::Result<()> {
        let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
            return Ok(());
        };
        if SUMMARY_WRITTEN.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // One bounded success checkpoint, not a claim about the final process
        // peak after later tests finish. No environment/path value is emitted.
        let summary = format!(
            "\nTemporary Windows LockedKey resources at replay stress completion: {}.\n",
            resources()
        );
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(summary.as_bytes())
    }
}

#[cfg(all(test, windows))]
pub(super) fn write_windows_key_test_summary() -> io::Result<()> {
    windows_key_diagnostics::write_replay_checkpoint()
}

/// NFC is applied equally on set and unlock. No minimum is imposed on unlock,
/// since a policy change must never strand an otherwise valid existing slot.
pub struct Password(Zeroizing<String>);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}

impl Password {
    pub fn normalize(value: &str) -> KeyResult<Self> {
        // A bound before normalization also bounds attacker-controlled work.
        // NFC never increases a valid 1,024-code-point input beyond 4 KiB.
        if value.len() > 8192 {
            return Err(KeyError::InvalidPassword);
        }
        let normalized = Zeroizing::new(value.nfc().collect::<String>());
        if normalized.chars().count() > 1024 {
            return Err(KeyError::InvalidPassword);
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn assess(&self, context: &[&str]) -> PasswordAssessment {
        assess_normalized(self.as_str(), context)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrengthLevel {
    TooEasy,
    CouldBeStronger,
    Strong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrengthReason {
    Length,
    Guesses,
    Accepted,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PasswordAssessment {
    pub level: StrengthLevel,
    pub reason: StrengthReason,
    pub guesses_log10: f64,
}

impl PasswordAssessment {
    pub fn allowed(self) -> bool {
        self.level != StrengthLevel::TooEasy
    }
}

static EFF_WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("keys/eff-large-wordlist.txt")
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, word)| word))
        .collect()
});

pub fn assess_password(value: &str, context: &[&str]) -> KeyResult<PasswordAssessment> {
    Ok(Password::normalize(value)?.assess(context))
}

fn assess_normalized(value: &str, context: &[&str]) -> PasswordAssessment {
    if !(15..=1024).contains(&value.chars().count()) {
        return PasswordAssessment {
            level: StrengthLevel::TooEasy,
            reason: StrengthReason::Length,
            guesses_log10: 0.0,
        };
    }
    let mut inputs = Vec::with_capacity(context.len().min(16) + 1);
    inputs.push("Maple");
    inputs.extend(context.iter().copied().take(16));
    // This is an estimate, not a guaranteed attack cost. The library bounds
    // its work by considering the first 100 Unicode characters.
    assessment_from_guesses(zxcvbn::zxcvbn(value, &inputs).guesses_log10())
}

fn assessment_from_guesses(mut guesses_log10: f64) -> PasswordAssessment {
    if !guesses_log10.is_finite() {
        guesses_log10 = 0.0;
    }
    let level = if guesses_log10 < 12.0 {
        StrengthLevel::TooEasy
    } else if guesses_log10 < 14.0 {
        StrengthLevel::CouldBeStronger
    } else {
        StrengthLevel::Strong
    };
    PasswordAssessment {
        level,
        reason: if level == StrengthLevel::TooEasy {
            StrengthReason::Guesses
        } else {
            StrengthReason::Accepted
        },
        guesses_log10,
    }
}

/// This type can only originate from a six-word OS-random sample. Editing its
/// text must go through ordinary typed-password assessment, never a client-
/// supplied "generated" flag.
pub struct GeneratedPassphrase(Password);

impl fmt::Debug for GeneratedPassphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GeneratedPassphrase([REDACTED])")
    }
}

impl GeneratedPassphrase {
    pub fn password(&self) -> &Password {
        &self.0
    }

    pub fn assessment(&self) -> PasswordAssessment {
        PasswordAssessment {
            level: StrengthLevel::Strong,
            reason: StrengthReason::Accepted,
            guesses_log10: 6.0 * (7776_f64).log10(),
        }
    }
}

pub fn generate_passphrase() -> KeyResult<GeneratedPassphrase> {
    generate_with_rng(&mut OsRng)
}

fn generate_with_rng(rng: &mut impl RngCore) -> KeyResult<GeneratedPassphrase> {
    if EFF_WORDS.len() != 7776 {
        return Err(KeyError::Damaged);
    }
    let mut phrase = Zeroizing::new(String::with_capacity(128));
    for ix in 0..6 {
        if ix != 0 {
            phrase.push(' ');
        }
        // Equal-size bins, with a rejected tail. In particular, never use
        // `random % 7776`, which would introduce modulo bias.
        let width = (u64::from(u32::MAX) + 1) / 7776;
        let limit = width * 7776;
        let index = loop {
            let mut bytes = [0_u8; 4];
            rng.try_fill_bytes(&mut bytes)
                .map_err(|_| KeyError::Random)?;
            let random = u64::from(u32::from_le_bytes(bytes));
            if random < limit {
                break (random / width) as usize;
            }
        };
        phrase.push_str(EFF_WORDS[index]);
    }
    Ok(GeneratedPassphrase(Password(phrase)))
}

#[cfg(test)]
pub(super) fn generated_for_test(rng: &mut impl RngCore) -> KeyResult<GeneratedPassphrase> {
    generate_with_rng(rng)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    pub const fn floor() -> Self {
        Self {
            m_kib: MIN_MEMORY_KIB,
            t: 3,
            p: 4,
        }
    }

    pub fn traffic_kib(self) -> u64 {
        (3 * u64::from(self.t))
            .saturating_sub(1)
            .saturating_mul(u64::from(self.m_kib))
    }

    pub fn validate(self) -> KeyResult<()> {
        if !(MIN_MEMORY_KIB..=MAX_READ_MEMORY_KIB).contains(&self.m_kib)
            || !(1..=16).contains(&self.t)
            || !(1..=16).contains(&self.p)
        {
            return Err(KeyError::Damaged);
        }
        Ok(())
    }

    pub fn validate_for_write(self) -> KeyResult<()> {
        self.validate()?;
        if self.m_kib > MAX_WRITE_MEMORY_KIB
            || self.p != 4
            || self.traffic_kib() < FLOOR_TRAFFIC_KIB
            || self.traffic_kib() > STORED_TRAFFIC_CAP_KIB
        {
            return Err(KeyError::Damaged);
        }
        Ok(())
    }

    /// Never weaken either memory or work when opportunistically raising a
    /// slot. Explicit password change can choose a cheaper machine's target.
    pub fn should_raise_to(self, target: Self) -> bool {
        target.validate_for_write().is_ok()
            && target.m_kib >= self.m_kib
            && target.traffic_kib() >= self.traffic_kib()
            && (target.m_kib > self.m_kib || target.traffic_kib() > self.traffic_kib())
    }

    pub fn calibrate(total_ram_bytes: u64) -> KeyResult<Self> {
        let password = Password::normalize("Maple Argon2 calibration, public probe")?;
        Self::calibrate_with(total_ram_bytes, |params| {
            let start = Instant::now();
            let _probe = derive_kek(&password, &[0x43; 32], params)?;
            Ok(start.elapsed())
        })
    }

    fn calibrate_with(
        total_ram_bytes: u64,
        mut measure: impl FnMut(Self) -> KeyResult<Duration>,
    ) -> KeyResult<Self> {
        let mut memory = (total_ram_bytes / 8 / 1024)
            .clamp(u64::from(MIN_MEMORY_KIB), u64::from(MAX_WRITE_MEMORY_KIB))
            as u32;
        // Whole multiples of the 4 lanes' four segments avoid surprising
        // Argon2 memory rounding while retaining memory-first calibration.
        memory -= memory % 16;
        loop {
            let mut params = Self {
                m_kib: memory,
                t: 1,
                p: 4,
            };
            let first = match measure(params) {
                Ok(duration) => duration,
                Err(KeyError::NotEnoughMemory { .. }) if memory > MIN_MEMORY_KIB => {
                    memory = lower_memory(memory);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if first > Duration::from_secs(2) && memory > MIN_MEMORY_KIB {
                memory = lower_memory(memory);
                continue;
            }
            let mut elapsed = first;
            while params.t < 16 {
                let next = Self {
                    t: params.t + 1,
                    ..params
                };
                if next.traffic_kib() > STORED_TRAFFIC_CAP_KIB {
                    break;
                }
                if params.traffic_kib() >= FLOOR_TRAFFIC_KIB && elapsed >= Duration::from_secs(1) {
                    break;
                }
                params = next;
                elapsed = measure(params)?;
            }
            params.validate_for_write()?;
            return Ok(params);
        }
    }
}

fn lower_memory(memory: u32) -> u32 {
    [512, 256, 128, 64]
        .into_iter()
        .map(|mib| mib * 1024)
        .find(|candidate| *candidate < memory)
        .unwrap_or(MIN_MEMORY_KIB)
}

fn derive_kek(password: &Password, salt: &[u8; 32], params: KdfParams) -> KeyResult<LockedKey> {
    params.validate()?;
    // try_reserve avoids Rust's abort-on-allocation-failure path. The matrix is
    // owned by Maple, and Zeroizing wipes it on success and every error path.
    let argon_params =
        Params::new(params.m_kib, params.t, params.p, Some(32)).map_err(|_| KeyError::Damaged)?;
    let mut memory = Zeroizing::new(Vec::<Block>::new());
    memory
        .try_reserve_exact(argon_params.block_count())
        .map_err(|_| KeyError::NotEnoughMemory {
            required_mib: params.m_kib.div_ceil(1024),
        })?;
    memory.resize(argon_params.block_count(), Block::new());
    let mut output = LockedKey::zeroed()?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params)
        .hash_password_into_with_memory(
            password.as_str().as_bytes(),
            salt,
            output.as_mut_bytes(),
            memory.as_mut_slice(),
        )
        .map_err(|_| KeyError::Damaged)?;
    Ok(output)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFile {
    db_salt: [u8; 16],
    params: KdfParams,
    salt: [u8; 32],
    nonce: [u8; 24],
    wrapped: [u8; 48],
    check: [u8; 16],
}

/// Retained only while presenting database damage: a new DEK can be wrapped
/// without keeping the user's password in memory.
pub struct WrappingKey {
    key: LockedKey,
    slot: KeyFile,
}

impl fmt::Debug for WrappingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WrappingKey([REDACTED])")
    }
}

impl WrappingKey {
    /// Recover the authenticated original key for a scoped read-only repair.
    /// The caller must bind the retained slot to the current generation/salt.
    pub(super) fn data_key(&self) -> KeyResult<LockedKey> {
        self.slot.open_with_kek(&self.key)
    }

    pub(super) fn database_salt(&self) -> &[u8; 16] {
        &self.slot.db_salt
    }

    pub fn wrap(&self, dek: &LockedKey, db_salt: [u8; 16]) -> KeyResult<KeyFile> {
        let mut nonce = [0_u8; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| KeyError::Random)?;
        KeyFile::seal_with_kek(
            dek,
            db_salt,
            self.slot.params,
            self.slot.salt,
            nonce,
            &self.key,
        )
    }
}

#[derive(Serialize, Deserialize)]
struct WireFile {
    format: String,
    db_salt: String,
    slots: Vec<WireSlot>,
}

#[derive(Serialize, Deserialize)]
struct WireSlot {
    #[serde(rename = "type")]
    kind: String,
    kdf: WireKdf,
    nonce: String,
    wrapped: String,
    check: String,
}

#[derive(Serialize, Deserialize)]
struct WireKdf {
    alg: String,
    v: u32,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
}

impl KeyFile {
    pub fn create(
        password: &Password,
        context: &[&str],
        dek: &LockedKey,
        db_salt: [u8; 16],
        params: KdfParams,
    ) -> KeyResult<Self> {
        if !password.assess(context).allowed() {
            return Err(KeyError::WeakPassword);
        }
        Self::create_randomized(password, dek, db_salt, params)
    }

    /// Only a generator-owned value can receive the entropy exemption. A
    /// boolean from a client or an edited suggestion is never sufficient.
    pub fn create_generated(
        generated: &GeneratedPassphrase,
        dek: &LockedKey,
        db_salt: [u8; 16],
        params: KdfParams,
    ) -> KeyResult<Self> {
        Self::create_randomized(generated.password(), dek, db_salt, params)
    }

    /// Strength policy is enforced when setting a password. An upgrade on
    /// unlock must still accept any already-authenticated existing password.
    pub fn raise(&self, password: &Password, target: KdfParams) -> KeyResult<Self> {
        if !self.params.should_raise_to(target) {
            return Ok(self.clone());
        }
        let dek = self.unlock(password)?;
        Self::create_randomized(password, &dek, self.db_salt, target)
    }

    fn create_randomized(
        password: &Password,
        dek: &LockedKey,
        db_salt: [u8; 16],
        params: KdfParams,
    ) -> KeyResult<Self> {
        params.validate_for_write()?;
        let mut salt = [0_u8; 32];
        let mut nonce = [0_u8; 24];
        OsRng
            .try_fill_bytes(&mut salt)
            .map_err(|_| KeyError::Random)?;
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| KeyError::Random)?;
        Self::create_with_randomness(password, dek, db_salt, params, salt, nonce)
    }

    fn create_with_randomness(
        password: &Password,
        dek: &LockedKey,
        db_salt: [u8; 16],
        params: KdfParams,
        salt: [u8; 32],
        nonce: [u8; 24],
    ) -> KeyResult<Self> {
        params.validate_for_write()?;
        let kek = derive_kek(password, &salt, params)?;
        Self::seal_with_kek(dek, db_salt, params, salt, nonce, &kek)
    }

    fn seal_with_kek(
        dek: &LockedKey,
        db_salt: [u8; 16],
        params: KdfParams,
        salt: [u8; 32],
        nonce: [u8; 24],
        kek: &LockedKey,
    ) -> KeyResult<Self> {
        let mut file = Self {
            db_salt,
            params,
            salt,
            nonce,
            wrapped: [0; 48],
            check: [0; 16],
        };
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.as_bytes()).map_err(|_| KeyError::Damaged)?;
        // Keep the only temporary plaintext DEK in a locked, wiping mapping. The
        // returned KeyFile contains ciphertext and public metadata only.
        let mut plaintext = LockedKey::zeroed()?;
        plaintext.as_mut_bytes().copy_from_slice(dek.as_bytes());
        let tag = cipher
            .encrypt_inout_detached(
                &XNonce::from(nonce),
                &file.slot_bytes(),
                plaintext.as_mut_bytes().as_mut_slice().into(),
            )
            .map_err(|_| KeyError::Damaged)?;
        file.wrapped[..32].copy_from_slice(plaintext.as_bytes());
        file.wrapped[32..].copy_from_slice(tag.as_slice());
        file.check = file.checksum();
        Ok(file)
    }

    pub fn params(&self) -> KdfParams {
        self.params
    }

    pub fn db_salt(&self) -> &[u8; 16] {
        &self.db_salt
    }

    /// String prefixes are u16 LE and integer parameters are u32 LE. No JSON
    /// serialization detail, unknown field or object ordering enters the AAD.
    pub fn slot_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(128);
        fn string(bytes: &mut Vec<u8>, value: &str) {
            bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        string(&mut bytes, FORMAT);
        bytes.extend_from_slice(&self.db_salt);
        string(&mut bytes, "password");
        string(&mut bytes, "argon2id");
        for value in [19, self.params.m_kib, self.params.t, self.params.p] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&self.salt);
        bytes
    }

    fn checksum(&self) -> [u8; 16] {
        let mut hash = Sha256::new();
        hash.update(self.slot_bytes());
        hash.update(self.nonce);
        hash.update(self.wrapped);
        let digest = hash.finalize();
        let mut check = [0_u8; 16];
        check.copy_from_slice(&digest[..16]);
        check
    }

    pub fn parse(bytes: &[u8], database_salt: &[u8; 16]) -> KeyResult<Self> {
        if bytes.len() as u64 > KEY_FILE_LIMIT {
            return Err(KeyError::Damaged);
        }
        let wire: WireFile = serde_json::from_slice(bytes).map_err(|_| KeyError::Damaged)?;
        if wire.format != FORMAT || wire.slots.len() != 1 {
            return Err(KeyError::Damaged);
        }
        let slot = wire.slots.into_iter().next().ok_or(KeyError::Damaged)?;
        if slot.kind != "password" || slot.kdf.alg != "argon2id" || slot.kdf.v != 19 {
            return Err(KeyError::Damaged);
        }
        let file = Self {
            db_salt: decode(&wire.db_salt)?,
            params: KdfParams {
                m_kib: slot.kdf.m_kib,
                t: slot.kdf.t,
                p: slot.kdf.p,
            },
            salt: decode(&slot.kdf.salt)?,
            nonce: decode(&slot.nonce)?,
            wrapped: decode(&slot.wrapped)?,
            check: decode(&slot.check)?,
        };
        // The order distinguishes accidental damage from a mismatched folder
        // and, critically, checks attacker-controlled costs before any KDF.
        if file.check != file.checksum() {
            return Err(KeyError::Damaged);
        }
        file.params.validate()?;
        if file.db_salt != *database_salt {
            return Err(KeyError::MismatchedDatabase);
        }
        Ok(file)
    }

    pub fn to_json(&self) -> KeyResult<Vec<u8>> {
        let wire = WireFile {
            format: FORMAT.into(),
            db_salt: URL_SAFE_NO_PAD.encode(self.db_salt),
            slots: vec![WireSlot {
                kind: "password".into(),
                kdf: WireKdf {
                    alg: "argon2id".into(),
                    v: 19,
                    m_kib: self.params.m_kib,
                    t: self.params.t,
                    p: self.params.p,
                    salt: URL_SAFE_NO_PAD.encode(self.salt),
                },
                nonce: URL_SAFE_NO_PAD.encode(self.nonce),
                wrapped: URL_SAFE_NO_PAD.encode(self.wrapped),
                check: URL_SAFE_NO_PAD.encode(self.check),
            }],
        };
        serde_json::to_vec_pretty(&wire).map_err(|_| KeyError::Damaged)
    }

    pub fn unlock(&self, password: &Password) -> KeyResult<LockedKey> {
        // Instances can only originate from validated parsing or creation.
        let kek = derive_kek(password, &self.salt, self.params)?;
        self.open_with_kek(&kek)
    }

    pub fn authenticate(&self, password: &Password) -> KeyResult<(LockedKey, WrappingKey)> {
        let kek = derive_kek(password, &self.salt, self.params)?;
        let dek = self.open_with_kek(&kek)?;
        Ok((
            dek,
            WrappingKey {
                key: kek,
                slot: self.clone(),
            },
        ))
    }

    fn open_with_kek(&self, kek: &LockedKey) -> KeyResult<LockedKey> {
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.as_bytes()).map_err(|_| KeyError::Damaged)?;
        let mut key = LockedKey::zeroed()?;
        key.as_mut_bytes().copy_from_slice(&self.wrapped[..32]);
        let mut tag = [0_u8; 16];
        tag.copy_from_slice(&self.wrapped[32..]);
        cipher
            .decrypt_inout_detached(
                &XNonce::from(self.nonce),
                &self.slot_bytes(),
                key.as_mut_bytes().as_mut_slice().into(),
                &Tag::from(tag),
            )
            .map_err(|_| KeyError::WrongPassword)?;
        Ok(key)
    }
}

fn decode<const N: usize>(value: &str) -> KeyResult<[u8; N]> {
    // Reject alternate encodings so one canonical byte string has one JSON
    // value; URL_SAFE_NO_PAD rejects padding and non-zero trailing bits.
    if value.len() != N.div_ceil(3) * 4 - ((3 - N % 3) % 3) {
        return Err(KeyError::Damaged);
    }
    let mut bytes = [0_u8; N];
    let len = URL_SAFE_NO_PAD
        .decode_slice(value, &mut bytes)
        .map_err(|_| KeyError::Damaged)?;
    if len != N {
        return Err(KeyError::Damaged);
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyWriteOutcome {
    /// Primary is installed, so the password change has committed in the running
    /// host. Report a retryable reconciliation issue, never roll back.
    pub backup_pending: bool,
    /// Keep both passwords when directory synchronization could not confirm
    /// which name survives a power failure. A successful backup write clears it.
    pub synchronization_pending: bool,
}

#[derive(Debug)]
pub struct UnlockResult {
    pub key: LockedKey,
    pub used_previous_password: bool,
    pub reconciliation_pending: bool,
}

#[derive(Debug)]
pub struct AuthenticatedUnlock {
    pub key: LockedKey,
    pub wrapping: WrappingKey,
    pub used_previous_password: bool,
    pub reconciliation_pending: bool,
}

#[derive(Debug)]
pub struct KeyFilePair {
    directory: PathBuf,
    primary: KeyFile,
    previous: Option<KeyFile>,
    reconciliation_pending: bool,
}

impl KeyFilePair {
    /// Startup metadata recovery never derives a key and never opens SQLite.
    /// The database salt is read by the store owner from the verified database.
    pub fn load(directory: &Path, database_salt: &[u8; 16]) -> KeyResult<Self> {
        verify_private_path(directory, true)?;
        let primary = read_key_file(&directory.join("keys.json"), database_salt);
        let backup = read_key_file(&directory.join("keys.backup.json"), database_salt);
        let mut pair = match (primary, backup) {
            (Ok(primary), backup) => {
                let previous = backup.ok().filter(|backup| *backup != primary);
                Self {
                    directory: directory.into(),
                    primary,
                    previous,
                    reconciliation_pending: true,
                }
            }
            (Err(_), Ok(backup)) => Self {
                directory: directory.into(),
                primary: backup,
                previous: None,
                reconciliation_pending: true,
            },
            (Err(primary), Err(backup)) => {
                return Err(
                    if primary == KeyError::MismatchedDatabase
                        || backup == KeyError::MismatchedDatabase
                    {
                        KeyError::MismatchedDatabase
                    } else {
                        KeyError::Damaged
                    },
                );
            }
        };
        // Read-only media or an interrupted writer may prevent reconciliation;
        // a valid current slot can still unlock, and a differing backup remains
        // available for the specifically documented previous-password fallback.
        let _ = pair.reconcile();
        Ok(pair)
    }

    pub fn primary(&self) -> &KeyFile {
        &self.primary
    }

    pub fn reconciliation_pending(&self) -> bool {
        self.reconciliation_pending
    }

    pub fn reconcile(&mut self) -> KeyResult<()> {
        self.reconciliation_pending = true;
        // Always restore the authoritative in-memory primary first. It can
        // have originated from backup when the primary file was damaged.
        for name in ["keys.json", "keys.backup.json"] {
            let path = self.directory.join(name);
            if read_key_file(&path, &self.primary.db_salt).ok().as_ref() != Some(&self.primary) {
                write_key_file(&path, &self.primary)?;
            }
        }
        self.previous = None;
        self.reconciliation_pending = false;
        Ok(())
    }

    pub fn unlock(&mut self, password: &Password) -> KeyResult<UnlockResult> {
        let authenticated = self.authenticate(password)?;
        Ok(UnlockResult {
            key: authenticated.key,
            used_previous_password: authenticated.used_previous_password,
            reconciliation_pending: authenticated.reconciliation_pending,
        })
    }

    pub fn authenticate(&mut self, password: &Password) -> KeyResult<AuthenticatedUnlock> {
        let ((key, wrapping), used_previous_password) = match self.primary.authenticate(password) {
            Ok(authenticated) => (authenticated, false),
            Err(KeyError::WrongPassword) => match &self.previous {
                Some(previous) => (previous.authenticate(password)?, true),
                None => return Err(KeyError::WrongPassword),
            },
            Err(error) => return Err(error),
        };
        let _ = self.reconcile();
        Ok(AuthenticatedUnlock {
            key,
            wrapping,
            used_previous_password,
            reconciliation_pending: self.reconciliation_pending,
        })
    }

    pub fn write(directory: &Path, key_file: &KeyFile) -> KeyResult<KeyWriteOutcome> {
        Self::write_with(directory, key_file, write_key_file)
    }

    fn write_with(
        directory: &Path,
        key_file: &KeyFile,
        mut write: impl FnMut(&Path, &KeyFile) -> KeyResult<()>,
    ) -> KeyResult<KeyWriteOutcome> {
        verify_private_path(directory, true)?;
        let primary_sync_pending = match write(&directory.join("keys.json"), key_file) {
            Ok(()) => false,
            Err(KeyError::SynchronizationPending { .. }) => true,
            Err(error) => return Err(error),
        };
        let backup_pending = write(&directory.join("keys.backup.json"), key_file).is_err();
        Ok(KeyWriteOutcome {
            backup_pending,
            synchronization_pending: primary_sync_pending && backup_pending,
        })
    }
}

pub(super) fn read_key_file(path: &Path, database_salt: &[u8; 16]) -> KeyResult<KeyFile> {
    verify_private_path(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if file.metadata()?.len() > KEY_FILE_LIMIT {
        return Err(KeyError::Damaged);
    }
    let mut bytes = Vec::new();
    file.take(KEY_FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    KeyFile::parse(&bytes, database_salt)
}

fn write_key_file(path: &Path, key_file: &KeyFile) -> KeyResult<()> {
    write_key_file_with_sync(path, key_file, sync_directory)
}

fn write_key_file_with_sync(
    path: &Path,
    key_file: &KeyFile,
    sync_parent: fn(&Path) -> io::Result<()>,
) -> KeyResult<()> {
    let parent = path.parent().ok_or(KeyError::UnsafePath)?;
    verify_private_path(parent, true)?;
    match fs::symlink_metadata(path) {
        Ok(_) => verify_private_path(path, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut random = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| KeyError::Random)?;
    let temporary = parent.join(format!(".keys-{}.tmp", URL_SAFE_NO_PAD.encode(random)));
    let mut file = private_new_file(&temporary)?;
    let mut installed = false;
    let operation: KeyResult<()> = (|| {
        file.write_all(&key_file.to_json()?)?;
        sync_key_file(&file)?;
        drop(file);
        // On Windows std::fs::rename cannot replace an existing destination.
        // MoveFileEx's write-through flag supplies the durable replacement.
        replace_key_file(&temporary, path)?;
        installed = true;
        sync_parent(parent)?;
        Ok(())
    })();
    if operation.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    match operation {
        Err(KeyError::Io { kind }) if installed => Err(KeyError::SynchronizationPending { kind }),
        outcome => outcome,
    }
}

pub(super) fn sync_key_file(file: &File) -> io::Result<()> {
    file.sync_all()?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn replace_key_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
pub(super) fn replace_key_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
