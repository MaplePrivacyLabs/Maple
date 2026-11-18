use maple_sdk::crypto::{derive_shared_secret, generate_key_pair};

#[test]
fn test_key_generation() {
    let keypair = generate_key_pair();

    // Public key should be 32 bytes
    assert_eq!(keypair.public.as_bytes().len(), 32);

    // Secret key should be 32 bytes
    assert_eq!(keypair.secret.as_bytes().len(), 32);

    // Keys should be different
    assert_ne!(keypair.public.as_bytes(), keypair.secret.as_bytes());
}

#[test]
fn test_ecdh_key_exchange() {
    let alice = generate_key_pair();
    let bob = generate_key_pair();

    // Derive shared secrets
    let alice_shared = derive_shared_secret(&alice.secret, &bob.public);
    let bob_shared = derive_shared_secret(&bob.secret, &alice.public);

    // Both should derive the same shared secret
    assert_eq!(alice_shared.as_bytes(), bob_shared.as_bytes());

    // Shared secret should be 32 bytes
    assert_eq!(alice_shared.as_bytes().len(), 32);
}
