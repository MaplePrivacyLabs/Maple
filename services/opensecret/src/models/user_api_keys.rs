use crate::models::schema::user_api_keys;
use chrono::{DateTime, Duration, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum UserApiKeyError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("API key with this name already exists")]
    DuplicateName,
    #[error("API key not found")]
    NotFound,
}

#[derive(Queryable, Serialize, Deserialize, Clone)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(table_name = user_api_keys)]
pub struct UserApiKey {
    pub id: i32,
    pub user_id: Uuid,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for UserApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserApiKey")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("key_hash", &"<redacted>")
            .field("name", &self.name)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("last_used_at", &self.last_used_at)
            .finish()
    }
}

#[derive(Insertable)]
#[diesel(table_name = user_api_keys)]
pub struct NewUserApiKey {
    pub user_id: Uuid,
    pub key_hash: String,
    pub name: String,
}

impl std::fmt::Debug for NewUserApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUserApiKey")
            .field("user_id", &self.user_id)
            .field("key_hash", &"<redacted>")
            .field("name", &self.name)
            .finish()
    }
}

impl NewUserApiKey {
    pub fn new(user_id: Uuid, key_hash: String, name: String) -> Self {
        Self {
            user_id,
            key_hash,
            name,
        }
    }

    pub fn insert(self, conn: &mut PgConnection) -> Result<UserApiKey, UserApiKeyError> {
        diesel::insert_into(user_api_keys::table)
            .values(&self)
            .get_result(conn)
            .map_err(|e| match e {
                diesel::result::Error::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    ref info,
                ) if info.constraint_name() == Some("user_api_keys_user_id_name_key") => {
                    UserApiKeyError::DuplicateName
                }
                _ => UserApiKeyError::DatabaseError(e),
            })
    }
}

impl UserApiKey {
    pub fn get_by_id(conn: &mut PgConnection, id: i32) -> Result<Option<Self>, UserApiKeyError> {
        user_api_keys::table
            .filter(user_api_keys::id.eq(id))
            .first::<Self>(conn)
            .optional()
            .map_err(UserApiKeyError::DatabaseError)
    }

    pub fn get_by_key_hash(
        conn: &mut PgConnection,
        key_hash: &str,
    ) -> Result<Option<Self>, UserApiKeyError> {
        user_api_keys::table
            .filter(user_api_keys::key_hash.eq(key_hash))
            .first::<Self>(conn)
            .optional()
            .map_err(UserApiKeyError::DatabaseError)
    }

    pub fn get_all_for_user(
        conn: &mut PgConnection,
        user_id: Uuid,
    ) -> Result<Vec<Self>, UserApiKeyError> {
        user_api_keys::table
            .filter(user_api_keys::user_id.eq(user_id))
            .load::<Self>(conn)
            .map_err(UserApiKeyError::DatabaseError)
    }

    pub fn delete(self, conn: &mut PgConnection) -> Result<(), UserApiKeyError> {
        diesel::delete(user_api_keys::table.filter(user_api_keys::id.eq(self.id)))
            .execute(conn)
            .map(|_| ())
            .map_err(UserApiKeyError::DatabaseError)
    }

    pub fn delete_by_id(conn: &mut PgConnection, id: i32) -> Result<(), UserApiKeyError> {
        diesel::delete(user_api_keys::table.filter(user_api_keys::id.eq(id)))
            .execute(conn)
            .map(|_| ())
            .map_err(UserApiKeyError::DatabaseError)
    }

    pub fn delete_by_name_and_user(
        conn: &mut PgConnection,
        name: &str,
        user_id: Uuid,
    ) -> Result<(), UserApiKeyError> {
        let rows_affected = diesel::delete(
            user_api_keys::table
                .filter(user_api_keys::name.eq(name))
                .filter(user_api_keys::user_id.eq(user_id)),
        )
        .execute(conn)
        .map_err(UserApiKeyError::DatabaseError)?;

        if rows_affected == 0 {
            Err(UserApiKeyError::NotFound)
        } else {
            Ok(())
        }
    }

    /// Records that this key just authenticated a request.
    ///
    /// The write is throttled: it only lands when `last_used_at` is NULL or
    /// older than one minute, so high-frequency authenticated requests do not
    /// amplify database writes. Returns whether a row was updated.
    pub fn touch_last_used(
        conn: &mut PgConnection,
        key_hash: &str,
    ) -> Result<usize, UserApiKeyError> {
        let stale_before = Utc::now() - Duration::minutes(1);
        diesel::update(user_api_keys::table.filter(user_api_keys::key_hash.eq(key_hash)))
            .set(user_api_keys::last_used_at.eq(Utc::now()))
            .filter(
                user_api_keys::last_used_at
                    .is_null()
                    .or(user_api_keys::last_used_at.lt(stale_before)),
            )
            .execute(conn)
            .map_err(UserApiKeyError::DatabaseError)
    }
}
