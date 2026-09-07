use crate::{
    db::{setup_db, DBConnection, DBError},
    models::{
        org_projects::OrgProject,
        schema::org_projects,
        user_seed_wrappings::NewUserSeedWrapping,
        users::{NewUser, User},
    },
    recovery_code::RecoveryCode,
    seed_wrapping::{new_recovery_seed_wrapping, CredentialKind},
};

use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

const TEST_ROOT_KEY: [u8; 32] = [42u8; 32];

fn recovery_code_fixture(secret: [u8; 32]) -> RecoveryCode {
    RecoveryCode {
        secret: Zeroizing::new(secret),
    }
}

fn build_db(database_url: String) -> Arc<dyn DBConnection + Send + Sync> {
    setup_db(database_url)
}

fn first_active_project(db: &Arc<dyn DBConnection + Send + Sync>) -> OrgProject {
    let conn = &mut db
        .get_pool()
        .get()
        .expect("test database connection should be available");

    org_projects::table
        .filter(org_projects::status.eq("active"))
        .order(org_projects::id.asc())
        .first::<OrgProject>(conn)
        .expect("test database should contain at least one active project")
}

fn create_test_user(
    db: &Arc<dyn DBConnection + Send + Sync>,
    project_id: i32,
    email: String,
) -> User {
    db.create_user(NewUser::new(Some(email), None, project_id))
        .expect("test user should insert")
}

fn make_recovery_wrapping(user: &User, secret: [u8; 32], seed: &[u8]) -> NewUserSeedWrapping {
    let code = recovery_code_fixture(secret);
    new_recovery_seed_wrapping(&TEST_ROOT_KEY, user, &code, seed)
        .expect("recovery wrapping should compute")
}

fn test_database_url() -> Option<String> {
    std::env::var("RECOVERY_TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").ok())
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_enrollment_creates_one_wrap_over_existing_seed() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-enroll-{marker}@example.com");
    let seed = b"test seed for recovery enrollment";

    let user = create_test_user(&db, project.id, email);
    assert!(
        !db.recovery_wrap_exists(user.uuid).unwrap(),
        "user without enrollment must have no recovery wrap"
    );

    let wrapping = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let inserted = db
        .insert_recovery_wrap_if_absent(wrapping)
        .expect("enrollment should create recovery wrap");

    let fetched = db
        .get_recovery_wrap(user.uuid)
        .expect("get_recovery_wrap should not error")
        .expect("recovery wrap should exist after enrollment");
    assert_eq!(fetched.id, inserted.id);
    assert_eq!(fetched.credential_kind, CredentialKind::Recovery.as_str());
    assert_eq!(fetched.user_id, user.uuid);

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_concurrent_enrollment_has_exactly_one_winner() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-concurrent-{marker}@example.com");
    let seed = b"test seed for concurrent enrollment";

    let user = create_test_user(&db, project.id, email);
    let wrapping_a = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let _ = db
        .insert_recovery_wrap_if_absent(wrapping_a)
        .expect("first enrollment should succeed");

    let wrapping_b = make_recovery_wrapping(&user, [0xCDu8; 32], seed);
    let result = db.insert_recovery_wrap_if_absent(wrapping_b);
    assert!(
        matches!(result, Err(DBError::StaleCredentialState)),
        "second enrollment should fail with StaleCredentialState"
    );

    let wraps = db
        .get_user_seed_wrappings_for_user_and_kind(user.uuid, CredentialKind::Recovery.as_str())
        .unwrap();
    assert_eq!(
        wraps.len(),
        1,
        "exactly one recovery wrap must exist after concurrent enrollment race"
    );

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_rotation_replaces_wrap_with_cas() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-rotate-{marker}@example.com");
    let seed = b"test seed for rotation";

    let user = create_test_user(&db, project.id, email);
    let old_wrapping = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let old = db
        .insert_recovery_wrap_if_absent(old_wrapping)
        .expect("initial enrollment should succeed");

    let new_wrapping = make_recovery_wrapping(&user, [0xCDu8; 32], seed);
    let new = db
        .replace_recovery_wrap_if_unchanged(&old, new_wrapping)
        .expect("rotation with matching old wrap should succeed");

    assert_ne!(
        old.id, new.id,
        "rotation should create a new row with a different id"
    );

    let fetched = db
        .get_recovery_wrap(user.uuid)
        .unwrap()
        .expect("recovery wrap should still exist after rotation");
    assert_eq!(fetched.id, new.id);

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_rotation_rejects_stale_old_wrap() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-rotate-stale-{marker}@example.com");
    let seed = b"test seed for stale rotation";

    let user = create_test_user(&db, project.id, email);
    let old_wrapping = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let old = db
        .insert_recovery_wrap_if_absent(old_wrapping)
        .expect("initial enrollment should succeed");

    // Rotate once: old is now invalidated
    let intermediate_wrapping = make_recovery_wrapping(&user, [0xCDu8; 32], seed);
    let intermediate = db
        .replace_recovery_wrap_if_unchanged(&old, intermediate_wrapping)
        .expect("first rotation should succeed");

    // Try to rotate again using the now-stale old wrap
    let newest_wrapping = make_recovery_wrapping(&user, [0xEFu8; 32], seed);
    let result = db.replace_recovery_wrap_if_unchanged(&old, newest_wrapping);
    assert!(
        matches!(result, Err(DBError::StaleCredentialState)),
        "rotation with stale old wrap should fail with StaleCredentialState"
    );

    // Verify the intermediate wrap is still in place
    let fetched = db
        .get_recovery_wrap(user.uuid)
        .unwrap()
        .expect("intermediate recovery wrap should still exist");
    assert_eq!(fetched.id, intermediate.id);

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_disablement_is_idempotent() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-disable-{marker}@example.com");
    let seed = b"test seed for disablement";

    let user = create_test_user(&db, project.id, email);
    let wrapping = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let _ = db
        .insert_recovery_wrap_if_absent(wrapping)
        .expect("enrollment should succeed");

    let deleted_count_1 = db
        .delete_recovery_wrap_for_user(user.uuid)
        .expect("disablement should succeed");
    assert_eq!(
        deleted_count_1, 1,
        "first disablement should delete exactly one row"
    );

    let deleted_count_2 = db
        .delete_recovery_wrap_for_user(user.uuid)
        .expect("second disablement should also succeed");
    assert_eq!(
        deleted_count_2, 0,
        "second disablement should be idempotent"
    );

    assert!(
        !db.recovery_wrap_exists(user.uuid).unwrap(),
        "recovery wrap should not exist after disablement"
    );

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_disablement_races_safely_with_rotation() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-race-{marker}@example.com");
    let seed = b"test seed for disablement race";

    let user = create_test_user(&db, project.id, email);
    let old_wrapping = make_recovery_wrapping(&user, [0xABu8; 32], seed);
    let old = db
        .insert_recovery_wrap_if_absent(old_wrapping)
        .expect("enrollment should succeed");

    // Delete first
    let deleted = db
        .delete_recovery_wrap_for_user(user.uuid)
        .expect("disablement should succeed");
    assert_eq!(deleted, 1);

    // Rotation against the now-deleted wrap should fail
    let new_wrapping = make_recovery_wrapping(&user, [0xCDu8; 32], seed);
    let result = db.replace_recovery_wrap_if_unchanged(&old, new_wrapping);
    assert!(
        matches!(result, Err(DBError::StaleCredentialState)),
        "rotation after disablement should fail with StaleCredentialState"
    );

    // Cleanup
    let _ = db.delete_user(&user);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_wrap_is_scoped_to_user() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email_a = format!("recovery-scope-a-{marker}@example.com");
    let email_b = format!("recovery-scope-b-{marker}@example.com");
    let seed = b"test seed for scope isolation";

    let user_a = create_test_user(&db, project.id, email_a);
    let user_b = create_test_user(&db, project.id, email_b);

    let wrapping_a = make_recovery_wrapping(&user_a, [0xABu8; 32], seed);
    let _ = db
        .insert_recovery_wrap_if_absent(wrapping_a)
        .expect("enrollment for user_a should succeed");

    // user_b should not see user_a's wrap
    assert!(
        !db.recovery_wrap_exists(user_b.uuid).unwrap(),
        "user_b should not have a recovery wrap"
    );

    let wrapping_b = make_recovery_wrapping(&user_b, [0xCDu8; 32], seed);
    let _ = db
        .insert_recovery_wrap_if_absent(wrapping_b)
        .expect("enrollment for user_b should also succeed");

    assert!(
        db.recovery_wrap_exists(user_a.uuid).unwrap(),
        "user_a should still have recovery wrap after user_b enrolls"
    );

    // Cleanup
    let _ = db.delete_user(&user_a);
    let _ = db.delete_user(&user_b);
}

#[tokio::test]
#[ignore = "requires RECOVERY_TEST_DATABASE_URL (or AEAD_TAMPER_TEST_DATABASE_URL) pointing at disposable migrated local Postgres"]
async fn recovery_get_wrap_returns_none_when_absent() {
    let Some(database_url) = test_database_url() else {
        eprintln!("skipping: no test database URL set");
        return;
    };

    let db = build_db(database_url);
    let project = first_active_project(&db);
    let marker = Uuid::new_v4();
    let email = format!("recovery-get-none-{marker}@example.com");

    let user = create_test_user(&db, project.id, email);
    let wrap = db.get_recovery_wrap(user.uuid).unwrap();
    assert!(
        wrap.is_none(),
        "get_recovery_wrap should return None for user without recovery"
    );

    // Cleanup
    let _ = db.delete_user(&user);
}
