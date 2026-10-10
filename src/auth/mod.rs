//! Accounts and access control, built on axum-login (sessions and the
//! authentication backend), tower-sessions (cookies), password-auth (Argon2id
//! password hashing) and totp-rs (two-factor codes). Everything is stored in a
//! local SQLite database; nothing is sent anywhere else.

pub mod api;
mod store;

// Dynamic SQL here joins fixed column lists and clauses; values are always bound.
use sqlx::AssertSqlSafe;
use std::{
    collections::HashMap,
    net::IpAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_login::{AuthUser, AuthnBackend, UserId};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

pub use store::SqliteSessionStore;

use crate::{
    AppState,
    error::{AppError, AppResult},
    store::now_secs,
    util::base_url,
};

pub type AuthSession = axum_login::AuthSession<Backend>;

pub const MIN_PASSWORD_LEN: usize = 12;
const TOKEN_PREFIX: &str = "gran_";

// ------------------------------------------------------------------ users

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub totp_enabled: bool,
    pub disabled: bool,
    pub must_change_password: bool,
    pub created_at: i64,
    pub last_login: Option<i64>,
    #[serde(skip)]
    password_hash: String,
    #[serde(skip)]
    totp_secret: Option<String>,
    #[serde(skip)]
    totp_last_step: i64,
}

// Never print password hashes or 2FA secrets, even in debug output.
impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl AuthUser for User {
    type Id = i64;

    fn id(&self) -> i64 {
        self.id
    }

    /// Sessions are tied to the password hash, so changing a password signs
    /// out every other session for that account.
    fn session_auth_hash(&self) -> &[u8] {
        self.password_hash.as_bytes()
    }
}

const USER_COLUMNS: &str = "id, username, role, totp_enabled, disabled, must_change_password, \
                            created_at, last_login, password_hash, totp_secret, totp_last_step";

type UserRow = (
    i64,
    String,
    String,
    bool,
    bool,
    bool,
    i64,
    Option<i64>,
    String,
    Option<String>,
    i64,
);

fn user_from_row(r: UserRow) -> User {
    User {
        id: r.0,
        username: r.1,
        role: Role::parse(&r.2).unwrap_or(Role::Viewer),
        totp_enabled: r.3,
        disabled: r.4,
        must_change_password: r.5,
        created_at: r.6,
        last_login: r.7,
        password_hash: r.8,
        totp_secret: r.9,
        totp_last_step: r.10,
    }
}

// ---------------------------------------------------------------- database

pub async fn open_db(data_dir: &Path) -> anyhow::Result<SqlitePool> {
    let dir = data_dir.join("state");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("granary.db");
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The database holds password hashes; keep it private to Granary's user.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    for stmt in [
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE COLLATE NOCASE,
            password_hash TEXT NOT NULL,
            role TEXT NOT NULL CHECK (role IN ('admin', 'viewer')),
            totp_secret TEXT,
            totp_pending TEXT,
            totp_enabled INTEGER NOT NULL DEFAULT 0,
            totp_last_step INTEGER NOT NULL DEFAULT 0,
            disabled INTEGER NOT NULL DEFAULT 0,
            must_change_password INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            last_login INTEGER
        )",
        "CREATE TABLE IF NOT EXISTS tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            hint TEXT NOT NULL,
            hash TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL,
            last_used INTEGER,
            expires_at INTEGER
        )",
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            data BLOB NOT NULL,
            expiry INTEGER NOT NULL
        )",
        crate::audit::SCHEMA,
        crate::audit::INDEX,
    ] {
        sqlx::query(stmt).execute(&pool).await?;
    }
    Ok(pool)
}

pub async fn hash_password(password: String) -> anyhow::Result<String> {
    Ok(tokio::task::spawn_blocking(move || password_auth::generate_hash(password)).await?)
}

async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || password_auth::verify_password(password, &hash).is_ok())
        .await
        .unwrap_or(false)
}

pub fn check_password_policy(pw: &str) -> AppResult<()> {
    if pw.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::bad_request(format!(
            "passwords need at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if pw.len() > 1024 {
        return Err(AppError::bad_request("that password is too long"));
    }
    Ok(())
}

/// A random password for new accounts and resets: 24 characters, no lookalikes.
pub fn random_password() -> String {
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("system random number generator");
    bytes
        .iter()
        .map(|b| CHARS[*b as usize % CHARS.len()] as char)
        .collect()
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn valid_username(name: &str) -> bool {
    (2..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Create the first admin account if there are no users yet. Returns the
/// generated password when one had to be made up.
pub async fn ensure_admin(
    pool: &SqlitePool,
    username: &str,
    password: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    if count > 0 {
        return Ok(None);
    }
    if !valid_username(username) {
        anyhow::bail!("GRANARY_ADMIN_USER {username:?} is not a valid username");
    }
    let (password, generated) = match password {
        Some(p) => {
            if p.chars().count() < MIN_PASSWORD_LEN {
                anyhow::bail!(
                    "GRANARY_ADMIN_PASSWORD needs at least {MIN_PASSWORD_LEN} characters"
                );
            }
            (p.to_string(), false)
        }
        None => (random_password(), true),
    };
    let hash = hash_password(password.clone()).await?;
    sqlx::query(
        "INSERT INTO users (username, password_hash, role, must_change_password, created_at)
         VALUES (?, ?, 'admin', ?, ?)",
    )
    .bind(username)
    .bind(hash)
    .bind(generated)
    .bind(now_secs() as i64)
    .execute(pool)
    .await?;
    tracing::info!("created admin account {username:?}");
    Ok(generated.then_some(password))
}

/// `granary reset-password <user> [--disable-2fa]`, for when you're locked out.
pub async fn reset_password_cli(
    data_dir: &Path,
    username: &str,
    disable_2fa: bool,
) -> anyhow::Result<()> {
    let pool = open_db(data_dir).await?;
    let password = random_password();
    let hash = hash_password(password.clone()).await?;
    let r = sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_password = 1, disabled = 0,
            totp_enabled = CASE WHEN ? THEN 0 ELSE totp_enabled END,
            totp_secret = CASE WHEN ? THEN NULL ELSE totp_secret END,
            totp_pending = CASE WHEN ? THEN NULL ELSE totp_pending END
         WHERE username = ?",
    )
    .bind(&hash)
    .bind(disable_2fa)
    .bind(disable_2fa)
    .bind(disable_2fa)
    .bind(username)
    .execute(&pool)
    .await?;
    if r.rows_affected() == 0 {
        let names: Vec<(String,)> = sqlx::query_as("SELECT username FROM users ORDER BY username")
            .fetch_all(&pool)
            .await?;
        let names: Vec<String> = names.into_iter().map(|n| n.0).collect();
        anyhow::bail!("no user named {username:?}. Users: {}", names.join(", "));
    }
    // Sessions are tied to the password hash, so the new password has
    // already signed this account (and only this account) out everywhere.
    crate::audit::record(
        &pool,
        "(server console)",
        "user.reset_password",
        username,
        None,
        true,
    )
    .await;
    println!("New password for {username}: {password}");
    println!("You'll be asked to change it after signing in.");
    if disable_2fa {
        println!("Two-factor authentication is now off for this account.");
    }
    Ok(())
}

// ------------------------------------------------------------- rate limits

#[derive(Default, Clone, Copy)]
struct Attempts {
    failures: u32,
    window_start: u64,
    locked_until: u64,
}

impl Attempts {
    fn bump(&mut self, max: u32, now: u64) {
        if now.saturating_sub(self.window_start) > WINDOW {
            *self = Attempts {
                failures: 0,
                window_start: now,
                locked_until: 0,
            };
        }
        self.failures += 1;
        if self.failures >= max {
            self.locked_until = now + LOCKOUT;
            self.failures = 0;
            self.window_start = now;
        }
    }

    fn active(&self, now: u64) -> bool {
        self.locked_until > now || now.saturating_sub(self.window_start) <= WINDOW
    }
}

/// Slows down password guessing without letting a stranger lock someone out:
///
/// * one address gets `MAX_PER_PAIR` tries at one account before that address
///   is locked out *of that account* (the owner elsewhere can still sign in);
/// * any account gets `MAX_PER_USER` failures from all addresses combined,
///   which stops guessing spread across many machines;
/// * any address gets `MAX_PER_IP` failures across all accounts.
///
/// Entries expire, and each table is capped so made-up usernames can't use
/// up memory.
#[derive(Default)]
pub struct Limiter {
    pairs: Mutex<HashMap<(String, IpAddr), Attempts>>,
    users: Mutex<HashMap<String, Attempts>>,
    ips: Mutex<HashMap<IpAddr, Attempts>>,
}

const WINDOW: u64 = 15 * 60;
const LOCKOUT: u64 = 15 * 60;
const MAX_PER_PAIR: u32 = 5;
const MAX_PER_USER: u32 = 50;
const MAX_PER_IP: u32 = 50;
const MAX_ENTRIES: usize = 10_000;

fn bump_in<K: std::hash::Hash + Eq>(map: &Mutex<HashMap<K, Attempts>>, key: K, max: u32) {
    let now = now_secs();
    let mut map = map.lock().unwrap();
    if map.len() >= MAX_ENTRIES {
        map.retain(|_, a| a.active(now));
        if map.len() >= MAX_ENTRIES {
            // Still full of live entries: keep only the lockouts.
            map.retain(|_, a| a.locked_until > now);
        }
    }
    map.entry(key).or_default().bump(max, now);
}

fn locked_in<K: std::hash::Hash + Eq>(map: &Mutex<HashMap<K, Attempts>>, key: &K) -> u64 {
    let now = now_secs();
    map.lock()
        .unwrap()
        .get(key)
        .map_or(0, |a| a.locked_until.saturating_sub(now))
}

impl Limiter {
    /// Seconds until sign-in is allowed again, if currently locked.
    pub fn locked_for(&self, user: &str, ip: IpAddr) -> Option<u64> {
        let user = user.to_lowercase();
        let secs = locked_in(&self.pairs, &(user.clone(), ip))
            .max(locked_in(&self.users, &user))
            .max(locked_in(&self.ips, &ip));
        (secs > 0).then_some(secs)
    }

    pub fn record_failure(&self, user: &str, ip: IpAddr) {
        let user = user.to_lowercase();
        bump_in(&self.pairs, (user.clone(), ip), MAX_PER_PAIR);
        bump_in(&self.users, user, MAX_PER_USER);
        bump_in(&self.ips, ip, MAX_PER_IP);
    }

    pub fn record_success(&self, user: &str, ip: IpAddr) {
        self.pairs
            .lock()
            .unwrap()
            .remove(&(user.to_lowercase(), ip));
    }

    pub fn unlock(&self, user: &str) {
        let user = user.to_lowercase();
        self.pairs.lock().unwrap().retain(|(u, _), _| *u != user);
        self.users.lock().unwrap().remove(&user);
    }
}

// ----------------------------------------------------------------- backend

#[derive(Clone)]
pub struct Backend {
    pub db: SqlitePool,
    pub limiter: Arc<Limiter>,
    /// Hash checked when a username doesn't exist, so a wrong username takes
    /// as long as a wrong password and can't be told apart by timing.
    dummy_hash: Arc<String>,
    token_used: Arc<Mutex<HashMap<i64, u64>>>,
}

pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Backend {
    pub async fn new(db: SqlitePool) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            limiter: Arc::default(),
            dummy_hash: Arc::new(hash_password(random_password()).await?),
            token_used: Arc::default(),
        })
    }

    pub async fn user_by_id(&self, id: i64) -> sqlx::Result<Option<User>> {
        let row: Option<UserRow> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(user_from_row))
    }

    async fn user_by_name(&self, name: &str) -> sqlx::Result<Option<User>> {
        let row: Option<UserRow> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users WHERE username = ?"
        )))
        .bind(name)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(user_from_row))
    }

    pub async fn list_users(&self) -> sqlx::Result<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users ORDER BY username"
        )))
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(user_from_row).collect())
    }

    /// Check a username and password. Disabled accounts never match.
    async fn check_password(&self, creds: &Credentials) -> sqlx::Result<Option<User>> {
        let user = self.user_by_name(&creds.username).await?;
        let hash = user
            .as_ref()
            .map(|u| u.password_hash.clone())
            .unwrap_or_else(|| (*self.dummy_hash).clone());
        let ok = verify_password(creds.password.clone(), hash).await;
        Ok(user.filter(|u| ok && !u.disabled))
    }

    /// Verify a 2FA code, refusing a code that was already used.
    pub async fn check_totp(&self, user: &User, code: &str) -> sqlx::Result<bool> {
        let Some(secret) = &user.totp_secret else {
            return Ok(false);
        };
        let Some(totp) = totp_for(secret, &user.username) else {
            return Ok(false);
        };
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let Some(step) = totp.check(&code, now_secs()) else {
            return Ok(false);
        };
        if step as i64 <= user.totp_last_step {
            return Ok(false);
        }
        // Only succeeds if no other request used this step first.
        let r =
            sqlx::query("UPDATE users SET totp_last_step = ? WHERE id = ? AND totp_last_step < ?")
                .bind(step as i64)
                .bind(user.id)
                .bind(step as i64)
                .execute(&self.db)
                .await?;
        Ok(r.rows_affected() == 1)
    }

    // --- access tokens

    /// Create a token and return its plaintext, which is never stored.
    pub async fn create_token(
        &self,
        user_id: i64,
        name: &str,
        expires_at: Option<i64>,
    ) -> sqlx::Result<(i64, String)> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("system random number generator");
        let token = format!("{TOKEN_PREFIX}{}", hex::encode(bytes));
        let hint = format!("{}…", &token[..TOKEN_PREFIX.len() + 6]);
        let r = sqlx::query(
            "INSERT INTO tokens (user_id, name, hint, hash, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(name)
        .bind(hint)
        .bind(sha256_hex(&token))
        .bind(now_secs() as i64)
        .bind(expires_at)
        .execute(&self.db)
        .await?;
        Ok((r.last_insert_rowid(), token))
    }

    /// The account a token belongs to, if the token is valid right now.
    pub async fn user_for_token(&self, token: &str) -> sqlx::Result<Option<User>> {
        if !token.starts_with(TOKEN_PREFIX) || token.len() != TOKEN_PREFIX.len() + 64 {
            return Ok(None);
        }
        let now = now_secs() as i64;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT t.id, t.user_id FROM tokens t JOIN users u ON u.id = t.user_id
             WHERE t.hash = ? AND u.disabled = 0 AND (t.expires_at IS NULL OR t.expires_at > ?)",
        )
        .bind(sha256_hex(token))
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        let Some((token_id, user_id)) = row else {
            return Ok(None);
        };
        // Record use at most once a minute per token, not on every file.
        let stale = {
            let mut used = self.token_used.lock().unwrap();
            let last = used.get(&token_id).copied().unwrap_or(0);
            if now as u64 - last >= 60 {
                used.insert(token_id, now as u64);
                true
            } else {
                false
            }
        };
        if stale {
            sqlx::query("UPDATE tokens SET last_used = ? WHERE id = ?")
                .bind(now)
                .bind(token_id)
                .execute(&self.db)
                .await?;
        }
        self.user_by_id(user_id).await
    }
}

pub fn totp_for(secret_b32: &str, username: &str) -> Option<totp_rs::Totp> {
    let secret = totp_rs::Secret::try_from_base32(secret_b32).ok()?;
    totp_rs::Builder::new()
        .with_secret(secret)
        .with_account_name(username.to_string())
        .with_issuer(Some("Granary"))
        .build()
        .ok()
}

impl AuthnBackend for Backend {
    type User = User;
    type Credentials = Credentials;
    type Error = sqlx::Error;

    async fn authenticate(&self, creds: Credentials) -> Result<Option<User>, sqlx::Error> {
        self.check_password(&creds).await
    }

    async fn get_user(&self, id: &UserId<Self>) -> Result<Option<User>, sqlx::Error> {
        // Disabled accounts lose their sessions on the next request.
        Ok(self.user_by_id(*id).await?.filter(|u| !u.disabled))
    }
}

// --------------------------------------------------------------- guards

/// The signed-in user, or 401.
pub fn current_user(auth: &AuthSession) -> AppResult<&User> {
    auth.user
        .as_ref()
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "sign in required"))
}

/// The signed-in user if they are an admin, otherwise 403.
pub fn require_admin(auth: &AuthSession) -> AppResult<&User> {
    let user = current_user(auth)?;
    if user.role != Role::Admin {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "this needs an admin account",
        ));
    }
    Ok(user)
}

/// Middleware for `/api`: changes must come from Granary's own pages (the
/// custom header can't be sent cross-site without CORS, which Granary never
/// grants), and everything except signing in needs a session.
pub async fn api_guard(auth: AuthSession, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes && !req.headers().contains_key("x-granary-request") {
        return AppError::new(StatusCode::FORBIDDEN, "missing X-Granary-Request header")
            .into_response();
    }
    if path.starts_with("/api/auth/") {
        return next.run(req).await;
    }
    let Some(user) = &auth.user else {
        return AppError::new(StatusCode::UNAUTHORIZED, "sign in required").into_response();
    };
    if user.must_change_password && path != "/api/account/password" {
        return AppError::new(StatusCode::FORBIDDEN, "change your password to continue")
            .into_response();
    }
    next.run(req).await
}

/// Pull an access token out of whichever header a package manager uses:
/// `Bearer <token>` (npm), Basic auth with the token as password (pip, uv,
/// Maven, Gradle)
/// or a bare token (Cargo).
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    if let Some(t) = value.strip_prefix("Bearer ") {
        return Some(t.trim().to_string());
    }
    if let Some(b64) = value.strip_prefix("Basic ") {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let raw = STANDARD.decode(b64.trim()).ok()?;
        let text = String::from_utf8(raw).ok()?;
        let (user, pass) = text.split_once(':').unwrap_or((&text, ""));
        let token = if pass.is_empty() { user } else { pass };
        return Some(token.to_string());
    }
    Some(value.to_string())
}

/// Middleware for package routes: when client authentication is on, require
/// a valid access token or a signed-in browser session.
pub async fn client_guard(
    State(st): State<AppState>,
    auth: AuthSession,
    req: Request,
    next: Next,
) -> Response {
    if !st.cfg.client_auth || auth.user.is_some() {
        return next.run(req).await;
    }
    if let Some(token) = token_from_headers(req.headers()) {
        match st.auth.user_for_token(&token).await {
            Ok(Some(_)) => return next.run(req).await,
            Ok(None) => {}
            Err(e) => {
                return AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("token check failed: {e}"),
                )
                .into_response();
            }
        }
    }
    let base = base_url(&st.cfg, req.headers());
    let path = req.uri().path();
    let challenge = if path.starts_with("/cargo/") {
        format!("Cargo login_url=\"{base}/#/tokens\"")
    } else if path.starts_with("/maven/") {
        // Maven only sends its <server> credentials after this challenge.
        "Basic realm=\"granary\"".to_string()
    } else {
        "Basic realm=\"Granary\", charset=\"UTF-8\"".to_string()
    };
    let mut resp = AppError::new(
        StatusCode::UNAUTHORIZED,
        "an access token is required. Create one in Granary under Access tokens",
    )
    .into_response();
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_header_forms() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer gran_x"),
        );
        assert_eq!(token_from_headers(&h).as_deref(), Some("gran_x"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("gran_y"));
        assert_eq!(token_from_headers(&h).as_deref(), Some("gran_y"));
        // __token__:gran_z
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic X190b2tlbl9fOmdyYW5feg=="),
        );
        assert_eq!(token_from_headers(&h).as_deref(), Some("gran_z"));
    }

    #[test]
    fn usernames_and_passwords() {
        assert!(valid_username("dev"));
        assert!(valid_username("first.last-2"));
        assert!(!valid_username("a"));
        assert!(!valid_username("bad name"));
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy("long enough pass").is_ok());
        let p = random_password();
        assert_eq!(p.len(), 24);
    }

    #[test]
    fn limiter_locks_after_failures() {
        let l = Limiter::default();
        let ip = IpAddr::from([10, 0, 0, 1]);
        let owner = IpAddr::from([10, 0, 0, 2]);
        for _ in 0..MAX_PER_PAIR - 1 {
            l.record_failure("Dev", ip);
        }
        assert!(l.locked_for("dev", ip).is_none());
        l.record_failure("dev", ip);
        assert!(l.locked_for("DEV", ip).is_some());
        // The account's owner on another machine is not locked out.
        assert!(l.locked_for("dev", owner).is_none());
        l.unlock("dev");
        assert!(l.locked_for("dev", ip).is_none());
    }

    #[test]
    fn limiter_stops_distributed_guessing_and_stays_bounded() {
        let l = Limiter::default();
        for i in 0..MAX_PER_USER {
            l.record_failure("admin", IpAddr::from([10, 1, (i / 256) as u8, i as u8]));
        }
        assert!(
            l.locked_for("admin", IpAddr::from([192, 168, 0, 9]))
                .is_some()
        );
        for i in 0..(MAX_ENTRIES as u32 + 50) {
            l.record_failure(&format!("nobody{i}"), IpAddr::from([10, 9, 9, 9]));
        }
        assert!(l.users.lock().unwrap().len() <= MAX_ENTRIES);
        assert!(l.pairs.lock().unwrap().len() <= MAX_ENTRIES);
    }
}
