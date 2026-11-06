use crate::models::schema::password_reset_requests;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum PasswordResetError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
}

#[derive(Queryable, Identifiable, AsChangeset, Serialize, Deserialize, Clone, Debug)]
#[diesel(table_name = password_reset_requests)]
pub struct PasswordResetRequest {
    pub id: i32,
    pub user_id: Uuid,
    pub hashed_secret: String,
    pub encrypted_code: Vec<u8>,
    pub expiration_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub is_reset: bool,
}

impl PasswordResetRequest {
    pub fn get_by_user_id_and_code(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_encrypted_code: &[u8],
    ) -> Result<Option<PasswordResetRequest>, PasswordResetError> {
        password_reset_requests::table
            .filter(password_reset_requests::user_id.eq(lookup_user_id))
            .filter(password_reset_requests::encrypted_code.eq(lookup_encrypted_code))
            .filter(password_reset_requests::is_reset.eq(false))
            // Codes can repeat across a user's open requests; pick the newest
            // so every lookup with the same code selects the same row.
            .order(password_reset_requests::id.desc())
            .first::<PasswordResetRequest>(conn)
            .optional()
            .map_err(PasswordResetError::DatabaseError)
    }

    pub fn mark_as_reset(&self, conn: &mut PgConnection) -> Result<(), PasswordResetError> {
        diesel::update(password_reset_requests::table)
            .filter(password_reset_requests::id.eq(self.id))
            .set(password_reset_requests::is_reset.eq(true))
            .execute(conn)
            .map(|_| ())
            .map_err(PasswordResetError::DatabaseError)
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expiration_time
    }
}

#[derive(Insertable)]
#[diesel(table_name = password_reset_requests)]
pub struct NewPasswordResetRequest {
    pub user_id: Uuid,
    pub hashed_secret: String,
    pub encrypted_code: Vec<u8>,
    pub expiration_time: DateTime<Utc>,
}

impl NewPasswordResetRequest {
    pub fn new(
        user_id: Uuid,
        hashed_secret: String,
        encrypted_code: Vec<u8>,
        expiration_hours: i64,
    ) -> Self {
        NewPasswordResetRequest {
            user_id,
            hashed_secret,
            encrypted_code,
            expiration_time: Utc::now() + chrono::Duration::hours(expiration_hours),
        }
    }

    pub fn insert(
        &self,
        conn: &mut PgConnection,
    ) -> Result<PasswordResetRequest, PasswordResetError> {
        diesel::insert_into(password_reset_requests::table)
            .values(self)
            .get_result::<PasswordResetRequest>(conn)
            .map_err(PasswordResetError::DatabaseError)
    }
}
