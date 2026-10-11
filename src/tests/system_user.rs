//! The warmer's token belongs to a system account that can't sign in, isn't
//! listed or managed as a user, and can only download.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{Harness, Opts};

#[tokio::test]
async fn warmer_token_belongs_to_a_download_only_system_account() {
    let h = Harness::with(Opts {
        client_auth: true,
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    let token = h.state.auth.warmer_token().await.unwrap();

    // Owned by @warmer, not by the admin.
    let (owner, system): (String, bool) = sqlx::query_as(
        "SELECT u.username, u.system FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.name = 'warmer'",
    )
    .fetch_one(&h.state.auth.db)
    .await
    .unwrap();
    assert_eq!((owner.as_str(), system), (crate::auth::WARMER_USER, true));

    // It downloads, and does nothing else.
    let (status, _, _) = h
        .request("GET", "/cargo/config.json", Some(format!("Bearer {token}")))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .request("GET", "/metrics", Some(format!("Bearer {token}")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = h
        .request("GET", "/api/templates", Some(format!("Bearer {token}")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // No sign-in, whatever the password.
    for password in ["!", "", "a long test password"] {
        let (status, _, _) = h
            .send(
                "POST",
                "/api/auth/login",
                None,
                Some(json!({"username": crate::auth::WARMER_USER, "password": password})),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{password:?}");
    }

    // Not a user anyone can see or manage.
    let users = h.api(&cookie, "/api/users").await;
    let names: Vec<&str> = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["admin"]);
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE system = 1")
        .fetch_one(&h.state.auth.db)
        .await
        .unwrap();
    for (method, path, body) in [
        (
            "PATCH",
            format!("/api/users/{id}"),
            Some(json!({"role": "admin"})),
        ),
        ("POST", format!("/api/users/{id}/reset-password"), None),
        ("DELETE", format!("/api/users/{id}"), None),
    ] {
        let (status, _, _) = h.send(method, &path, Some(&cookie), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let (status, _, _) = h
        .send(
            "POST",
            "/api/users",
            Some(&cookie),
            Some(json!({"username": crate::auth::WARMER_USER, "role": "admin"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Still listed among tokens, as a system token.
    let tokens = h.api(&cookie, "/api/tokens?all=true").await;
    let t: &Value = tokens["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "warmer")
        .unwrap();
    assert_eq!(t["system"], true);
}

#[tokio::test]
async fn old_warmer_tokens_move_to_the_system_account() {
    let h = Harness::new(true, Duration::from_secs(300)).await;
    let db = &h.state.auth.db;
    // As earlier versions did: a system token owned by the first admin.
    let (admin,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(db)
        .await
        .unwrap();
    let (token_id, token) = h
        .state
        .auth
        .create_token(admin, "warmer", None)
        .await
        .unwrap();
    sqlx::query("UPDATE tokens SET system = 1 WHERE id = ?")
        .bind(token_id)
        .execute(db)
        .await
        .unwrap();

    // The next start moves it.
    let reopened = crate::auth::open_db(&h.data).await.unwrap();
    let (owner,): (String,) = sqlx::query_as(
        "SELECT u.username FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(token_id)
    .fetch_one(&reopened)
    .await
    .unwrap();
    assert_eq!(owner, crate::auth::WARMER_USER);
    // And it keeps working.
    let (status, _, _) = h
        .request("GET", "/cargo/config.json", Some(format!("Bearer {token}")))
        .await;
    assert_eq!(status, StatusCode::OK);
}
