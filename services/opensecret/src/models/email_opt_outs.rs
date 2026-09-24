use crate::models::schema::email_opt_outs;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EmailOptOutError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
}

/// Where an opt-out came from, kept so support can answer "I never
/// unsubscribed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptOutSource {
    /// A mail client's native unsubscribe button (RFC 8058 one-click POST).
    OneClick,
    /// The unsubscribe page linked from an email footer.
    Page,
    /// Support, on the user's request.
    Support,
}

impl OptOutSource {
    pub fn as_str(self) -> &'static str {
        match self {
            OptOutSource::OneClick => "one_click",
            OptOutSource::Page => "page",
            OptOutSource::Support => "support",
        }
    }
}

#[derive(Queryable, Debug, Clone)]
#[diesel(table_name = email_opt_outs)]
pub struct EmailOptOut {
    pub user_id: Uuid,
    pub source: String,
    pub opted_out_at: DateTime<Utc>,
}

#[derive(Insertable)]
#[diesel(table_name = email_opt_outs)]
struct NewEmailOptOut<'a> {
    user_id: Uuid,
    source: &'a str,
}

impl EmailOptOut {
    pub fn get_by_user_id(
        conn: &mut PgConnection,
        lookup_user_id: Uuid,
    ) -> Result<Option<EmailOptOut>, EmailOptOutError> {
        email_opt_outs::table
            .filter(email_opt_outs::user_id.eq(lookup_user_id))
            .first::<EmailOptOut>(conn)
            .optional()
            .map_err(EmailOptOutError::DatabaseError)
    }

    /// Idempotent: opting out again keeps the original record.
    pub fn opt_out(
        conn: &mut PgConnection,
        user_id: Uuid,
        source: OptOutSource,
    ) -> Result<(), EmailOptOutError> {
        diesel::insert_into(email_opt_outs::table)
            .values(NewEmailOptOut {
                user_id,
                source: source.as_str(),
            })
            .on_conflict(email_opt_outs::user_id)
            .do_nothing()
            .execute(conn)
            .map(|_| ())
            .map_err(EmailOptOutError::DatabaseError)
    }

    /// Idempotent: resubscribing someone who isn't opted out is a no-op.
    pub fn opt_in(conn: &mut PgConnection, user_id: Uuid) -> Result<(), EmailOptOutError> {
        diesel::delete(email_opt_outs::table.filter(email_opt_outs::user_id.eq(user_id)))
            .execute(conn)
            .map(|_| ())
            .map_err(EmailOptOutError::DatabaseError)
    }
}
