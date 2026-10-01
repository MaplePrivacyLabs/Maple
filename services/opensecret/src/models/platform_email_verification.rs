use crate::models::schema::platform_email_verifications;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum PlatformEmailVerificationError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("Verification expired")]
    Expired,
    #[error("Verification already used")]
    AlreadyVerified,
    #[error("Invalid expiration duration: {0}")]
    InvalidExpirationDuration(String),
}

#[derive(Queryable, Identifiable, AsChangeset, Serialize, Deserialize, Clone)]
#[diesel(table_name = platform_email_verifications)]
pub struct PlatformEmailVerification {
    pub id: i32,
    pub platform_user_id: Uuid,
    pub verification_code: Uuid,
    pub is_verified: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for PlatformEmailVerification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformEmailVerification")
            .field("id", &self.id)
            .field("platform_user_id", &self.platform_user_id)
            .field("verification_code", &"[redacted]")
            .field("is_verified", &self.is_verified)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl PlatformEmailVerification {
    pub fn get_by_id(
        conn: &mut PgConnection,
        lookup_id: i32,
    ) -> Result<Option<PlatformEmailVerification>, PlatformEmailVerificationError> {
        platform_email_verifications::table
            .filter(platform_email_verifications::id.eq(lookup_id))
            .first::<PlatformEmailVerification>(conn)
            .optional()
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }

    pub fn get_by_platform_user_id(
        conn: &mut PgConnection,
        lookup_platform_user_id: Uuid,
    ) -> Result<Option<PlatformEmailVerification>, PlatformEmailVerificationError> {
        platform_email_verifications::table
            .filter(platform_email_verifications::platform_user_id.eq(lookup_platform_user_id))
            .first::<PlatformEmailVerification>(conn)
            .optional()
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }

    pub fn get_by_verification_code(
        conn: &mut PgConnection,
        lookup_verification_code: Uuid,
    ) -> Result<Option<PlatformEmailVerification>, PlatformEmailVerificationError> {
        platform_email_verifications::table
            .filter(platform_email_verifications::verification_code.eq(lookup_verification_code))
            .first::<PlatformEmailVerification>(conn)
            .optional()
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }

    pub fn update(&self, conn: &mut PgConnection) -> Result<(), PlatformEmailVerificationError> {
        diesel::update(platform_email_verifications::table)
            .filter(platform_email_verifications::id.eq(self.id))
            .set((
                platform_email_verifications::is_verified.eq(self.is_verified),
                platform_email_verifications::updated_at.eq(diesel::dsl::now),
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }

    pub fn delete(&self, conn: &mut PgConnection) -> Result<(), PlatformEmailVerificationError> {
        diesel::delete(platform_email_verifications::table)
            .filter(platform_email_verifications::id.eq(self.id))
            .execute(conn)
            .map(|_| ())
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }

    pub fn verify(
        &mut self,
        conn: &mut PgConnection,
    ) -> Result<(), PlatformEmailVerificationError> {
        // Check if already verified
        if self.is_verified {
            return Err(PlatformEmailVerificationError::AlreadyVerified);
        }

        // Check if verification is expired
        if self.expires_at < Utc::now() {
            return Err(PlatformEmailVerificationError::Expired);
        }

        // Mark as verified
        self.is_verified = true;
        self.update(conn)
    }
}

#[derive(Insertable)]
#[diesel(table_name = platform_email_verifications)]
pub struct NewPlatformEmailVerification {
    pub platform_user_id: Uuid,
    pub verification_code: Uuid,
    pub expires_at: DateTime<Utc>,
    pub is_verified: bool,
}

impl NewPlatformEmailVerification {
    pub fn new(
        platform_user_id: Uuid,
        expire_hours: i64,
        is_verified: bool,
    ) -> Result<Self, PlatformEmailVerificationError> {
        if expire_hours <= 0 {
            return Err(PlatformEmailVerificationError::InvalidExpirationDuration(
                "expire_hours must be positive".to_string(),
            ));
        }

        // Prevent unreasonably large durations (1 year max)
        if expire_hours > 8760 {
            return Err(PlatformEmailVerificationError::InvalidExpirationDuration(
                "expire_hours must be less than or equal to 8760 (1 year)".to_string(),
            ));
        }

        let expires_at = Utc::now() + chrono::Duration::hours(expire_hours);
        Ok(NewPlatformEmailVerification {
            platform_user_id,
            verification_code: Uuid::new_v4(),
            expires_at,
            is_verified,
        })
    }

    pub fn insert(
        &self,
        conn: &mut PgConnection,
    ) -> Result<PlatformEmailVerification, PlatformEmailVerificationError> {
        diesel::insert_into(platform_email_verifications::table)
            .values(self)
            .get_result::<PlatformEmailVerification>(conn)
            .map_err(PlatformEmailVerificationError::DatabaseError)
    }
}
