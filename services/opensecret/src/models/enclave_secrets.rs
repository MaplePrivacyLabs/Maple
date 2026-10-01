use crate::models::schema::enclave_secrets;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EnclaveSecretError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
}

#[derive(Queryable, Identifiable, AsChangeset, Serialize, Deserialize, Clone)]
#[diesel(table_name = enclave_secrets)]
pub struct EnclaveSecret {
    pub id: i32,
    pub key: String,
    pub value: Vec<u8>,
}

impl std::fmt::Debug for EnclaveSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnclaveSecret")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("value", &"[redacted]")
            .finish()
    }
}

impl EnclaveSecret {
    pub fn get_by_id(
        conn: &mut PgConnection,
        lookup_id: i32,
    ) -> Result<Option<EnclaveSecret>, EnclaveSecretError> {
        enclave_secrets::table
            .filter(enclave_secrets::id.eq(lookup_id))
            .first::<EnclaveSecret>(conn)
            .optional()
            .map_err(EnclaveSecretError::DatabaseError)
    }

    pub fn get_by_key(
        conn: &mut PgConnection,
        lookup_key: &str,
    ) -> Result<Option<EnclaveSecret>, EnclaveSecretError> {
        enclave_secrets::table
            .filter(enclave_secrets::key.eq(lookup_key))
            .first::<EnclaveSecret>(conn)
            .optional()
            .map_err(EnclaveSecretError::DatabaseError)
    }

    pub fn get_all(conn: &mut PgConnection) -> Result<Vec<EnclaveSecret>, EnclaveSecretError> {
        enclave_secrets::table
            .load::<EnclaveSecret>(conn)
            .map_err(EnclaveSecretError::DatabaseError)
    }

    pub fn update(&self, conn: &mut PgConnection) -> Result<(), EnclaveSecretError> {
        diesel::update(enclave_secrets::table)
            .filter(enclave_secrets::id.eq(self.id))
            .set(self)
            .execute(conn)
            .map(|_| ())
            .map_err(EnclaveSecretError::DatabaseError)
    }

    pub fn delete(&self, conn: &mut PgConnection) -> Result<(), EnclaveSecretError> {
        diesel::delete(enclave_secrets::table)
            .filter(enclave_secrets::id.eq(self.id))
            .execute(conn)
            .map(|_| ())
            .map_err(EnclaveSecretError::DatabaseError)
    }
}

#[derive(Insertable)]
#[diesel(table_name = enclave_secrets)]
pub struct NewEnclaveSecret {
    pub key: String,
    pub value: Vec<u8>,
}

impl NewEnclaveSecret {
    pub fn new(key: String, value: Vec<u8>) -> Self {
        NewEnclaveSecret { key, value }
    }

    pub fn insert(&self, conn: &mut PgConnection) -> Result<EnclaveSecret, EnclaveSecretError> {
        diesel::insert_into(enclave_secrets::table)
            .values(self)
            .on_conflict(enclave_secrets::key)
            .do_update()
            .set(enclave_secrets::value.eq(self.value.clone()))
            .get_result::<EnclaveSecret>(conn)
            .map_err(EnclaveSecretError::DatabaseError)
    }
}
