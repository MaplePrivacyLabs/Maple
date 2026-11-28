use crate::models::schema::password_reset_requests;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum PasswordResetError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
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

    /// Decides one reset proof attempt for the code-selected request and
    /// charges a failed guess, all inside one transaction.
    ///
    /// Every unconsumed request created with `lookup_hashed_secret` is
    /// locked first. Only those requests can ever be completed by this
    /// secret, so every attempt that could succeed queues on the same row
    /// locks and sees the charges of the attempts before it: concurrent
    /// guesses cannot read a stale budget. See [`decide_proof_attempt`].
    pub fn attempt_proof(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
        lookup_hashed_secret: &str,
        lookup_encrypted_code: &[u8],
    ) -> Result<ResetProofAttempt<PasswordResetRequest>, PasswordResetError> {
        conn.transaction(|conn| {
            let secret_requests = password_reset_requests::table
                .filter(password_reset_requests::user_id.eq(lookup_user_id))
                .filter(password_reset_requests::hashed_secret.eq(lookup_hashed_secret))
                .filter(password_reset_requests::is_reset.eq(false))
                .order(password_reset_requests::id.asc())
                .for_update()
                .load::<PasswordResetRequest>(conn)?;
            let selected =
                Self::get_by_user_id_and_code(conn, lookup_user_id, lookup_encrypted_code)?;
            let (attempt, charge) =
                decide_proof_attempt(&secret_requests, selected, lookup_hashed_secret);
            if !charge.is_empty() {
                diesel::update(
                    password_reset_requests::table
                        .filter(password_reset_requests::id.eq_any(charge)),
                )
                .set(
                    password_reset_requests::failed_attempts
                        .eq(password_reset_requests::failed_attempts + 1),
                )
                .execute(conn)?;
            }
            Ok(attempt)
        })
    }

    /// Whether this request still has unused guess budget.
    pub fn has_attempt_budget(&self) -> bool {
        self.failed_attempts < PASSWORD_RESET_MAX_FAILED_ATTEMPTS
    }
}

impl ResetRequestRow for PasswordResetRequest {
    fn row_id(&self) -> i32 {
        self.id
    }
    fn row_hashed_secret(&self) -> &str {
        &self.hashed_secret
    }
    fn expired(&self) -> bool {
        self.is_expired()
    }
    fn has_budget(&self) -> bool {
        self.has_attempt_budget()
    }
}

/// Outcome of one reset proof attempt, shared by user and platform resets.
#[derive(Debug)]
pub enum ResetProofAttempt<R> {
    /// Correct code and secret for an active request with budget left.
    Valid(R),
    /// The code selects an expired request; nothing was charged.
    Expired,
    /// The code-selected request, or every active request carrying the
    /// submitted secret, has spent its budget.
    Exhausted,
    /// The code selects another secret's request; the guess was charged.
    WrongSecret,
    /// The code selects no request; the guess was charged.
    WrongCode,
}

/// The fields the shared proof decision reads from either reset table.
pub(crate) trait ResetRequestRow {
    fn row_id(&self) -> i32;
    fn row_hashed_secret(&self) -> &str;
    fn expired(&self) -> bool;
    fn has_budget(&self) -> bool;
}

/// Decides a proof attempt from the locked rows carrying the submitted
/// secret and the row the submitted code selects. Returns the outcome and
/// the ids of the rows a failed guess charges.
///
/// The code-selected row's budget is checked before its secret is compared,
/// so a spent request stays closed even to its correct proof. Any failed
/// guess (wrong code, or correct code with a wrong secret) charges every
/// active row carrying the submitted secret, so nobody without that secret
/// can spend its budget. Expired rows are never charged; a secret with no
/// active row left has nothing durable to charge.
pub(crate) fn decide_proof_attempt<R: ResetRequestRow>(
    secret_rows: &[R],
    selected: Option<R>,
    submitted_hashed_secret: &str,
) -> (ResetProofAttempt<R>, Vec<i32>) {
    let failure = match selected {
        Some(row) if row.expired() => return (ResetProofAttempt::Expired, Vec::new()),
        Some(row) if !row.has_budget() => return (ResetProofAttempt::Exhausted, Vec::new()),
        Some(row)
            if bool::from(
                submitted_hashed_secret
                    .as_bytes()
                    .ct_eq(row.row_hashed_secret().as_bytes()),
            ) =>
        {
            return (ResetProofAttempt::Valid(row), Vec::new())
        }
        Some(_) => ResetProofAttempt::WrongSecret,
        None => ResetProofAttempt::WrongCode,
    };
    let mut any_active = false;
    let mut charge = Vec::new();
    for row in secret_rows.iter().filter(|row| !row.expired()) {
        any_active = true;
        if row.has_budget() {
            charge.push(row.row_id());
        }
    }
    if any_active && charge.is_empty() {
        return (ResetProofAttempt::Exhausted, Vec::new());
    }
    (failure, charge)
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

#[cfg(test)]
mod tests {
    use super::{decide_proof_attempt, ResetProofAttempt, ResetRequestRow};

    #[derive(Debug, Clone)]
    struct Row {
        id: i32,
        secret: &'static str,
        expired: bool,
        budget: bool,
    }

    impl ResetRequestRow for Row {
        fn row_id(&self) -> i32 {
            self.id
        }
        fn row_hashed_secret(&self) -> &str {
            self.secret
        }
        fn expired(&self) -> bool {
            self.expired
        }
        fn has_budget(&self) -> bool {
            self.budget
        }
    }

    fn row(id: i32, secret: &'static str, expired: bool, budget: bool) -> Row {
        Row {
            id,
            secret,
            expired,
            budget,
        }
    }

    #[test]
    fn valid_proof_is_accepted_without_charging() {
        let rows = [row(1, "s", false, true)];
        let (attempt, charge) = decide_proof_attempt(&rows, Some(rows[0].clone()), "s");
        assert!(matches!(
            attempt,
            ResetProofAttempt::Valid(Row { id: 1, .. })
        ));
        assert!(charge.is_empty());
    }

    #[test]
    fn spent_or_expired_selected_request_is_refused_before_the_secret_is_compared() {
        let spent = [row(1, "s", false, false)];
        let (attempt, charge) = decide_proof_attempt(&spent, Some(spent[0].clone()), "s");
        assert!(matches!(attempt, ResetProofAttempt::Exhausted));
        assert!(charge.is_empty());

        let expired = [row(1, "s", true, true)];
        let (attempt, charge) = decide_proof_attempt(&expired, Some(expired[0].clone()), "s");
        assert!(matches!(attempt, ResetProofAttempt::Expired));
        assert!(charge.is_empty());
    }

    #[test]
    fn failed_guess_charges_only_active_secret_rows_with_budget() {
        let rows = [
            row(1, "s", true, true),
            row(2, "s", false, true),
            row(3, "s", false, false),
        ];
        let (attempt, charge) = decide_proof_attempt(&rows, None, "s");
        assert!(matches!(attempt, ResetProofAttempt::WrongCode));
        assert_eq!(charge, vec![2]);

        // Correct code for another secret's request is a charged failure too.
        let other = row(9, "other", false, true);
        let (attempt, charge) = decide_proof_attempt(&rows, Some(other), "s");
        assert!(matches!(attempt, ResetProofAttempt::WrongSecret));
        assert_eq!(charge, vec![2]);
    }

    #[test]
    fn failed_guess_with_no_budget_left_is_exhausted() {
        let rows = [row(1, "s", true, true), row(2, "s", false, false)];
        let (attempt, charge) = decide_proof_attempt(&rows, None, "s");
        assert!(matches!(attempt, ResetProofAttempt::Exhausted));
        assert!(charge.is_empty());
    }

    #[test]
    fn failed_guess_with_no_active_secret_row_charges_nothing() {
        let rows = [row(1, "s", true, true)];
        let (attempt, charge) = decide_proof_attempt(&rows, None, "s");
        assert!(matches!(attempt, ResetProofAttempt::WrongCode));
        assert!(charge.is_empty());
    }
}
