//! Session storage for tower-sessions, kept in Granary's SQLite database so
//! people stay signed in across restarts.

use async_trait::async_trait;
use sqlx::SqlitePool;
use tower_sessions::{
    SessionStore,
    session::{Id, Record},
    session_store::{Error, Result},
};

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
}

fn backend(e: sqlx::Error) -> Error {
    Error::Backend(e.to_string())
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn save(&self, record: &Record) -> Result<()> {
        let data = serde_json::to_vec(record).map_err(|e| Error::Encode(e.to_string()))?;
        sqlx::query(
            "INSERT INTO sessions (id, data, expiry) VALUES (?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, expiry = excluded.expiry",
        )
        .bind(record.id.to_string())
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
                .bind(id.to_string())
                .bind(now)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        match row {
            Some((data,)) => serde_json::from_slice(&data)
                .map(Some)
                .map_err(|e| Error::Decode(e.to_string())),
            None => Ok(None),
        }
    }

    async fn delete(&self, id: &Id) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}
