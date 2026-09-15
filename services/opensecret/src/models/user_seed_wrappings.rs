use crate::models::schema::user_seed_wrappings;
use crate::seed_wrapping::{
    CredentialKind, MAX_RECOVERY_ENVELOPE_BYTES, MIN_RECOVERY_ENVELOPE_BYTES,
};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel::upsert::excluded;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum UserSeedWrappingError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("Invalid stored recovery wrapping")]
    InvalidRecoveryWrapping,
}

diesel::define_sql_function! {
    fn octet_length(value: diesel::sql_types::Binary) -> diesel::sql_types::Integer;
}

/// SQL returns NULL for invalid-sized fields instead of transferring their
/// contents. This projection must be validated before it becomes a seed wrap.
#[derive(Queryable)]
struct RecoveryWrappingRow {
    id: i64,
    user_id: Uuid,
    credential_kind: String,
    credential_lookup_hash: Option<Vec<u8>>,
    wrapping_version: i16,
    seed_enc: Option<Vec<u8>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Queryable, Identifiable, Clone, Debug)]
#[diesel(table_name = user_seed_wrappings)]
pub struct UserSeedWrapping {
    pub id: i64,
    pub user_id: Uuid,
    pub credential_kind: String,
    pub credential_lookup_hash: Vec<u8>,
    pub wrapping_version: i16,
    pub seed_enc: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl UserSeedWrapping {
    /// Recovery has a single active slot. Bound both binary fields in the same
    /// query snapshot, and reject invalid sizes or duplicate slots as storage
    /// integrity failures, not as an absent wrap or a wrong recovery code.
    pub fn get_recovery_for_user(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<Option<Self>, UserSeedWrappingError> {
        use diesel::dsl::case_when;
        use user_seed_wrappings as w;

        let mut rows = w::table
            .filter(w::user_id.eq(lookup_user_id))
            .filter(w::credential_kind.eq(CredentialKind::Recovery.as_str()))
            .select((
                w::id,
                w::user_id,
                w::credential_kind,
                case_when(
                    octet_length(w::credential_lookup_hash).eq(32),
                    w::credential_lookup_hash,
                ),
                w::wrapping_version,
                case_when(
                    octet_length(w::seed_enc).between(
                        MIN_RECOVERY_ENVELOPE_BYTES as i32,
                        MAX_RECOVERY_ENVELOPE_BYTES as i32,
                    ),
                    w::seed_enc,
                ),
                w::created_at,
                w::updated_at,
            ))
            .limit(2)
            .load::<RecoveryWrappingRow>(conn)?;
        if rows.len() > 1 {
            return Err(UserSeedWrappingError::InvalidRecoveryWrapping);
        }
        let Some(row) = rows.pop() else {
            return Ok(None);
        };
        let credential_lookup_hash = row
            .credential_lookup_hash
            .filter(|hash| hash.len() == 32)
            .ok_or(UserSeedWrappingError::InvalidRecoveryWrapping)?;
        let seed_enc = row
            .seed_enc
            .filter(|seed| {
                (MIN_RECOVERY_ENVELOPE_BYTES..=MAX_RECOVERY_ENVELOPE_BYTES).contains(&seed.len())
            })
            .ok_or(UserSeedWrappingError::InvalidRecoveryWrapping)?;
        Ok(Some(Self {
            id: row.id,
            user_id: row.user_id,
            credential_kind: row.credential_kind,
            credential_lookup_hash,
            wrapping_version: row.wrapping_version,
            seed_enc,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }))
    }

    pub fn get_for_user_and_kind(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_credential_kind: &str,
    ) -> Result<Vec<Self>, UserSeedWrappingError> {
        user_seed_wrappings::table
            .filter(user_seed_wrappings::user_id.eq(lookup_user_id))
            .filter(user_seed_wrappings::credential_kind.eq(lookup_credential_kind))
            .order(user_seed_wrappings::id.asc())
            .load::<Self>(conn)
            .map_err(UserSeedWrappingError::DatabaseError)
    }

    pub fn get_by_credential(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_credential_kind: &str,
        lookup_credential_hash: &[u8],
        lookup_wrapping_version: i16,
    ) -> Result<Option<Self>, UserSeedWrappingError> {
        user_seed_wrappings::table
            .filter(user_seed_wrappings::user_id.eq(lookup_user_id))
            .filter(user_seed_wrappings::credential_kind.eq(lookup_credential_kind))
            .filter(user_seed_wrappings::credential_lookup_hash.eq(lookup_credential_hash))
            .filter(user_seed_wrappings::wrapping_version.eq(lookup_wrapping_version))
            .first::<Self>(conn)
            .optional()
            .map_err(UserSeedWrappingError::DatabaseError)
    }

    pub fn delete_for_user(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<usize, UserSeedWrappingError> {
        diesel::delete(
            user_seed_wrappings::table.filter(user_seed_wrappings::user_id.eq(lookup_user_id)),
        )
        .execute(conn)
        .map_err(UserSeedWrappingError::DatabaseError)
    }

    pub fn delete_for_user_and_kind(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_credential_kind: &str,
    ) -> Result<usize, UserSeedWrappingError> {
        diesel::delete(
            user_seed_wrappings::table
                .filter(user_seed_wrappings::user_id.eq(lookup_user_id))
                .filter(user_seed_wrappings::credential_kind.eq(lookup_credential_kind)),
        )
        .execute(conn)
        .map_err(UserSeedWrappingError::DatabaseError)
    }
}

#[derive(Insertable, Clone, Debug)]
#[diesel(table_name = user_seed_wrappings)]
pub struct NewUserSeedWrapping {
    pub user_id: Uuid,
    pub credential_kind: String,
    pub credential_lookup_hash: Vec<u8>,
    pub wrapping_version: i16,
    pub seed_enc: Vec<u8>,
}

impl NewUserSeedWrapping {
    pub fn new(
        user_id: Uuid,
        credential_kind: impl Into<String>,
        credential_lookup_hash: Vec<u8>,
        wrapping_version: i16,
        seed_enc: Vec<u8>,
    ) -> Self {
        Self {
            user_id,
            credential_kind: credential_kind.into(),
            credential_lookup_hash,
            wrapping_version,
            seed_enc,
        }
    }

    pub fn insert(
        &self,
        conn: &mut PgConnection,
    ) -> Result<UserSeedWrapping, UserSeedWrappingError> {
        diesel::insert_into(user_seed_wrappings::table)
            .values(self)
            .get_result::<UserSeedWrapping>(conn)
            .map_err(UserSeedWrappingError::DatabaseError)
    }

    pub fn upsert_by_credential(
        &self,
        conn: &mut PgConnection,
    ) -> Result<UserSeedWrapping, UserSeedWrappingError> {
        diesel::insert_into(user_seed_wrappings::table)
            .values(self)
            .on_conflict((
                user_seed_wrappings::user_id,
                user_seed_wrappings::credential_kind,
                user_seed_wrappings::credential_lookup_hash,
                user_seed_wrappings::wrapping_version,
            ))
            .do_update()
            .set((
                user_seed_wrappings::seed_enc.eq(excluded(user_seed_wrappings::seed_enc)),
                user_seed_wrappings::updated_at.eq(diesel::dsl::now),
            ))
            .get_result::<UserSeedWrapping>(conn)
            .map_err(UserSeedWrappingError::DatabaseError)
    }
}
