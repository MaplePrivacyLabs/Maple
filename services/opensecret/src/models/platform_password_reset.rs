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
    #[error("No attempt budget left for this reset request")]
    AttemptBudgetExhausted,
}

/// Durable per-request guess budget for platform (developer) accounts,
/// matching the user-account budget in `models/password_reset.rs`.
pub const PLATFORM_PASSWORD_RESET_MAX_FAILED_ATTEMPTS: i32 = 5;

/// Durable per-account email cap for platform (developer) accounts.
pub const PLATFORM_PASSWORD_RESET_REQUESTS_PER_HOUR: i64 = 5;

/// Length of the rolling window the platform email cap counts over.
pub const PLATFORM_PASSWORD_RESET_EMAIL_WINDOW_SECONDS: i64 = 60 * 60;

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
    pub failed_attempts: i32,
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

    /// Whether this request still has unused guess budget.
    pub fn has_attempt_budget(&self) -> bool {
        self.failed_attempts < PLATFORM_PASSWORD_RESET_MAX_FAILED_ATTEMPTS
    }

    /// Charges one failed guess to the durable budget of the account's oldest
    /// active request created with `hashed_secret`.
    ///
    /// Used when the submitted code matched no request: the secret has
    /// already been proven to belong to at least one active request (the
    /// admission check), so the wrong code is charged to that request's
    /// owner. The guarded `UPDATE` matches only rows carrying that secret,
    /// so nobody without the secret can spend this budget.
    pub fn charge_failed_attempt_for_secret(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
    ) -> Result<(), PlatformPasswordResetError> {
        let charged = diesel::update(
            platform_password_reset_requests::table
                .filter(platform_password_reset_requests::platform_user_id.eq(lookup_user_id))
                .filter(platform_password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
                .filter(platform_password_reset_requests::is_reset.eq(false))
                .filter(platform_password_reset_requests::expiration_time.gt(Utc::now()))
                .filter(
                    platform_password_reset_requests::failed_attempts
                        .lt(PLATFORM_PASSWORD_RESET_MAX_FAILED_ATTEMPTS),
                ),
        )
        .set(
            platform_password_reset_requests::failed_attempts
                .eq(platform_password_reset_requests::failed_attempts + 1),
        )
        .execute(conn)?;
        if charged == 0 {
            return Err(PlatformPasswordResetError::AttemptBudgetExhausted);
        }
        Ok(())
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

    /// Inserts the request only while the platform account still has email
    /// budget in the rolling hour, the platform mirror of
    /// [`NewPasswordResetRequest::insert_within_email_budget`]. The
    /// platform-user-row lock serializes competing creations for the same
    /// account before the count. Returns `None` when the account is capped:
    /// no row and no email may be created.
    pub fn insert_within_email_budget(
        &self,
        conn: &mut PgConnection,
    ) -> Result<Option<PlatformPasswordResetRequest>, PlatformPasswordResetError> {
        use crate::models::schema::platform_users;
        conn.transaction::<_, PlatformPasswordResetError, _>(|conn| {
            let locked = platform_users::table
                .filter(platform_users::uuid.eq(self.platform_user_id))
                .select(platform_users::uuid)
                .for_update()
                .first::<Uuid>(conn)
                .optional()?;
            if locked.is_none() {
                return Ok(None);
            }
            let window_start = Utc::now()
                - chrono::Duration::seconds(PLATFORM_PASSWORD_RESET_EMAIL_WINDOW_SECONDS);
            let recent: i64 = platform_password_reset_requests::table
                .filter(
                    platform_password_reset_requests::platform_user_id.eq(self.platform_user_id),
                )
                .filter(platform_password_reset_requests::created_at.gt(window_start))
                .count()
                .get_result(conn)?;
            if recent >= PLATFORM_PASSWORD_RESET_REQUESTS_PER_HOUR {
                return Ok(None);
            }
            let inserted = diesel::insert_into(platform_password_reset_requests::table)
                .values(self)
                .get_result::<PlatformPasswordResetRequest>(conn)?;
            Ok(Some(inserted))
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
