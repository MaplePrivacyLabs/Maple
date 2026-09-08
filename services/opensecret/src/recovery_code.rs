use crate::aws_credentials::AwsCredentialManager;
use crate::encrypt::{generate_random, generate_random_enclave};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

const RECOVERY_CODE_VERSION: u8 = 1;
const RECOVERY_CODE_PREFIX: &str = "MPLRC1";
const RECOVERY_CODE_GROUP_SIZE: usize = 4;

pub struct RecoveryCode {
    pub(crate) secret: Zeroizing<[u8; 32]>,
}

impl RecoveryCode {
    pub async fn generate(credentials: Option<Arc<RwLock<Option<AwsCredentialManager>>>>) -> Self {
        let secret = match credentials {
            Some(creds) => generate_random_enclave::<32>(creds).await,
            None => generate_random::<32>(),
        };
        Self {
            secret: Zeroizing::new(secret),
        }
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

    pub(super) fn encode(data: &[u8]) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_code_display_canonical_shape() {
        let secret = [0x55u8; 32];
        let code = RecoveryCode {
            secret: Zeroizing::new(secret),
        };
        let displayed = code.display();
        let parts: Vec<&str> = displayed.split('-').collect();
        // Prefix + 13 secret groups + 2 checksum groups = 16 parts
        assert_eq!(parts.len(), 16);
        assert_eq!(parts[0], "MPLRC1");
        for part in &parts[1..] {
            assert_eq!(part.len(), 4);
        }
    }
}
