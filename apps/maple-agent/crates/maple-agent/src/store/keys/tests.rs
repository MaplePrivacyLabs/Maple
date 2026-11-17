use super::*;
use std::collections::HashSet;

fn vector() -> serde_json::Value {
    serde_json::from_str(include_str!("slot-vector.json")).unwrap()
}

fn vector_slot() -> KeyFile {
    let vector = vector();
    let salt: [u8; 16] = std::array::from_fn(|ix| 160 + ix as u8);
    KeyFile::parse(&serde_json::to_vec(&vector["key_file"]).unwrap(), &salt).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn independent_reference_vector_matches_canonical_bytes_checksum_kdf_and_unwrap() {
    let expected = vector();
    let slot = vector_slot();
    assert_eq!(hex(&slot.slot_bytes()), expected["slot_bytes_hex"]);
    assert_eq!(hex(&slot.checksum()), expected["check_hex"]);
    let password = Password::normalize(expected["password"].as_str().unwrap()).unwrap();
    assert_eq!(password.as_str(), expected["normalized_password"]);
    let kek = derive_kek(&password, &slot.salt, slot.params).unwrap();
    assert_eq!(hex(kek.as_bytes()), expected["kek_hex"]);
    drop(kek);
    let key = slot.unlock(&password).unwrap();
    assert_eq!(hex(key.as_bytes()), expected["dek_hex"]);
    // The reverse direction must produce the exact libsodium ciphertext too.
    let recreated = KeyFile::create_with_randomness(
        &password,
        &key,
        slot.db_salt,
        slot.params,
        slot.salt,
        slot.nonce,
    )
    .unwrap();
    assert_eq!(recreated, slot);
    assert!(matches!(
        slot.unlock(&Password::normalize("a different password entirely").unwrap()),
        Err(KeyError::WrongPassword),
    ));
}

#[test]
fn metadata_damage_bounds_and_database_identity_precede_any_kdf() {
    let slot = vector_slot();
    let encoded = slot.to_json().unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    json["extra_future_metadata"] = serde_json::json!({ "public": "ignored" });
    let reordered = serde_json::to_vec(&json).unwrap();
    assert_eq!(KeyFile::parse(&reordered, &slot.db_salt).unwrap(), slot);

    for field in ["nonce", "wrapped", "check"] {
        let mut damaged = json.clone();
        let value = damaged["slots"][0][field].as_str().unwrap();
        let mut value = value.as_bytes().to_vec();
        value[0] = if value[0] == b'A' { b'B' } else { b'A' };
        damaged["slots"][0][field] = String::from_utf8(value).unwrap().into();
        assert_eq!(
            KeyFile::parse(&serde_json::to_vec(&damaged).unwrap(), &slot.db_salt),
            Err(KeyError::Damaged)
        );
    }
    let mut changed = slot.clone();
    changed.db_salt[0] ^= 1;
    // An accidental salt edit fails the checksum before the mismatch check.
    assert_eq!(
        KeyFile::parse(&changed.to_json().unwrap(), &slot.db_salt),
        Err(KeyError::Damaged)
    );
    changed.check = changed.checksum();
    assert_eq!(
        KeyFile::parse(&changed.to_json().unwrap(), &slot.db_salt),
        Err(KeyError::MismatchedDatabase)
    );
    for params in [
        KdfParams {
            m_kib: u32::MAX,
            t: 3,
            p: 4,
        },
        KdfParams {
            m_kib: 65535,
            t: 3,
            p: 4,
        },
        KdfParams {
            m_kib: 65536,
            t: u32::MAX,
            p: 4,
        },
        KdfParams {
            m_kib: 65536,
            t: 3,
            p: 0,
        },
    ] {
        let mut changed = slot.clone();
        changed.params = params;
        changed.check = changed.checksum();
        assert_eq!(
            KeyFile::parse(&changed.to_json().unwrap(), &slot.db_salt),
            Err(KeyError::Damaged)
        );
    }
    json["slots"] = serde_json::json!([]);
    assert_eq!(
        KeyFile::parse(&serde_json::to_vec(&json).unwrap(), &slot.db_salt),
        Err(KeyError::Damaged)
    );
    assert_eq!(
        KeyFile::parse(&vec![b' '; KEY_FILE_LIMIT as usize + 1], &slot.db_salt),
        Err(KeyError::Damaged)
    );
    assert_eq!(
        decode::<16>("oKGio6SlpqeoqaqrrK2urw=="),
        Err(KeyError::Damaged)
    );
}

#[test]
fn authenticating_metadata_prevents_downgrades_even_after_public_checksum_is_recomputed() {
    let mut slot = vector_slot();
    slot.params.t = 1;
    slot.check = slot.checksum();
    let slot = KeyFile::parse(&slot.to_json().unwrap(), &slot.db_salt).unwrap();
    let password = Password::normalize(vector()["password"].as_str().unwrap()).unwrap();
    assert!(matches!(
        slot.unlock(&password),
        Err(KeyError::WrongPassword)
    ));
}

#[test]
fn keyfile_pair_repairs_torn_changes_and_recovers_without_a_password() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = super::super::StorePaths::new(temporary.path())
        .prepare()
        .unwrap();
    let directory = paths.db();
    let original = vector_slot();
    KeyFilePair::write(&directory, &original).unwrap();
    for name in ["keys.json", "keys.backup.json"] {
        verify_private_path(&directory.join(name), false).unwrap();
    }
    let mut changed = original.clone();
    changed.nonce[0] ^= 1;
    changed.check = changed.checksum();
    // Public metadata is enough to test the commit protocol; no KDF is needed.
    let outcome = KeyFilePair::write_with(&directory, &changed, |path, key| {
        if path.file_name().unwrap() == "keys.backup.json" {
            Err(KeyError::Io {
                kind: io::ErrorKind::PermissionDenied,
            })
        } else {
            write_key_file(path, key)
        }
    })
    .unwrap();
    assert!(outcome.backup_pending);
    assert_eq!(
        read_key_file(&directory.join("keys.json"), &original.db_salt).unwrap(),
        changed
    );
    assert_eq!(
        read_key_file(&directory.join("keys.backup.json"), &original.db_salt).unwrap(),
        original
    );
    let pair = KeyFilePair::load(&directory, &original.db_salt).unwrap();
    assert_eq!(*pair.primary(), changed);
    assert!(!pair.reconciliation_pending());
    assert_eq!(
        read_key_file(&directory.join("keys.backup.json"), &original.db_salt).unwrap(),
        changed
    );

    fs::remove_file(directory.join("keys.json")).unwrap();
    let recovered = KeyFilePair::load(&directory, &original.db_salt).unwrap();
    assert_eq!(*recovered.primary(), changed);
    assert!(directory.join("keys.json").is_file());
    // Corruption is restored from the valid backup without starting an empty store.
    fs::write(directory.join("keys.json"), b"damaged JSON").unwrap();
    assert_eq!(
        *KeyFilePair::load(&directory, &original.db_salt)
            .unwrap()
            .primary(),
        changed
    );
    fs::write(directory.join("keys.json"), b"damaged JSON").unwrap();
    fs::write(directory.join("keys.backup.json"), b"damaged JSON").unwrap();
    assert!(matches!(
        KeyFilePair::load(&directory, &original.db_salt),
        Err(KeyError::Damaged)
    ));
    assert!(directory.join("keys.json").exists());
}

#[test]
fn previous_password_fallback_retains_authoritative_new_slot_when_reconcile_is_blocked() {
    let old = vector_slot();
    let old_password = Password::normalize(vector()["password"].as_str().unwrap()).unwrap();
    let dek = old.unlock(&old_password).unwrap();
    let new_password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    let new = KeyFile::create(&new_password, &[], &dek, old.db_salt, KdfParams::floor()).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let mut pair = KeyFilePair {
        directory: temporary.path().join("missing"),
        primary: new.clone(),
        previous: Some(old),
        reconciliation_pending: true,
    };
    let result = pair.unlock(&old_password).unwrap();
    assert_eq!(result.key.as_bytes(), dek.as_bytes());
    assert!(result.used_previous_password);
    assert!(result.reconciliation_pending);
    assert_eq!(*pair.primary(), new);
    // Once reconciliation can finish, old-password fallback is retired.
    let paths = super::super::StorePaths::new(temporary.path())
        .prepare()
        .unwrap();
    pair.directory = paths.db();
    pair.reconcile().unwrap();
    assert!(matches!(
        pair.unlock(&old_password),
        Err(KeyError::WrongPassword)
    ));
    assert!(!pair.unlock(&new_password).unwrap().used_previous_password);
}

#[cfg(unix)]
#[test]
fn atomic_private_writes_refuse_symlinks_hardlinks_and_exposed_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temporary = tempfile::tempdir().unwrap();
    let paths = super::super::StorePaths::new(temporary.path())
        .prepare()
        .unwrap();
    let slot = vector_slot();
    let target = temporary.path().join("untouched");
    fs::write(&target, b"leave intact").unwrap();
    symlink(&target, paths.keys()).unwrap();
    assert_eq!(
        write_key_file(&paths.keys(), &slot),
        Err(KeyError::UnsafePath)
    );
    assert_eq!(fs::read(&target).unwrap(), b"leave intact");
    fs::remove_file(paths.keys()).unwrap();
    fs::hard_link(&target, paths.keys()).unwrap();
    assert_eq!(
        write_key_file(&paths.keys(), &slot),
        Err(KeyError::UnsafePath)
    );
    fs::remove_file(paths.keys()).unwrap();
    fs::write(paths.keys(), b"public").unwrap();
    fs::set_permissions(paths.keys(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        write_key_file(&paths.keys(), &slot),
        Err(KeyError::UnsafePath)
    );
    fs::remove_file(paths.keys()).unwrap();
    write_key_file(&paths.keys(), &slot).unwrap();
    assert_eq!(
        fs::metadata(paths.keys()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(fs::read_dir(paths.db()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}

#[test]
fn calibration_is_memory_first_has_work_floor_and_respects_portable_write_cap() {
    let mut probed = Vec::new();
    let high_memory = KdfParams::calibrate_with(64 * 1024_u64.pow(3), |params| {
        probed.push(params);
        Ok(Duration::from_millis(100 * u64::from(params.t)))
    })
    .unwrap();
    assert_eq!(
        high_memory,
        KdfParams {
            m_kib: 1024 * 1024,
            t: 1,
            p: 4
        }
    );
    assert_eq!(probed.len(), 1);
    let low_memory = KdfParams::calibrate_with(512 * 1024_u64.pow(2), |params| {
        Ok(Duration::from_millis(100 * u64::from(params.t)))
    })
    .unwrap();
    low_memory.validate_for_write().unwrap();
    assert_eq!(low_memory.m_kib, 64 * 1024);
    assert!(low_memory.traffic_kib() >= FLOOR_TRAFFIC_KIB);
    assert!(low_memory.traffic_kib() <= STORED_TRAFFIC_CAP_KIB);
    let slow = KdfParams::calibrate_with(64 * 1024_u64.pow(3), |params| {
        Ok(if params.m_kib >= 256 * 1024 {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(1)
        })
    })
    .unwrap();
    assert_eq!(
        slow,
        KdfParams {
            m_kib: 128 * 1024,
            t: 2,
            p: 4
        }
    );
    let failed = KdfParams::calibrate_with(64 * 1024_u64.pow(3), |params| {
        if params.m_kib > 64 * 1024 {
            Err(KeyError::NotEnoughMemory {
                required_mib: params.m_kib / 1024,
            })
        } else {
            Ok(Duration::from_secs(1))
        }
    })
    .unwrap();
    assert_eq!(failed, KdfParams::floor());
    assert!(!high_memory.should_raise_to(KdfParams::floor()));
    assert!(KdfParams::floor().should_raise_to(high_memory));
    assert!(
        KdfParams {
            m_kib: 65536,
            t: 1,
            p: 4
        }
        .validate_for_write()
        .is_err()
    );
    assert!(
        KdfParams {
            m_kib: 1048576,
            t: 6,
            p: 4
        }
        .validate_for_write()
        .is_err()
    );
}

#[test]
fn password_normalization_preserves_unlock_and_set_length_policies() {
    assert_eq!(
        assess_password("too short", &[]).unwrap().reason,
        StrengthReason::Length
    );
    // The minimum is a set policy; unlock normalization continues to permit
    // shorter existing passwords. Code points are counted after NFC.
    assert!(Password::normalize("short").is_ok());
    assert_ne!(
        assess_password("abcdefghijklmno", &[]).unwrap().reason,
        StrengthReason::Length
    );
    assert_eq!(Password::normalize("e\u{301}").unwrap().as_str(), "é");
    assert!(Password::normalize(&"é".repeat(1024)).is_ok());
    let long = "é".repeat(1025);
    assert!(matches!(
        Password::normalize(&long),
        Err(KeyError::InvalidPassword)
    ));
    assert!(matches!(
        Password::normalize(&"a".repeat(8193)),
        Err(KeyError::InvalidPassword)
    ));
}

#[test]
fn password_score_thresholds_and_invalid_estimates_are_fail_closed() {
    for (guesses_log10, expected) in [
        (11.999, StrengthLevel::TooEasy),
        (12.0, StrengthLevel::CouldBeStronger),
        (13.999, StrengthLevel::CouldBeStronger),
        (14.0, StrengthLevel::Strong),
        (f64::NAN, StrengthLevel::TooEasy),
        (f64::INFINITY, StrengthLevel::TooEasy),
        (f64::NEG_INFINITY, StrengthLevel::TooEasy),
    ] {
        let assessment = assessment_from_guesses(guesses_log10);
        assert_eq!(assessment.level, expected);
        assert_eq!(assessment.allowed(), expected != StrengthLevel::TooEasy);
        assert!(assessment.guesses_log10.is_finite());
    }
}

#[test]
fn password_estimation_rejects_obvious_patterns_and_uses_account_context() {
    for password in ["passwordpassword", "1QAZ2WSX3EDC4RFV", "aaaaaaaaaaaaaaa"] {
        let assessment = assess_password(password, &[]).unwrap();
        assert_eq!(assessment.level, StrengthLevel::TooEasy);
        assert_eq!(assessment.reason, StrengthReason::Guesses);
    }
    let password = "Z7jv!9qP#3nMx@8Ks^2dH";
    assert_eq!(
        assess_password(password, &[]).unwrap().level,
        StrengthLevel::Strong
    );
    assert!(
        assess_password(password, &[password])
            .unwrap()
            .guesses_log10
            < 12.0
    );
}

#[test]
fn typed_phrase_policy_uses_the_estimator_without_a_separate_dictionary_cap() {
    // This deliberately records the accepted policy relaxation. It is a
    // zxcvbn estimate, not a promise about an attacker's guessing cost.
    let assessment = assess_password("my cat is named biscuit", &[]).unwrap();
    assert_eq!(assessment.level, StrengthLevel::Strong);
    assert!(assessment.allowed());
}

#[test]
fn generator_rejects_tail_values_and_emits_six_uniform_dictionary_words() {
    struct BoundaryRng {
        calls: usize,
    }
    impl RngCore for BoundaryRng {
        fn next_u32(&mut self) -> u32 {
            unreachable!()
        }
        fn next_u64(&mut self) -> u64 {
            unreachable!()
        }
        fn fill_bytes(&mut self, _: &mut [u8]) {
            unreachable!()
        }
        fn try_fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), rand::Error> {
            self.calls += 1;
            destination.fill(if self.calls == 1 { 255 } else { 0 });
            Ok(())
        }
    }
    let mut rng = BoundaryRng { calls: 0 };
    let generated = generate_with_rng(&mut rng).unwrap();
    assert_eq!(rng.calls, 7);
    assert_eq!(
        generated.password().as_str(),
        "abacus abacus abacus abacus abacus abacus"
    );
    assert_eq!(generated.assessment().level, StrengthLevel::Strong);
    assert!(generated.assessment().guesses_log10 > 23.0);
    // Identical text supplied as a typed password receives the ordinary score.
    assert_eq!(
        generated.password().assess(&[]).level,
        StrengthLevel::TooEasy
    );
    assert_eq!(EFF_WORDS.len(), 7776);
    assert_eq!(
        EFF_WORDS.iter().copied().collect::<HashSet<_>>().len(),
        7776
    );
}

#[test]
fn raw_key_reader_writes_fragmented_input_directly_into_the_locked_mapping() {
    struct Fragments {
        position: usize,
        first_destination: Option<usize>,
    }
    impl io::Read for Fragments {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.first_destination
                .get_or_insert(output.as_mut_ptr() as usize);
            let count = output.len().min(3).min(33 - self.position);
            for byte in &mut output[..count] {
                *byte = self.position as u8;
                self.position += 1;
            }
            Ok(count)
        }
    }
    let mut input = Fragments {
        position: 0,
        first_destination: None,
    };
    let key = LockedKey::read_from(&mut input).unwrap();
    assert_eq!(input.first_destination, Some(key.memory.as_ptr() as usize));
    assert_eq!(
        input.position, 32,
        "the reader must not consume trailing data"
    );
    assert_eq!(key.as_bytes(), &std::array::from_fn(|index| index as u8));
}

#[test]
fn raw_key_reader_rejects_short_input_and_preserves_io_failures() {
    for length in 0..32 {
        let mut input = io::Cursor::new(vec![0x5a; length]);
        assert!(matches!(
            LockedKey::read_from(&mut input),
            Err(KeyError::Io {
                kind: io::ErrorKind::UnexpectedEof
            })
        ));
    }
    struct BrokenPipe(bool);
    impl io::Read for BrokenPipe {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            output[0] = 0x5a;
            Ok(1)
        }
    }
    assert!(matches!(
        LockedKey::read_from(&mut BrokenPipe(false)),
        Err(KeyError::Io {
            kind: io::ErrorKind::BrokenPipe
        })
    ));
}

#[test]
fn key_and_password_debug_and_errors_are_redacted_and_raw_key_is_exact() {
    let key = LockedKey::from_bytes(std::array::from_fn(|ix| ix as u8)).unwrap();
    let password = Password::normalize("public test marker, never print this string").unwrap();
    for formatted in [format!("{key:?}"), format!("{password:?}")] {
        assert!(formatted.contains("REDACTED"));
        assert!(!formatted.contains("public test marker"));
    }
    key.with_sqlcipher_key(|bytes| {
        assert_eq!(bytes.len(), 67);
        assert_eq!(&bytes[..2], b"x'");
        assert_eq!(bytes[66], b'\'');
        assert_eq!(
            &bytes[2..66],
            b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
    })
    .unwrap();
    let errors = [
        KeyError::WrongPassword,
        KeyError::Damaged,
        KeyError::MismatchedDatabase,
        KeyError::InvalidPassword,
        KeyError::WeakPassword,
        KeyError::MemoryLock,
        KeyError::NotEnoughMemory { required_mib: 64 },
    ];
    for error in errors {
        assert!(std::error::Error::source(&error).is_none());
        assert!(!format!("{error:?}: {error}").contains("public test marker"));
    }
}

#[test]
fn a_key_rename_followed_by_sync_failure_reports_the_installed_password() {
    fn fail_sync(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let paths = super::super::StorePaths::new(directory.path())
        .prepare()
        .unwrap();
    let slot = vector_slot();
    let error = write_key_file_with_sync(&paths.keys(), &slot, fail_sync).unwrap_err();
    assert!(matches!(error, KeyError::SynchronizationPending { .. }));
    assert_eq!(read_key_file(&paths.keys(), &slot.db_salt).unwrap(), slot);
    let outcome = KeyFilePair::write_with(&paths.db(), &slot, |path, key| {
        write_key_file_with_sync(path, key, fail_sync)
    })
    .unwrap();
    assert!(outcome.backup_pending);
    assert!(outcome.synchronization_pending);
    // A later successful backup+directory sync confirms both installed names.
    let mut primary = true;
    let outcome = KeyFilePair::write_with(&paths.db(), &slot, |path, key| {
        if primary {
            primary = false;
            write_key_file_with_sync(path, key, fail_sync)
        } else {
            write_key_file(path, key)
        }
    })
    .unwrap();
    assert!(!outcome.backup_pending);
    assert!(!outcome.synchronization_pending);
}

/// Windows locks about 40 pages per process by default and every key holds
/// one, so parallel tests that each opened a store ran out and failed with a
/// key-memory-lock error. Far more keys than that must be held at once.
#[test]
fn many_keys_can_be_held_at_once() {
    let keys = (0..512)
        .map(|_| LockedKey::random())
        .collect::<KeyResult<Vec<_>>>()
        .unwrap();
    assert_eq!(keys.len(), 512);
    keys[0]
        .with_sqlcipher_key(|bytes| assert_eq!(bytes.len(), 67))
        .unwrap();
}
