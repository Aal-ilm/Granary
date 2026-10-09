//! Session storage for tower-sessions, kept in Granary's SQLite database so
//! people stay signed in across restarts. Rows are keyed by a SHA-256 hash of
//! the session ID and the ID itself is never stored, so a copy of the database
//! can't be used to take over a signed-in session.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tower_sessions::{
    SessionStore,
    session::{Id, Record},
    session_store::{Error, Result},
};

/// What is stored for a session: everything except its ID.
#[derive(Serialize, Deserialize)]
struct Stored {
    data: std::collections::HashMap<String, serde_json::Value>,
    #[serde(with = "time::serde::timestamp")]
    expiry_date: time::OffsetDateTime,
}

fn key(id: &Id) -> String {
    format!(
        "h:{}",
        hex::encode(Sha256::digest(id.to_string().as_bytes()))
    )
}

#[derive(Clone, Debug)]
pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Delete expired sessions. Returns how many were removed.
    pub async fn delete_expired(&self) -> sqlx::Result<u64> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let r = sqlx::query("DELETE FROM sessions WHERE expiry <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Drop sessions saved by older versions, which stored the raw ID.
    pub async fn remove_unhashed(&self) -> sqlx::Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE id NOT LIKE 'h:%'")
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

fn backend(e: sqlx::Error) -> Error {
    Error::Backend(e.to_string())
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn save(&self, record: &Record) -> Result<()> {
        let stored = Stored {
            data: record.data.clone(),
            expiry_date: record.expiry_date,
        };
        let data = serde_json::to_vec(&stored).map_err(|e| Error::Encode(e.to_string()))?;
        sqlx::query(
            "INSERT INTO sessions (id, data, expiry) VALUES (?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, expiry = excluded.expiry",
        )
        .bind(key(&record.id))
        .bind(data)
        .bind(record.expiry_date.unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn load(&self, id: &Id) -> Result<Option<Record>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT data FROM sessions WHERE id = ? AND expiry > ?")
                .bind(key(id))
                .bind(now)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        match row {
            Some((data,)) => serde_json::from_slice::<Stored>(&data)
                .map(|s| {
                    Some(Record {
                        id: *id,
                        data: s.data,
                        expiry_date: s.expiry_date,
                    })
                })
                .map_err(|e| Error::Decode(e.to_string())),
            None => Ok(None),
        }
    }

    async fn delete(&self, id: &Id) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(key(id))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}
