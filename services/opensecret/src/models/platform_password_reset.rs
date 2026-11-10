use crate::models::schema::platform_password_reset_requests;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum PlatformPasswordResetError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("Invalid expiration hours: {0}")]
    InvalidExpirationHours(String),
}

#[derive(Queryable, Identifiable, AsChangeset, Serialize, Deserialize, Clone, Debug)]
#[diesel(table_name = platform_password_reset_requests)]
pub struct PlatformPasswordResetRequest {
    pub id: i32,
    pub platform_user_id: Uuid,
    pub hashed_secret: String,
    pub encrypted_code: Vec<u8>,
    pub expiration_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_reset: bool,
}

impl PlatformPasswordResetRequest {
    pub fn get_by_user_id_and_code(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_encrypted_code: &[u8],
    ) -> Result<Option<PlatformPasswordResetRequest>, PlatformPasswordResetError> {
        platform_password_reset_requests::table
            .filter(platform_password_reset_requests::platform_user_id.eq(lookup_user_id))
            .filter(platform_password_reset_requests::encrypted_code.eq(lookup_encrypted_code))
            .filter(platform_password_reset_requests::is_reset.eq(false))
            .filter(platform_password_reset_requests::expiration_time.gt(Utc::now()))
            .first::<PlatformPasswordResetRequest>(conn)
            .optional()
            .map_err(PlatformPasswordResetError::DatabaseError)
    }

    /// Whether the account has a live request created with this reset
    /// secret. A confirmation can only ever complete such a request, whatever
    /// code it submits.
    pub fn active_exists_for_user_id_and_secret(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
    ) -> Result<bool, PlatformPasswordResetError> {
        diesel::select(diesel::dsl::exists(
            platform_password_reset_requests::table
                .filter(platform_password_reset_requests::platform_user_id.eq(lookup_user_id))
                .filter(platform_password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
                .filter(platform_password_reset_requests::is_reset.eq(false))
                .filter(platform_password_reset_requests::expiration_time.gt(Utc::now())),
        ))
        .get_result(conn)
        .map_err(PlatformPasswordResetError::DatabaseError)
    }

    pub fn mark_as_reset(&self, conn: &mut PgConnection) -> Result<(), PlatformPasswordResetError> {
        diesel::update(platform_password_reset_requests::table)
            .filter(platform_password_reset_requests::id.eq(self.id))
            .set(platform_password_reset_requests::is_reset.eq(true))
            .execute(conn)
            .map(|_| ())
            .map_err(PlatformPasswordResetError::DatabaseError)
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expiration_time
    }
}

#[derive(Insertable)]
#[diesel(table_name = platform_password_reset_requests)]
pub struct NewPlatformPasswordResetRequest {
    pub platform_user_id: Uuid,
    pub hashed_secret: String,
    pub encrypted_code: Vec<u8>,
    pub expiration_time: DateTime<Utc>,
}

impl NewPlatformPasswordResetRequest {
    pub fn new(
        platform_user_id: Uuid,
        hashed_secret: String,
        encrypted_code: Vec<u8>,
        expiration_hours: i64,
    ) -> Result<Self, PlatformPasswordResetError> {
        if expiration_hours <= 0 {
            return Err(PlatformPasswordResetError::InvalidExpirationHours(
                "expiration_hours must be positive".to_string(),
            ));
        }

        Ok(NewPlatformPasswordResetRequest {
            platform_user_id,
            hashed_secret,
            encrypted_code,
            expiration_time: Utc::now() + chrono::Duration::hours(expiration_hours),
        })
    }

    pub fn insert(
        &self,
        conn: &mut PgConnection,
    ) -> Result<PlatformPasswordResetRequest, PlatformPasswordResetError> {
        diesel::insert_into(platform_password_reset_requests::table)
            .values(self)
            .get_result::<PlatformPasswordResetRequest>(conn)
            .map_err(PlatformPasswordResetError::DatabaseError)
    }
}
