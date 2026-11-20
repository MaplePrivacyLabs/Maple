use rand_core::{OsRng, RngCore};
use x25519_dalek::{PublicKey as X25519PublicKey, SharedSecret, StaticSecret};

// Re-export for tests
pub use x25519_dalek::PublicKey;

// Public test utilities
pub struct KeyPair {
    pub secret: StaticSecret,
    pub public: X25519PublicKey,
}

pub fn generate_key_pair() -> KeyPair {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = X25519PublicKey::from(&secret);
    KeyPair { secret, public }
}

pub fn derive_shared_secret(secret: &StaticSecret, their_public: &X25519PublicKey) -> SharedSecret {
    secret.diffie_hellman(their_public)
}

pub fn generate_random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_exchange() {
        let alice = generate_key_pair();
        let bob = generate_key_pair();

        let alice_shared = derive_shared_secret(&alice.secret, &bob.public);
        let bob_shared = derive_shared_secret(&bob.secret, &alice.public);

        assert_eq!(alice_shared.as_bytes(), bob_shared.as_bytes());
    }
}
