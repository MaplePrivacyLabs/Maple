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
    #[error("No attempt budget left for this reset request")]
    AttemptBudgetExhausted,
}

/// Durable per-request guess budget shared by every process that reaches the
/// database: at most this many incorrect email-code guesses per request.
pub const PASSWORD_RESET_MAX_FAILED_ATTEMPTS: i32 = 5;

/// Durable per-account email cap: at most this many reset requests (and so
/// reset emails) created inside the rolling hour below.
pub const PASSWORD_RESET_REQUESTS_PER_HOUR: i64 = 5;

/// Length of the rolling window the per-account email cap counts over.
pub const PASSWORD_RESET_EMAIL_WINDOW_SECONDS: i64 = 60 * 60;

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
    pub failed_attempts: i32,
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

    /// Whether the account has an unconsumed request created with this reset
    /// secret. A confirmation can only ever complete such a request, whatever
    /// code it submits. Like the code lookup, this leaves expiry to the
    /// caller, which reports an expired request as such.
    pub fn unconsumed_exists_for_user_id_and_secret(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
    ) -> Result<bool, PasswordResetError> {
        diesel::select(diesel::dsl::exists(
            password_reset_requests::table
                .filter(password_reset_requests::user_id.eq(lookup_user_id))
                .filter(password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
                .filter(password_reset_requests::is_reset.eq(false)),
        ))
        .get_result(conn)
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

    /// Charges one failed guess to the durable budget of the account's
    /// unconsumed requests created with `hashed_secret`.
    ///
    /// Used when the submitted code matched no request: the secret has
    /// already been proven to belong to at least one unconsumed request (the
    /// admission check), so the wrong code is charged to that request's
    /// owner. The guarded `UPDATE` matches only rows carrying that secret, so
    /// nobody without the secret can spend this budget, and an unrelated
    /// secret cannot burn anyone else's.
    pub fn charge_failed_attempt_for_secret(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
    ) -> Result<(), PasswordResetError> {
        let charged = diesel::update(
            password_reset_requests::table
                .filter(password_reset_requests::user_id.eq(lookup_user_id))
                .filter(password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
                .filter(password_reset_requests::is_reset.eq(false))
                .filter(
                    password_reset_requests::failed_attempts.lt(PASSWORD_RESET_MAX_FAILED_ATTEMPTS),
                ),
        )
        .set(
            password_reset_requests::failed_attempts
                .eq(password_reset_requests::failed_attempts + 1),
        )
        .execute(conn)?;
        if charged == 0 {
            return Err(PasswordResetError::AttemptBudgetExhausted);
        }
        Ok(())
    }

    /// The newest unconsumed request for the account created with
    /// `hashed_secret`, if any: the request a completion through this
    /// secret would select. Expiry is left to the caller.
    pub fn newest_unconsumed_for_user_id_and_secret(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
    ) -> Result<Option<PasswordResetRequest>, PasswordResetError> {
        password_reset_requests::table
            .filter(password_reset_requests::user_id.eq(lookup_user_id))
            .filter(password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
            .filter(password_reset_requests::is_reset.eq(false))
            .order(password_reset_requests::id.desc())
            .first::<PasswordResetRequest>(conn)
            .optional()
            .map_err(PasswordResetError::DatabaseError)
    }

    /// Whether this request still has unused guess budget. A read-only view
    /// of the same durable counter `charge_failed_attempt` spends.
    pub fn has_attempt_budget(&self) -> bool {
        self.failed_attempts < PASSWORD_RESET_MAX_FAILED_ATTEMPTS
    }

    /// Count of this account's reset requests created inside the rolling
    /// email window. Every request row means one sent email, so this count is
    /// the durable per-account email budget already spent.
    pub fn count_recent_requests_for_user(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<i64, PasswordResetError> {
        let window_start =
            Utc::now() - chrono::Duration::seconds(PASSWORD_RESET_EMAIL_WINDOW_SECONDS);
        password_reset_requests::table
            .filter(password_reset_requests::user_id.eq(lookup_user_id))
            .filter(password_reset_requests::created_at.gt(window_start))
            .count()
            .get_result(conn)
            .map_err(PasswordResetError::DatabaseError)
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

    /// Inserts the request only while the account still has email budget in
    /// the rolling hour. The user-row lock serializes competing creations
    /// for the same account before the count, so concurrent processes
    /// cannot exceed the cap together. Returns the inserted request, or
    /// `None` when the account is capped and no row (and so no email) may
    /// be created.
    pub fn insert_within_email_budget(
        &self,
        conn: &mut PgConnection,
    ) -> Result<Option<PasswordResetRequest>, PasswordResetError> {
        use crate::models::schema::users;
        conn.transaction::<_, PasswordResetError, _>(|conn| {
            // Serialize creations for this account the same way the
            // credential-mutation flows do: through the user row.
            let locked = users::table
                .filter(users::uuid.eq(self.user_id))
                .select(users::uuid)
                .for_update()
                .first::<Uuid>(conn)
                .optional()?;
            if locked.is_none() {
                // The account vanished between the eligibility check and
                // this insert; nothing may be created for it.
                return Ok(None);
            }
            let recent = Self::recent_request_count(conn, self.user_id)?;
            if recent >= PASSWORD_RESET_REQUESTS_PER_HOUR {
                return Ok(None);
            }
            let inserted = diesel::insert_into(password_reset_requests::table)
                .values(self)
                .get_result::<PasswordResetRequest>(conn)?;
            Ok(Some(inserted))
        })
    }

    fn recent_request_count(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<i64, PasswordResetError> {
        let window_start =
            Utc::now() - chrono::Duration::seconds(PASSWORD_RESET_EMAIL_WINDOW_SECONDS);
        password_reset_requests::table
            .filter(password_reset_requests::user_id.eq(lookup_user_id))
            .filter(password_reset_requests::created_at.gt(window_start))
            .count()
            .get_result(conn)
            .map_err(PasswordResetError::DatabaseError)
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
