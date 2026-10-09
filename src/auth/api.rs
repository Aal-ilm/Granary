//! Sign-in, account, user management and access token endpoints.

use std::net::IpAddr;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    AuthSession, Credentials, Role, User, check_password_policy, current_user, hash_password,
    random_password, require_admin, totp_for, valid_username,
};
use crate::{
    AppState,
    error::{AppError, AppResult},
    net::ClientIp,
    store::now_secs,
};

async fn audit(st: &AppState, actor: &str, action: &str, target: &str, ip: IpAddr, ok: bool) {
    crate::audit::record(&st.auth.db, actor, action, target, Some(ip), ok).await;
}

/// Re-check the signed-in user's password before a sensitive change, with
/// the same lockout as signing in so a borrowed session can't be used to
/// guess it.
async fn confirm_password(
    st: &AppState,
    user: &User,
    password: String,
    ip: IpAddr,
) -> AppResult<()> {
    let limiter = &st.auth.limiter;
    if let Some(secs) = limiter.locked_for(&user.username, ip) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many wrong passwords. Try again in {} minutes",
                secs.div_ceil(60).max(1)
            ),
        ));
    }
    let creds = Credentials {
        username: user.username.clone(),
        password,
    };
    if st
        .auth
        .check_password(&creds)
        .await
        .map_err(db_err)?
        .is_none()
    {
        limiter.record_failure(&user.username, ip);
        audit(
            st,
            &user.username,
            "account.password_check",
            &user.username,
            ip,
            false,
        )
        .await;
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "your password is wrong",
        ));
    }
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/account/password", post(change_password))
        .route("/api/account/2fa/setup", post(totp_setup))
        .route("/api/account/2fa/enable", post(totp_enable))
        .route("/api/account/2fa/disable", post(totp_disable))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", delete(revoke_token))
        .route("/api/users", get(list_users).post(create_user))
        .route(
            "/api/users/{id}",
            axum::routing::patch(update_user).delete(delete_user),
        )
        .route("/api/users/{id}/reset-password", post(reset_password))
        .route("/api/users/{id}/reset-2fa", post(reset_totp))
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("database error: {e}"),
    )
}

// ------------------------------------------------------------------ sign-in

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
    code: Option<String>,
}

async fn login(
    State(st): State<AppState>,
    mut auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<LoginBody>,
) -> AppResult<Json<Value>> {
    let username = body.username.trim().to_string();
    if username.len() > 64 || body.password.len() > 1024 {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "wrong username or password",
        ));
    }
    let limiter = &st.auth.limiter;

    if let Some(secs) = limiter.locked_for(&username, ip) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many failed sign-ins. Try again in {} minutes",
                secs.div_ceil(60).max(1)
            ),
        ));
    }

    let wrong = || AppError::new(StatusCode::UNAUTHORIZED, "wrong username or password");

    let creds = Credentials {
        username: username.clone(),
        password: body.password,
    };
    let user = match auth.authenticate(creds).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            limiter.record_failure(&username, ip);
            tracing::warn!("failed sign-in for {username:?} from {ip}");
            audit(&st, &username, "auth.login", &username, ip, false).await;
            return Err(wrong());
        }
        Err(e) => {
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
            ));
        }
    };

    if user.totp_enabled {
        let Some(code) = body.code.filter(|c| !c.trim().is_empty()) else {
            // Password was right; ask for the second factor.
            return Ok(Json(json!({ "status": "totp_required" })));
        };
        if !st.auth.check_totp(&user, &code).await.map_err(db_err)? {
            limiter.record_failure(&username, ip);
            tracing::warn!("failed two-factor code for {:?} from {ip}", user.username);
            audit(
                &st,
                &user.username,
                "auth.login_2fa",
                &user.username,
                ip,
                false,
            )
            .await;
            return Err(AppError::new(
                StatusCode::UNAUTHORIZED,
                "that code didn't work. Wait for a new one and try again",
            ));
        }
    }

    auth.login(&user)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    limiter.record_success(&username, ip);
    sqlx::query("UPDATE users SET last_login = ? WHERE id = ?")
        .bind(now_secs() as i64)
        .bind(user.id)
        .execute(&st.auth.db)
        .await
        .map_err(db_err)?;
    tracing::info!("{} signed in from {ip}", user.username);
    audit(&st, &user.username, "auth.login", &user.username, ip, true).await;
    Ok(Json(json!({ "status": "ok", "user": user })))
}

async fn logout(
    State(st): State<AppState>,
    mut auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
) -> AppResult<Json<Value>> {
    if let Ok(Some(user)) = auth.logout().await {
        tracing::info!("{} signed out", user.username);
        audit(&st, &user.username, "auth.logout", &user.username, ip, true).await;
    }
    Ok(Json(json!({ "status": "ok" })))
}

async fn me(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?;
    Ok(Json(json!({
        "user": user,
        "client_auth": st.cfg.client_auth,
        "min_password_length": super::MIN_PASSWORD_LEN,
    })))
}

// ------------------------------------------------------------------ account

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

async fn change_password(
    State(st): State<AppState>,
    mut auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<PasswordBody>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?.clone();
    check_password_policy(&body.new)?;
    if body.current == body.new {
        return Err(AppError::bad_request(
            "choose a password different from your current one",
        ));
    }
    confirm_password(&st, &user, body.current, ip).await?;
    let hash = hash_password(body.new)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    sqlx::query("UPDATE users SET password_hash = ?, must_change_password = 0 WHERE id = ?")
        .bind(hash)
        .bind(user.id)
        .execute(&st.auth.db)
        .await
        .map_err(db_err)?;
    // The new hash signs out every other session; keep this one signed in.
    let updated = st
        .auth
        .user_by_id(user.id)
        .await
        .map_err(db_err)?
        .ok_or_else(AppError::not_found)?;
    auth.login(&updated)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!("{} changed their password", user.username);
    audit(
        &st,
        &user.username,
        "account.password_change",
        &user.username,
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok", "user": updated })))
}

async fn totp_setup(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?;
    if user.totp_enabled {
        return Err(AppError::bad_request(
            "two-factor authentication is already on",
        ));
    }
    let secret = totp_rs::Secret::generate().to_base32();
    let totp = totp_for(&secret, &user.username).ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not create a 2FA secret",
        )
    })?;
    let qr = totp
        .to_qr_base64()
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let uri = totp
        .to_url()
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    sqlx::query("UPDATE users SET totp_pending = ? WHERE id = ?")
        .bind(&secret)
        .bind(user.id)
        .execute(&st.auth.db)
        .await
        .map_err(db_err)?;
    Ok(Json(json!({
        "secret": secret,
        "uri": uri,
        "qr": format!("data:image/png;base64,{qr}"),
    })))
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn totp_enable(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<CodeBody>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?.clone();
    let (pending,): (Option<String>,) =
        sqlx::query_as("SELECT totp_pending FROM users WHERE id = ?")
            .bind(user.id)
            .fetch_one(&st.auth.db)
            .await
            .map_err(db_err)?;
    let Some(secret) = pending else {
        return Err(AppError::bad_request("start two-factor setup first"));
    };
    let totp = totp_for(&secret, &user.username)
        .ok_or_else(|| AppError::bad_request("start two-factor setup again"))?;
    let code: String = body.code.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(step) = totp.check(&code, now_secs()) else {
        return Err(AppError::bad_request(
            "that code didn't match. Check the time on your phone and try the next code",
        ));
    };
    sqlx::query(
        "UPDATE users SET totp_secret = totp_pending, totp_pending = NULL, totp_enabled = 1,
            totp_last_step = ? WHERE id = ?",
    )
    .bind(step as i64)
    .bind(user.id)
    .execute(&st.auth.db)
    .await
    .map_err(db_err)?;
    tracing::info!("{} turned on two-factor authentication", user.username);
    audit(
        &st,
        &user.username,
        "account.2fa_enable",
        &user.username,
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok" })))
}

#[derive(Deserialize)]
struct DisableTotpBody {
    password: String,
}

async fn totp_disable(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<DisableTotpBody>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?.clone();
    confirm_password(&st, &user, body.password, ip).await?;
    sqlx::query(
        "UPDATE users SET totp_enabled = 0, totp_secret = NULL, totp_pending = NULL WHERE id = ?",
    )
    .bind(user.id)
    .execute(&st.auth.db)
    .await
    .map_err(db_err)?;
    tracing::info!("{} turned off two-factor authentication", user.username);
    audit(
        &st,
        &user.username,
        "account.2fa_disable",
        &user.username,
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok" })))
}

// ------------------------------------------------------------------- tokens

#[derive(Serialize, sqlx::FromRow)]
struct TokenRow {
    id: i64,
    user_id: i64,
    username: String,
    name: String,
    hint: String,
    created_at: i64,
    last_used: Option<i64>,
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct TokenQuery {
    all: Option<bool>,
}

async fn list_tokens(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<TokenQuery>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?;
    let all = q.all.unwrap_or(false) && user.role == Role::Admin;
    let sql = "SELECT t.id, t.user_id, u.username, t.name, t.hint, t.created_at, t.last_used, t.expires_at
               FROM tokens t JOIN users u ON u.id = t.user_id";
    let rows: Vec<TokenRow> = if all {
        sqlx::query_as(&format!("{sql} ORDER BY u.username, t.created_at DESC"))
            .fetch_all(&st.auth.db)
            .await
    } else {
        sqlx::query_as(&format!(
            "{sql} WHERE t.user_id = ? ORDER BY t.created_at DESC"
        ))
        .bind(user.id)
        .fetch_all(&st.auth.db)
        .await
    }
    .map_err(db_err)?;
    Ok(Json(json!({ "tokens": rows, "now": now_secs() })))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
    expires_days: Option<u32>,
}

async fn create_token(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<NewToken>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "give the token a name of up to 80 characters",
        ));
    }
    let expires_at = match body.expires_days {
        Some(0) | None => None,
        Some(d) if d <= 3650 => Some(now_secs() as i64 + d as i64 * 86_400),
        Some(_) => return Err(AppError::bad_request("tokens can last at most 3650 days")),
    };
    let (id, token) = st
        .auth
        .create_token(user.id, name, expires_at)
        .await
        .map_err(db_err)?;
    tracing::info!("{} created access token {name:?}", user.username);
    audit(
        &st,
        &user.username,
        "token.create",
        &format!("#{id} {name}"),
        ip,
        true,
    )
    .await;
    Ok(Json(
        json!({ "id": id, "token": token, "name": name, "expires_at": expires_at }),
    ))
}

async fn revoke_token(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let user = current_user(&auth)?;
    let r = if user.role == Role::Admin {
        sqlx::query("DELETE FROM tokens WHERE id = ?")
            .bind(id)
            .execute(&st.auth.db)
            .await
    } else {
        sqlx::query("DELETE FROM tokens WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user.id)
            .execute(&st.auth.db)
            .await
    }
    .map_err(db_err)?;
    if r.rows_affected() == 0 {
        return Err(AppError::not_found());
    }
    tracing::info!("{} revoked access token #{id}", user.username);
    audit(
        &st,
        &user.username,
        "token.revoke",
        &format!("#{id}"),
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok" })))
}

// -------------------------------------------------------------------- users

async fn list_users(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let users = st.auth.list_users().await.map_err(db_err)?;
    let counts: Vec<(i64, i64)> =
        sqlx::query_as("SELECT user_id, COUNT(*) FROM tokens GROUP BY user_id")
            .fetch_all(&st.auth.db)
            .await
            .map_err(db_err)?;
    let list: Vec<Value> = users
        .iter()
        .map(|u| {
            let tokens = counts.iter().find(|c| c.0 == u.id).map_or(0, |c| c.1);
            let mut v = json!(u);
            v["tokens"] = json!(tokens);
            v
        })
        .collect();
    Ok(Json(json!({ "users": list })))
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    role: String,
}

async fn create_user(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<NewUser>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let username = body.username.trim();
    if !valid_username(username) {
        return Err(AppError::bad_request(
            "usernames are 2 to 64 letters, digits, dots, dashes or underscores",
        ));
    }
    let role = Role::parse(&body.role)
        .ok_or_else(|| AppError::bad_request("role must be admin or viewer"))?;
    let password = random_password();
    let hash = hash_password(password.clone())
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let r = sqlx::query(
        "INSERT INTO users (username, password_hash, role, must_change_password, created_at)
         VALUES (?, ?, ?, 1, ?)",
    )
    .bind(username)
    .bind(hash)
    .bind(role.as_str())
    .bind(now_secs() as i64)
    .execute(&st.auth.db)
    .await;
    match r {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                format!("there is already a user named {username}"),
            ));
        }
        Err(e) => return Err(db_err(e)),
    }
    tracing::info!(
        "{} created user {username:?} ({})",
        admin.username,
        role.as_str()
    );
    audit(
        &st,
        &admin.username,
        "user.create",
        &format!("{username} ({})", role.as_str()),
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "username": username, "password": password })))
}

/// Refuse changes that would leave no active admin.
async fn ensure_other_admin(st: &AppState, target: &User) -> AppResult<()> {
    if target.role != Role::Admin || target.disabled {
        return Ok(());
    }
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0 AND id != ?",
    )
    .bind(target.id)
    .fetch_one(&st.auth.db)
    .await
    .map_err(db_err)?;
    if n == 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "this is the only admin. Make another user an admin first",
        ));
    }
    Ok(())
}

async fn load_user(st: &AppState, id: i64) -> AppResult<User> {
    st.auth
        .user_by_id(id)
        .await
        .map_err(db_err)?
        .ok_or_else(AppError::not_found)
}

#[derive(Deserialize)]
struct UserUpdate {
    role: Option<String>,
    disabled: Option<bool>,
}

async fn update_user(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(id): Path<i64>,
    Json(body): Json<UserUpdate>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.clone();
    let target = load_user(&st, id).await?;
    if let Some(role) = &body.role {
        let role = Role::parse(role)
            .ok_or_else(|| AppError::bad_request("role must be admin or viewer"))?;
        if role != Role::Admin {
            ensure_other_admin(&st, &target).await?;
        }
        sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(role.as_str())
            .bind(id)
            .execute(&st.auth.db)
            .await
            .map_err(db_err)?;
        tracing::info!(
            "{} made {} a {}",
            admin.username,
            target.username,
            role.as_str()
        );
        audit(
            &st,
            &admin.username,
            "user.role",
            &format!("{} -> {}", target.username, role.as_str()),
            ip,
            true,
        )
        .await;
    }
    if let Some(disabled) = body.disabled {
        if disabled {
            if target.id == admin.id {
                return Err(AppError::bad_request("you can't disable your own account"));
            }
            ensure_other_admin(&st, &target).await?;
        }
        sqlx::query("UPDATE users SET disabled = ? WHERE id = ?")
            .bind(disabled)
            .bind(id)
            .execute(&st.auth.db)
            .await
            .map_err(db_err)?;
        tracing::info!(
            "{} {} {}",
            admin.username,
            if disabled { "disabled" } else { "enabled" },
            target.username
        );
        let action = if disabled {
            "user.disable"
        } else {
            "user.enable"
        };
        audit(&st, &admin.username, action, &target.username, ip, true).await;
    }
    Ok(Json(json!({ "user": load_user(&st, id).await? })))
}

async fn reset_password(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let target = load_user(&st, id).await?;
    let password = random_password();
    let hash = hash_password(password.clone())
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    sqlx::query("UPDATE users SET password_hash = ?, must_change_password = 1 WHERE id = ?")
        .bind(hash)
        .bind(id)
        .execute(&st.auth.db)
        .await
        .map_err(db_err)?;
    st.auth.limiter.unlock(&target.username);
    tracing::info!(
        "{} reset the password for {}",
        admin.username,
        target.username
    );
    audit(
        &st,
        &admin.username,
        "user.reset_password",
        &target.username,
        ip,
        true,
    )
    .await;
    Ok(Json(
        json!({ "username": target.username, "password": password }),
    ))
}

async fn reset_totp(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let target = load_user(&st, id).await?;
    sqlx::query(
        "UPDATE users SET totp_enabled = 0, totp_secret = NULL, totp_pending = NULL WHERE id = ?",
    )
    .bind(id)
    .execute(&st.auth.db)
    .await
    .map_err(db_err)?;
    tracing::info!(
        "{} turned off two-factor authentication for {}",
        admin.username,
        target.username
    );
    audit(
        &st,
        &admin.username,
        "user.reset_2fa",
        &target.username,
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok" })))
}

async fn delete_user(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.clone();
    let target = load_user(&st, id).await?;
    if target.id == admin.id {
        return Err(AppError::bad_request("you can't delete your own account"));
    }
    ensure_other_admin(&st, &target).await?;
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(&st.auth.db)
        .await
        .map_err(db_err)?;
    tracing::info!("{} deleted user {}", admin.username, target.username);
    audit(
        &st,
        &admin.username,
        "user.delete",
        &target.username,
        ip,
        true,
    )
    .await;
    Ok(Json(json!({ "status": "ok" })))
}
