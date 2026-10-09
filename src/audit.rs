//! Persistent audit trail of security-relevant actions: sign-ins, account
//! and token changes, deletions and settings changes. Stored in Granary's
//! SQLite database (unlike the in-memory log view, it survives restarts) and
//! pruned after `GRANARY_AUDIT_DAYS`.

use std::net::IpAddr;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
// Dynamic SQL below is built only from fixed fragments; every value is bound.
use sqlx::{AssertSqlSafe, SqlitePool};

use crate::{
    AppState,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    store::now_secs,
};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    ip TEXT NOT NULL DEFAULT '',
    ok INTEGER NOT NULL DEFAULT 1
)";
pub const INDEX: &str = "CREATE INDEX IF NOT EXISTS audit_at ON audit (at)";

/// Record one action. Failures are logged, never returned: auditing must not
/// stop the action itself from working.
pub async fn record(
    db: &SqlitePool,
    actor: &str,
    action: &str,
    target: &str,
    ip: Option<IpAddr>,
    ok: bool,
) {
    let r = sqlx::query(
        "INSERT INTO audit (at, actor, action, target, ip, ok) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(now_secs() as i64)
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(ip.map(|i| i.to_string()).unwrap_or_default())
    .bind(ok)
    .execute(db)
    .await;
    if let Err(e) = r {
        tracing::warn!("could not write audit entry {action}: {e}");
    }
}

/// Delete entries older than `days`. Returns how many were removed.
pub async fn prune(db: &SqlitePool, days: u32) -> sqlx::Result<u64> {
    let cutoff = now_secs() as i64 - days as i64 * 86_400;
    let r = sqlx::query("DELETE FROM audit WHERE at < ?")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(r.rows_affected())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/audit", get(list))
        .route("/api/audit.csv", get(export_csv))
}

#[derive(Deserialize)]
struct AuditQuery {
    q: Option<String>,
    action: Option<String>,
    failed: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct Entry {
    id: i64,
    at: i64,
    actor: String,
    action: String,
    target: String,
    ip: String,
    ok: bool,
}

/// Build the WHERE clause shared by the list and the CSV export.
fn filter(q: &AuditQuery) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut binds = Vec::new();
    if let Some(needle) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push(
            "(actor LIKE ? ESCAPE '\\' OR target LIKE ? ESCAPE '\\' OR ip LIKE ? ESCAPE '\\')",
        );
        let like = format!(
            "%{}%",
            needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        binds.extend([like.clone(), like.clone(), like]);
    }
    if let Some(action) = q.action.as_deref().filter(|s| !s.is_empty()) {
        // Match a whole family too: "user" matches "user.create", "user.delete"…
        clauses.push("(action = ? OR action LIKE ?)");
        binds.push(action.to_string());
        binds.push(format!("{}.%", action.replace('%', "")));
    }
    if q.failed == Some(true) {
        clauses.push("ok = 0");
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (sql, binds)
}

async fn query_entries(
    db: &SqlitePool,
    q: &AuditQuery,
    limit: i64,
    offset: i64,
) -> sqlx::Result<(i64, Vec<Entry>)> {
    let (clause, binds) = filter(q);
    let count_sql = format!("SELECT COUNT(*) FROM audit{clause}");
    let mut count = sqlx::query_as::<_, (i64,)>(AssertSqlSafe(count_sql));
    for b in &binds {
        count = count.bind(b);
    }
    let (total,) = count.fetch_one(db).await?;
    let sql = format!(
        "SELECT id, at, actor, action, target, ip, ok FROM audit{clause} ORDER BY id DESC LIMIT ? OFFSET ?"
    );
    let mut rows = sqlx::query_as::<_, Entry>(AssertSqlSafe(sql));
    for b in &binds {
        rows = rows.bind(b);
    }
    let entries = rows.bind(limit).bind(offset).fetch_all(db).await?;
    Ok((total, entries))
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        format!("database error: {e}"),
    )
}

async fn list(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<AuditQuery>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&auth)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let (total, entries) = query_entries(&st.auth.db, &q, limit, offset)
        .await
        .map_err(db_err)?;
    Ok(Json(json!({
        "total": total,
        "offset": offset,
        "entries": entries,
        "retention_days": st.cfg.audit_days,
    })))
}

/// Quote a CSV field, and neutralise leading characters that spreadsheet
/// programs would treat as a formula.
fn csv_field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    format!("\"{}\"", s.replace('"', "\"\""))
}

async fn export_csv(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<AuditQuery>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let (_, entries) = query_entries(&st.auth.db, &q, 100_000, 0)
        .await
        .map_err(db_err)?;
    let mut out = String::from("time_utc,actor,action,target,ip,result\n");
    for e in entries {
        let at = time::OffsetDateTime::from_unix_timestamp(e.at)
            .ok()
            .and_then(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            csv_field(&at),
            csv_field(&e.actor),
            csv_field(&e.action),
            csv_field(&e.target),
            csv_field(&e.ip),
            if e.ok { "ok" } else { "failed" }
        ));
    }
    record(&st.auth.db, &admin, "audit.export", "", None, true).await;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"granary-audit.csv\"",
            ),
        ],
        out,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_and_defused() {
        assert_eq!(csv_field("plain"), "\"plain\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=cmd()"), "\"'=cmd()\"");
    }
}
