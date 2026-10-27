//! Native Authenticode checks for staged Windows executables. The signed update
//! envelope authenticates the complete archive independently; Windows trust must
//! additionally accept the executable and the expected publisher and timestamp.

use std::{
    ffi::c_void,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    mem::size_of,
    os::windows::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, INVALID_HANDLE_VALUE, SetLastError, TRUST_E_NOSIGNATURE},
    Security::{
        Cryptography::{CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_COMMON_NAME},
        WinTrust::{
            CRYPT_PROVIDER_SGNR, SGNR_TYPE_TIMESTAMP, WINTRUST_ACTION_GENERIC_VERIFY_V2,
            WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL,
            WTD_CHOICE_FILE, WTD_DISABLE_MD2_MD4, WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
            WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
            WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvCertFromChain,
            WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
        },
    },
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
};

const EXPECTED_PUBLISHER: &str = "Mutiny Wallet Inc. dba OpenSecret";
const MAX_NAME_UNITS: u32 = 512;

#[derive(Clone, Copy)]
enum Purpose {
    Admission,
    Startup,
}

impl Purpose {
    fn provider_flags(self) -> u32 {
        match self {
            Self::Admission => WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT | WTD_DISABLE_MD2_MD4,
            Self::Startup => {
                WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE | WTD_DISABLE_MD2_MD4
            }
        }
    }
}

fn trust_configuration(info: &mut WINTRUST_FILE_INFO, purpose: Purpose) -> WINTRUST_DATA {
    WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: purpose.provider_flags(),
        ..Default::default()
    }
}

/// State belongs to WinTrust, including after a failed verification. Closing it
/// is mandatory for every VERIFY call, not only for successful verifications.
struct TrustState(WINTRUST_DATA);

impl Drop for TrustState {
    fn drop(&mut self) {
        self.0.dwStateAction = WTD_STATEACTION_CLOSE;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        // SAFETY: the state was initialized for this exact action. Its file and
        // path arguments outlive this guard; CLOSE releases provider resources.
        unsafe {
            WinVerifyTrust(
                INVALID_HANDLE_VALUE,
                &mut action,
                (&mut self.0 as *mut WINTRUST_DATA).cast::<c_void>(),
            );
        }
    }
}

fn publisher_matches(name: &[u16]) -> bool {
    name.len() > 1
        && name.last() == Some(&0)
        && !name[..name.len() - 1].contains(&0)
        && String::from_utf16(&name[..name.len() - 1]).is_ok_and(|name| name == EXPECTED_PUBLISHER)
}

/// An unsigned Dev executable must still be a PE with no certificate table.
/// WinTrust can report NOSIGNATURE for malformed signatures too, so its error
/// alone must never turn a damaged signed executable into an unsigned one.
fn pe_has_no_certificate(reader: &mut (impl Read + Seek)) -> bool {
    let Ok(length) = reader.seek(SeekFrom::End(0)) else {
        return false;
    };
    let mut dos = [0u8; 64];
    if reader.seek(SeekFrom::Start(0)).is_err()
        || reader.read_exact(&mut dos).is_err()
        || &dos[..2] != b"MZ"
    {
        return false;
    }
    let pe_offset = u64::from(u32::from_le_bytes(dos[60..64].try_into().unwrap()));
    if !(64..=16 * 1024 * 1024).contains(&pe_offset) || pe_offset + 24 > length {
        return false;
    }
    let mut coff = [0u8; 24];
    if reader.seek(SeekFrom::Start(pe_offset)).is_err()
        || reader.read_exact(&mut coff).is_err()
        || &coff[..4] != b"PE\0\0"
        || u16::from_le_bytes(coff[22..24].try_into().unwrap()) & 2 == 0
        || !(1..=96).contains(&u16::from_le_bytes(coff[6..8].try_into().unwrap()))
    {
        return false;
    }
    let optional_size = usize::from(u16::from_le_bytes(coff[20..22].try_into().unwrap()));
    if !(96..=4096).contains(&optional_size) || pe_offset + 24 + optional_size as u64 > length {
        return false;
    }
    let mut optional = vec![0u8; optional_size];
    if reader.read_exact(&mut optional).is_err() {
        return false;
    }
    let directory_start = match u16::from_le_bytes(optional[..2].try_into().unwrap()) {
        0x10b => 96,  // PE32, including the NSIS uninstaller.
        0x20b => 112, // PE32+, including the x86-64 app.
        _ => return false,
    };
    if optional.len() < directory_start {
        return false;
    }
    let directory_count = u32::from_le_bytes(
        optional[directory_start - 4..directory_start]
            .try_into()
            .unwrap(),
    ) as usize;
    // Both supported toolchains emit the standard 16 data directories. Reject
    // truncated/ambiguous headers rather than infer that a signature is absent.
    if directory_count != 16 || optional.len() < directory_start + 16 * 8 {
        return false;
    }
    let certificate = &optional[directory_start + 4 * 8..directory_start + 5 * 8];
    certificate == [0; 8]
}

/// Verify data only. This never runs the downloaded executable or invokes a
/// shell. There are no environment, registry, or test overrides of the signer.
/// Windows may retrieve certificate-chain/revocation material under its trust
/// policy; an unavailable or untrusted chain fails closed.
pub(super) fn verify(path: &Path) -> bool {
    verify_for_purpose(path, false, Purpose::Admission)
}

/// Only the caller's embedded Dev profile may opt into unsigned local builds.
/// Signed files always require the exact Maple publisher and trusted timestamp,
/// including when unsigned Dev files are permitted. Production updates call
/// `verify`, and never enter this exception.
pub(super) fn verify_or_unsigned(path: &Path, allow_unsigned: bool) -> bool {
    verify_for_purpose(path, allow_unsigned, Purpose::Admission)
}

/// Reauthenticate an already admitted installed executable at normal startup.
/// Startup uses only cached chain material and does not perform revocation URL
/// retrieval; it still requires Windows trust, exact publisher and timestamp.
/// Installation and update mutation must continue to use the admission APIs.
pub(super) fn verify_startup_or_unsigned(path: &Path, allow_unsigned: bool) -> bool {
    verify_for_purpose(path, allow_unsigned, Purpose::Startup)
}

fn verify_for_purpose(path: &Path, allow_unsigned: bool, purpose: Purpose) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.is_empty() || wide.len() > 32_766 || wide.contains(&0) {
        return false;
    }
    wide.push(0);
    // Deny concurrent writes/deletion while the same handle is authenticated.
    // The caller separately validates every staging ancestor and archive hash.
    let Ok(file) = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return false;
    }
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        hFile: file.as_raw_handle(),
        ..Default::default()
    };
    let mut state = TrustState(trust_configuration(&mut info, purpose));
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: all structures have their required sizes and pointers refer to
    // live immutable path/file data. The state remains live until CLOSE.
    let (status, reason) = unsafe {
        SetLastError(0);
        let status = WinVerifyTrust(
            INVALID_HANDLE_VALUE,
            &mut action,
            (&mut state.0 as *mut WINTRUST_DATA).cast::<c_void>(),
        );
        (status, GetLastError())
    };
    if allow_unsigned && status == TRUST_E_NOSIGNATURE && reason == TRUST_E_NOSIGNATURE as u32 {
        return pe_has_no_certificate(&mut &file);
    }
    // WinVerifyTrust returns LONG, not HRESULT. Only exactly zero is success.
    if status != 0 || state.0.hWVTStateData.is_null() {
        return false;
    }
    // SAFETY: provider pointers are owned by the successful state and remain
    // valid until its guard closes it. No provider-owned memory is modified.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state.0.hWVTStateData);
        if provider.is_null() || (*provider).dwError != 0 {
            return false;
        }
        let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
        if signer.is_null()
            || (*signer).dwError != 0
            || (*signer).csCertChain == 0
            || (*signer).csCounterSigners == 0
        {
            return false;
        }
        let certificate = WTHelperGetProvCertFromChain(signer, 0);
        if certificate.is_null() || (*certificate).dwError != 0 || (*certificate).pCert.is_null() {
            return false;
        }
        let count = CertGetNameStringW(
            (*certificate).pCert,
            CERT_NAME_ATTR_TYPE,
            0,
            szOID_COMMON_NAME.cast::<c_void>(),
            std::ptr::null_mut(),
            0,
        );
        if !(2..=MAX_NAME_UNITS).contains(&count) {
            return false;
        }
        let mut name = vec![0u16; count as usize];
        if CertGetNameStringW(
            (*certificate).pCert,
            CERT_NAME_ATTR_TYPE,
            0,
            szOID_COMMON_NAME.cast::<c_void>(),
            name.as_mut_ptr(),
            count,
        ) != count
            || !publisher_matches(&name)
        {
            return false;
        }
        let timestamp = WTHelperGetProvSignerFromChain(provider, 0, 1, 0);
        valid_timestamp(timestamp)
    }
}

unsafe fn valid_timestamp(timestamp: *const CRYPT_PROVIDER_SGNR) -> bool {
    // SAFETY: this pointer, when non-null, comes from the live verified provider
    // state. Bounds come from that provider, not from serialized app metadata.
    unsafe {
        !timestamp.is_null()
            && (*timestamp).dwError == 0
            && (*timestamp).dwSignerType & SGNR_TYPE_TIMESTAMP != 0
            && (*timestamp).csCertChain > 0
            && !(*timestamp).pasCertChain.is_null()
            && !(*(*timestamp).pasCertChain).pCert.is_null()
            && (*(*timestamp).pasCertChain).dwError == 0
            && ((*timestamp).sftVerifyAsOf.dwLowDateTime != 0
                || (*timestamp).sftVerifyAsOf.dwHighDateTime != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purpose_provider_flags_pin_startup_offline_and_admission_revocation() {
        assert_eq!(Purpose::Admission.provider_flags(), 0x2080);
        assert_eq!(Purpose::Startup.provider_flags(), 0x3010);
        assert_eq!(
            Purpose::Admission.provider_flags() & WTD_CACHE_ONLY_URL_RETRIEVAL,
            0
        );
        assert_eq!(
            Purpose::Startup.provider_flags() & WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
            0
        );
    }

    #[test]
    fn native_trust_configuration_keeps_startup_and_admission_separate() {
        let mut info = WINTRUST_FILE_INFO::default();
        for (purpose, flags) in [(Purpose::Admission, 0x2080), (Purpose::Startup, 0x3010)] {
            let data = trust_configuration(&mut info, purpose);
            assert_eq!(data.cbStruct as usize, size_of::<WINTRUST_DATA>());
            assert_eq!(data.dwProvFlags, flags);
            assert_eq!(data.fdwRevocationChecks, WTD_REVOKE_NONE);
            assert_eq!(data.dwUIChoice, WTD_UI_NONE);
            assert_eq!(data.dwUnionChoice, WTD_CHOICE_FILE);
            assert_eq!(data.dwStateAction, WTD_STATEACTION_VERIFY);
            assert!(data.hWVTStateData.is_null());
        }
    }

    #[test]
    fn signer_requires_the_exact_compiled_publisher() {
        let encode = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        assert!(publisher_matches(&encode(EXPECTED_PUBLISHER)));
        for other in [
            "A different trusted software publisher",
            "Mutiny Wallet Inc. dba OpenSecret ",
            "mutiny wallet inc. dba opensecret",
            "Mutiny Wallet Inc. dba OpenSecret\0untrusted",
        ] {
            assert!(!publisher_matches(&encode(other)));
        }
        assert!(!publisher_matches(&[]));
        assert!(!publisher_matches(&[0]));
        assert!(!publisher_matches(&[0xD800, 0]));
    }

    #[test]
    fn pe_certificate_directory_must_be_completely_absent() {
        use std::io::Cursor;

        for (magic, directory_start, optional_size) in [(0x10bu16, 96, 224), (0x20b, 112, 240)] {
            let mut bytes = vec![0u8; 64 + 24 + optional_size];
            bytes[..2].copy_from_slice(b"MZ");
            bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
            bytes[64..68].copy_from_slice(b"PE\0\0");
            bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
            bytes[84..86].copy_from_slice(&(optional_size as u16).to_le_bytes());
            bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
            bytes[88..90].copy_from_slice(&magic.to_le_bytes());
            bytes[88 + directory_start - 4..88 + directory_start]
                .copy_from_slice(&16u32.to_le_bytes());
            assert!(pe_has_no_certificate(&mut Cursor::new(&bytes)));
            for nonzero in 0..8 {
                let mut signed = bytes.clone();
                signed[88 + directory_start + 4 * 8 + nonzero] = 1;
                assert!(!pe_has_no_certificate(&mut Cursor::new(signed)));
            }
            for truncated_at in [0, 63, 87, bytes.len() - 1] {
                assert!(!pe_has_no_certificate(&mut Cursor::new(
                    &bytes[..truncated_at]
                )));
            }
            bytes[88 + directory_start - 4..88 + directory_start]
                .copy_from_slice(&4u32.to_le_bytes());
            assert!(!pe_has_no_certificate(&mut Cursor::new(bytes)));
        }
    }

    #[test]
    fn native_verification_rejects_missing_malformed_and_nonfiles() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-signed.exe");
        std::fs::write(&file, b"not a signed portable executable").unwrap();
        for path in [
            file,
            temporary.path().join("missing.exe"),
            temporary.path().to_owned(),
            Path::new("relative.exe").to_owned(),
        ] {
            assert!(!verify(&path));
            assert!(!verify_or_unsigned(&path, true));
            assert!(!verify_startup_or_unsigned(&path, false));
            assert!(!verify_startup_or_unsigned(&path, true));
        }
    }

    #[test]
    fn native_unsigned_test_executable_requires_the_explicit_dev_exception() {
        // Cargo's test executable has not passed through the release signing
        // job. It exercises the real Windows provider, without running any
        // downloaded file or installing a test certificate into the OS store.
        let executable = std::env::current_exe().unwrap();
        assert!(!verify(&executable));
        assert!(verify_or_unsigned(&executable, true));
        assert!(!verify_startup_or_unsigned(&executable, false));
        assert!(verify_startup_or_unsigned(&executable, true));
    }

    #[test]
    fn native_microsoft_publisher_is_rejected_even_for_unsigned_dev() {
        // Native qualification separately proves this fixed Windows binary has
        // a valid Microsoft signature; another publisher cannot be a Dev bypass.
        let executable = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        assert!(executable.is_file());
        assert!(!verify(&executable));
        assert!(!verify_or_unsigned(&executable, true));
        assert!(!verify_startup_or_unsigned(&executable, false));
        assert!(!verify_startup_or_unsigned(&executable, true));
    }
}
