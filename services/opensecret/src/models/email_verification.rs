use crate::models::schema::email_verifications;
use chrono::{DateTime, Duration, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EmailVerificationError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
}

#[derive(Queryable, Identifiable, AsChangeset, Serialize, Deserialize, Clone)]
#[diesel(table_name = email_verifications)]
pub struct EmailVerification {
    pub id: i32,
    pub user_id: Uuid,
    pub verification_code: Uuid,
    pub is_verified: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for EmailVerification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailVerification")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("verification_code", &"[redacted]")
            .field("is_verified", &self.is_verified)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl EmailVerification {
    pub fn get_by_id(
        conn: &mut PgConnection,
        lookup_id: i32,
    ) -> Result<Option<EmailVerification>, EmailVerificationError> {
        email_verifications::table
            .filter(email_verifications::id.eq(lookup_id))
            .first::<EmailVerification>(conn)
            .optional()
            .map_err(EmailVerificationError::DatabaseError)
    }

    pub fn get_by_user_id(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<Option<EmailVerification>, EmailVerificationError> {
        email_verifications::table
            .filter(email_verifications::user_id.eq(lookup_user_id))
            .first::<EmailVerification>(conn)
            .optional()
            .map_err(EmailVerificationError::DatabaseError)
    }

    pub fn get_by_verification_code(
        conn: &mut PgConnection,
        lookup_code: Uuid,
    ) -> Result<Option<EmailVerification>, EmailVerificationError> {
        email_verifications::table
            .filter(email_verifications::verification_code.eq(lookup_code))
            .first::<EmailVerification>(conn)
            .optional()
            .map_err(EmailVerificationError::DatabaseError)
    }

    pub fn update(&self, conn: &mut PgConnection) -> Result<(), EmailVerificationError> {
        diesel::update(email_verifications::table)
            .filter(email_verifications::id.eq(self.id))
            .set(self)
            .execute(conn)
            .map(|_| ())
            .map_err(EmailVerificationError::DatabaseError)
    }

    pub fn delete(&self, conn: &mut PgConnection) -> Result<(), EmailVerificationError> {
        diesel::delete(email_verifications::table)
            .filter(email_verifications::id.eq(self.id))
            .execute(conn)
            .map(|_| ())
            .map_err(EmailVerificationError::DatabaseError)
    }

    pub fn verify(&mut self, conn: &mut PgConnection) -> Result<(), EmailVerificationError> {
        self.is_verified = true;
        self.update(conn)
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

#[derive(Insertable)]
#[diesel(table_name = email_verifications)]
pub struct NewEmailVerification {
    pub user_id: Uuid,
    pub verification_code: Uuid,
    pub expires_at: DateTime<Utc>,
    pub is_verified: bool,
}

impl NewEmailVerification {
    pub fn new(user_id: Uuid, expiration_hours: i64, is_verified: bool) -> Self {
        NewEmailVerification {
            user_id,
            verification_code: Uuid::new_v4(),
            expires_at: Utc::now() + Duration::hours(expiration_hours),
            is_verified,
        }
    }

    pub fn insert(
        &self,
        conn: &mut PgConnection,
    ) -> Result<EmailVerification, EmailVerificationError> {
        diesel::insert_into(email_verifications::table)
            .values(self)
            .get_result::<EmailVerification>(conn)
            .map_err(EmailVerificationError::DatabaseError)
    }
}
