use proptest::{collection::vec, prelude::*, test_runner::Config};
use uuid::Uuid;

use crate::{
    encrypt::{decrypt_aead_v1, encrypt_aead_v1, CanonicalBytes},
    seed_wrapping::{
        decrypt_seed_v1, encrypt_seed_v1, normalize_email_login_identifier, AuthBinding,
        CredentialKind,
    },
};

const PROPERTY_CASES: u32 = 64;

fn property_config() -> Config {
    Config {
        cases: PROPERTY_CASES,
        max_shrink_iters: 512,
        ..Config::default()
    }
}

fn credential_kind() -> impl Strategy<Value = CredentialKind> {
    prop_oneof![Just(CredentialKind::Password), Just(CredentialKind::OAuth)]
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn aead_round_trips_arbitrary_plaintext_and_aad(
        key in any::<[u8; 32]>(),
        plaintext in vec(any::<u8>(), 0..513),
        aad in vec(any::<u8>(), 0..129),
    ) {
        let encrypted = encrypt_aead_v1(&key, &plaintext, &aad)
            .expect("bounded plaintext should encrypt");
        let decrypted = decrypt_aead_v1(&key, &encrypted, &aad)
            .expect("matching key and AAD should decrypt");

        prop_assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn aead_rejects_any_single_bit_ciphertext_tamper(
        key in any::<[u8; 32]>(),
        plaintext in vec(any::<u8>(), 0..513),
        aad in vec(any::<u8>(), 0..129),
        offset in any::<usize>(),
        bit in 0u8..8,
    ) {
        let mut encrypted = encrypt_aead_v1(&key, &plaintext, &aad)
            .expect("bounded plaintext should encrypt");
        let tampered_offset = offset % encrypted.len();
        encrypted[tampered_offset] ^= 1 << bit;

        prop_assert!(decrypt_aead_v1(&key, &encrypted, &aad).is_err());
    }

    #[test]
    fn aead_rejects_changed_aad(
        key in any::<[u8; 32]>(),
        plaintext in vec(any::<u8>(), 0..513),
        aad in vec(any::<u8>(), 0..129),
    ) {
        let encrypted = encrypt_aead_v1(&key, &plaintext, &aad)
            .expect("bounded plaintext should encrypt");
        let mut changed_aad = aad.clone();
        changed_aad.push(0);

        prop_assert!(decrypt_aead_v1(&key, &encrypted, &changed_aad).is_err());
    }

    #[test]
    fn canonical_bytes_unambiguously_frame_field_boundaries(
        prefix in vec(any::<u8>(), 0..65),
        moved_bytes in vec(any::<u8>(), 1..65),
        suffix in vec(any::<u8>(), 0..65),
    ) {
        let mut left_first_field = prefix.clone();
        left_first_field.extend_from_slice(&moved_bytes);
        let mut right_second_field = moved_bytes;
        right_second_field.extend_from_slice(&suffix);

        let mut left = CanonicalBytes::new("property-test-domain");
        left.append_bytes(&left_first_field).append_bytes(&suffix);

        let mut right = CanonicalBytes::new("property-test-domain");
        right.append_bytes(&prefix).append_bytes(&right_second_field);

        // Both logical sequences contain identical concatenated payload bytes;
        // only their field boundaries differ.
        prop_assert_ne!(left.into_bytes(), right.into_bytes());
    }

    #[test]
    fn seed_wrap_round_trips_and_rejects_changed_context(
        root_key in any::<[u8; 32]>(),
        alternate_root_key in any::<[u8; 32]>(),
        plaintext_seed in vec(any::<u8>(), 0..257),
        user_id in any::<u128>(),
        project_id in any::<i32>(),
        kind in credential_kind(),
        auth_binding_bytes in any::<[u8; 32]>(),
        alternate_auth_binding_bytes in any::<[u8; 32]>(),
    ) {
        prop_assume!(alternate_root_key != root_key);
        prop_assume!(alternate_auth_binding_bytes != auth_binding_bytes);

        let user_uuid = Uuid::from_u128(user_id);
        let auth_binding = AuthBinding::from_bytes(auth_binding_bytes);
        let encrypted = encrypt_seed_v1(
            &root_key,
            &plaintext_seed,
            user_uuid,
            project_id,
            kind,
            &auth_binding,
        )
        .expect("bounded seed should encrypt");

        let decrypted = decrypt_seed_v1(
            &root_key,
            &encrypted,
            user_uuid,
            project_id,
            kind,
            &auth_binding,
        )
        .expect("matching seed-wrap context should decrypt");
        prop_assert_eq!(decrypted, plaintext_seed);

        let changed_user_uuid = Uuid::from_u128(user_id ^ 1);
        prop_assert!(decrypt_seed_v1(
            &root_key,
            &encrypted,
            changed_user_uuid,
            project_id,
            kind,
            &auth_binding,
        )
        .is_err());

        prop_assert!(decrypt_seed_v1(
            &root_key,
            &encrypted,
            user_uuid,
            project_id.wrapping_add(1),
            kind,
            &auth_binding,
        )
        .is_err());

        let changed_kind = match kind {
            CredentialKind::Password => CredentialKind::OAuth,
            CredentialKind::OAuth => CredentialKind::Recovery,
            CredentialKind::Recovery => CredentialKind::Password,
        };
        prop_assert!(decrypt_seed_v1(
            &root_key,
            &encrypted,
            user_uuid,
            project_id,
            changed_kind,
            &auth_binding,
        )
        .is_err());

        let changed_auth_binding = AuthBinding::from_bytes(alternate_auth_binding_bytes);
        prop_assert!(decrypt_seed_v1(
            &root_key,
            &encrypted,
            user_uuid,
            project_id,
            kind,
            &changed_auth_binding,
        )
        .is_err());

        prop_assert!(decrypt_seed_v1(
            &alternate_root_key,
            &encrypted,
            user_uuid,
            project_id,
            kind,
            &auth_binding,
        )
        .is_err());
    }

    #[test]
    fn email_normalization_is_idempotent(
        email in vec(any::<char>(), 0..129).prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let normalized = normalize_email_login_identifier(&email);

        prop_assert_eq!(normalize_email_login_identifier(&normalized), normalized);
    }
}
