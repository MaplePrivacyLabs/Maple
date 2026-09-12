use crate::aws_credentials::AwsCredentialManager;
use crate::encrypt::{generate_random, generate_random_enclave};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

const RECOVERY_CODE_VERSION: u8 = 1;
const RECOVERY_CODE_PREFIX: &str = "MPLRC1";
const RECOVERY_CODE_GROUP_SIZE: usize = 4;

#[derive(Debug, thiserror::Error)]
// `Invalid*` names keep every rejection reason self-describing for callers.
#[allow(clippy::enum_variant_names)]
pub enum RecoveryCodeError {
    #[error("Invalid format")]
    InvalidFormat,
    #[error("Invalid checksum")]
    InvalidChecksum,
    #[error("Invalid prefix")]
    InvalidPrefix,
    #[error("Invalid length")]
    InvalidLength,
    #[error("Invalid character")]
    InvalidCharacter,
    #[error("Invalid padding")]
    InvalidPadding,
}

pub struct RecoveryCode {
    pub(crate) secret: Zeroizing<[u8; 32]>,
}

impl RecoveryCode {
    pub async fn generate(
        credentials: Option<Arc<RwLock<Option<AwsCredentialManager>>>>,
    ) -> Result<Self, RecoveryCodeError> {
        let secret = match credentials {
            Some(creds) => generate_random_enclave::<32>(creds).await,
            None => generate_random::<32>(),
        };
        Ok(Self {
            secret: Zeroizing::new(secret),
        })
    }

    /// Parses a user-submitted code, rejecting malformed input before any
    /// database work. Consumed by the recovery reset completion flow.
    pub fn parse(input: &str) -> Result<Self, RecoveryCodeError> {
        let normalized: String = input.chars().filter(|c| *c != ' ' && *c != '-').collect();

        let normalized_upper = normalized.to_ascii_uppercase();
        if !normalized_upper.starts_with(RECOVERY_CODE_PREFIX) {
            return Err(RecoveryCodeError::InvalidPrefix);
        }

        let payload = &normalized_upper[RECOVERY_CODE_PREFIX.len()..];

        // 52 chars for secret + 8 chars for checksum = 60 chars
        if payload.len() != 60 {
            return Err(RecoveryCodeError::InvalidLength);
        }

        let secret_part = &payload[..52];
        let checksum_part = &payload[52..];

        let secret_bytes = crockford::decode(secret_part)?;
        if secret_bytes.len() != 32 {
            return Err(RecoveryCodeError::InvalidFormat);
        }

        let checksum_bytes = crockford::decode(checksum_part)?;
        if checksum_bytes.len() != 5 {
            return Err(RecoveryCodeError::InvalidFormat);
        }

        let mut secret = [0u8; 32];
        secret.copy_from_slice(&secret_bytes);

        // Verify checksum
        let expected_checksum = compute_checksum(&secret);
        if expected_checksum != checksum_bytes.as_slice() {
            return Err(RecoveryCodeError::InvalidChecksum);
        }

        Ok(Self {
            secret: Zeroizing::new(secret),
        })
    }

    pub fn display(&self) -> Zeroizing<String> {
        let secret_encoded = crockford::encode(&self.secret[..]);
        let checksum = compute_checksum(&self.secret);
        let checksum_encoded = crockford::encode(&checksum);

        let mut groups: Vec<String> = Vec::new();
        // Prefix
        groups.push(RECOVERY_CODE_PREFIX.to_string());
        // Secret groups of 4
        for chunk in secret_encoded.as_bytes().chunks(RECOVERY_CODE_GROUP_SIZE) {
            groups.push(String::from_utf8_lossy(chunk).to_string());
        }
        // Checksum groups of 4
        for chunk in checksum_encoded.as_bytes().chunks(RECOVERY_CODE_GROUP_SIZE) {
            groups.push(String::from_utf8_lossy(chunk).to_string());
        }

        Zeroizing::new(groups.join("-"))
    }

    pub(crate) fn secret_bytes(&self) -> &[u8; 32] {
        &self.secret
    }
}

fn compute_checksum(secret: &[u8; 32]) -> [u8; 5] {
    let mut hasher = Sha256::new();
    hasher.update([RECOVERY_CODE_VERSION]);
    hasher.update(secret);
    let hash = hasher.finalize();
    let mut checksum = [0u8; 5];
    checksum.copy_from_slice(&hash[..5]);
    checksum
}

mod crockford {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
        let mut buffer = 0u64;
        let mut bits = 0;

        for &byte in data {
            buffer = (buffer << 8) | (byte as u64);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                let val = ((buffer >> bits) & 0x1F) as usize;
                out.push(ALPHABET[val] as char);
            }
        }

        if bits > 0 {
            let val = ((buffer << (5 - bits)) & 0x1F) as usize;
            out.push(ALPHABET[val] as char);
        }

        out
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, super::RecoveryCodeError> {
        let mut out = Vec::with_capacity(input.len() * 5 / 8);
        let mut buffer = 0u64;
        let mut bits = 0;

        for ch in input.chars() {
            let val = decode_char(ch)?;
            buffer = (buffer << 5) | (val as u64);
            bits += 5;
            while bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
            }
        }

        // Verify padding bits are zero
        if bits > 0 {
            let padding = buffer & ((1u64 << bits) - 1);
            if padding != 0 {
                return Err(super::RecoveryCodeError::InvalidPadding);
            }
        }

        Ok(out)
    }

    fn decode_char(ch: char) -> Result<u8, super::RecoveryCodeError> {
        match ch {
            '0' | 'O' | 'o' => Ok(0),
            '1' | 'I' | 'i' | 'L' | 'l' => Ok(1),
            '2' => Ok(2),
            '3' => Ok(3),
            '4' => Ok(4),
            '5' => Ok(5),
            '6' => Ok(6),
            '7' => Ok(7),
            '8' => Ok(8),
            '9' => Ok(9),
            'A' | 'a' => Ok(10),
            'B' | 'b' => Ok(11),
            'C' | 'c' => Ok(12),
            'D' | 'd' => Ok(13),
            'E' | 'e' => Ok(14),
            'F' | 'f' => Ok(15),
            'G' | 'g' => Ok(16),
            'H' | 'h' => Ok(17),
            'J' | 'j' => Ok(18),
            'K' | 'k' => Ok(19),
            'M' | 'm' => Ok(20),
            'N' | 'n' => Ok(21),
            'P' | 'p' => Ok(22),
            'Q' | 'q' => Ok(23),
            'R' | 'r' => Ok(24),
            'S' | 's' => Ok(25),
            'T' | 't' => Ok(26),
            'V' | 'v' => Ok(27),
            'W' | 'w' => Ok(28),
            'X' | 'x' => Ok(29),
            'Y' | 'y' => Ok(30),
            'Z' | 'z' => Ok(31),
            _ => Err(super::RecoveryCodeError::InvalidCharacter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_code_generate_roundtrip() {
        let code = RecoveryCode::generate(None).await.unwrap();
        let displayed = code.display();
        let parsed = RecoveryCode::parse(&displayed).unwrap();
        assert_eq!(*code.secret_bytes(), *parsed.secret_bytes());
    }

    #[test]
    fn recovery_code_roundtrip() {
        let secret = [0xABu8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let displayed = code.display();
        let parsed = RecoveryCode::parse(&displayed).unwrap();
        assert_eq!(*code.secret_bytes(), *parsed.secret_bytes());
    }

    #[test]
    fn recovery_code_parsing_is_case_insensitive() {
        let secret = [0xCDu8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let displayed = code.display();
        let lower = displayed.to_ascii_lowercase();
        let parsed = RecoveryCode::parse(&lower).unwrap();
        assert_eq!(*code.secret_bytes(), *parsed.secret_bytes());
    }

    #[test]
    fn recovery_code_parsing_ignores_spaces_and_hyphens() {
        let secret = [0xEFu8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let displayed = code.display();
        let mangled = displayed
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 5 == 0 && c != '-' {
                    format!(" {} ", c)
                } else {
                    c.to_string()
                }
            })
            .collect::<String>();
        let parsed = RecoveryCode::parse(&mangled).unwrap();
        assert_eq!(*code.secret_bytes(), *parsed.secret_bytes());
    }

    #[test]
    fn recovery_code_rejects_wrong_prefix() {
        let input = "WRONG1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000";
        assert!(matches!(
            RecoveryCode::parse(input),
            Err(RecoveryCodeError::InvalidPrefix)
        ));
    }

    #[test]
    fn recovery_code_rejects_invalid_character() {
        let input =
            "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-UUUU-UUUU";
        let result = RecoveryCode::parse(input);
        assert!(matches!(result, Err(RecoveryCodeError::InvalidCharacter)));
    }

    #[test]
    fn recovery_code_rejects_wrong_checksum() {
        let secret = [0x12u8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let mut displayed = code.display().to_string();
        // Flip the last char of the checksum
        let last_char = displayed.pop().unwrap();
        let new_last = if last_char == '0' { '1' } else { '0' };
        displayed.push(new_last);
        assert!(matches!(
            RecoveryCode::parse(&displayed),
            Err(RecoveryCodeError::InvalidChecksum)
        ));
    }

    #[test]
    fn recovery_code_rejects_invalid_length() {
        assert!(matches!(
            RecoveryCode::parse("MPLRC1-0000"),
            Err(RecoveryCodeError::InvalidLength)
        ));
    }

    #[test]
    fn recovery_code_format_fields_count() {
        let secret = [0x55u8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let displayed = code.display();
        let parts: Vec<&str> = displayed.split('-').collect();
        // Prefix + 13 secret groups + 2 checksum groups = 16 parts
        assert_eq!(parts.len(), 16);
        assert_eq!(parts[0], "MPLRC1");
        for part in &parts[1..=13] {
            assert_eq!(part.len(), 4);
        }
        for part in &parts[14..=15] {
            assert_eq!(part.len(), 4);
        }
    }
}
